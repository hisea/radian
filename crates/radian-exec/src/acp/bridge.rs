//! The pending-queue bridge (ADR 0049 decision 7): one queue entry becomes one
//! `session/request_permission`, and the client's answer completes the queue entry.
//!
//! The queue is the session's (ADR 0023 U7), so an approval and an `ask_user` question take
//! the same path and keep their arrival order. A `cancelled` answer denies with a reason and
//! cancels the turn; the tool result and the journal then follow the interrupted-turn rule
//! (ADR 0003).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use radian_coding::{
    PromptError, Question, QuestionAnswer, QuestionKind, QuestionReply, QuestionRequest,
};
use radian_host::{PendingRequest, PendingResponse, SessionFacade};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use super::protocol;
use super::wire::{self, PermissionOutcome};
use super::{RequestError, State};

/// Starts the queue bridge for an opened session.
pub(crate) fn spawn(
    state: Arc<State>,
    session: Arc<radian_host::LocalSession>,
    session_id: String,
    workspace: PathBuf,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let queue = session.queue();
        let mut changes = queue.subscribe();
        let mut handled: Option<u64> = None;
        loop {
            if let Some(view) = queue.head()
                && Some(view.request_id) != handled
            {
                let (response, cancelled) = match view.request {
                    PendingRequest::Approval(request) => {
                        handle_approval(&state, &session_id, &workspace, request).await
                    }
                    PendingRequest::Question(request) => {
                        handle_question(&state, &session_id, view.request_id, request).await
                    }
                };
                let _ = session.answer(view.request_id, response);
                handled = Some(view.request_id);
                if cancelled {
                    state.cancel_turn();
                }
                continue;
            }
            if changes.changed().await.is_err() {
                break;
            }
        }
    })
}

async fn handle_approval(
    state: &State,
    session_id: &str,
    workspace: &Path,
    request: radian_permissions::ApprovalRequest,
) -> (PendingResponse, bool) {
    let params = json!({
        "sessionId": session_id,
        "toolCall": protocol::permission_tool_call(
            &request.call_id,
            &format!("{}: {}", request.tool, request.resolved_resource),
            protocol::tool_kind(&request.tool),
            request.arguments.clone(),
            protocol::tool_locations(&request.arguments, workspace),
        ),
        "options": protocol::approval_options(),
    });
    let outcome = state.request("session/request_permission", params).await;
    match outcome {
        Ok(value) => match wire::permission_outcome(&value) {
            PermissionOutcome::Selected(option) if option == protocol::ALLOW_ALWAYS => (
                PendingResponse::Approval(radian_permissions::ApprovalOutcome::AllowForSession),
                false,
            ),
            PermissionOutcome::Selected(option) if option == protocol::ALLOW_ONCE => (
                PendingResponse::Approval(radian_permissions::ApprovalOutcome::AllowOnce),
                false,
            ),
            PermissionOutcome::Selected(_) => (
                PendingResponse::Approval(radian_permissions::ApprovalOutcome::Deny {
                    reason: Some("the user rejected this call".to_owned()),
                }),
                false,
            ),
            PermissionOutcome::Cancelled => (
                PendingResponse::Approval(radian_permissions::ApprovalOutcome::Deny {
                    reason: Some("the client cancelled the permission request".to_owned()),
                }),
                true,
            ),
            PermissionOutcome::Invalid(message) => (
                PendingResponse::Approval(radian_permissions::ApprovalOutcome::Deny {
                    reason: Some(format!("invalid permission response: {message}")),
                }),
                false,
            ),
        },
        Err(RequestError::Cancelled) => (
            PendingResponse::Approval(radian_permissions::ApprovalOutcome::Deny {
                reason: Some("the client cancelled the permission request".to_owned()),
            }),
            true,
        ),
        Err(RequestError::Rpc(error)) => (
            PendingResponse::Approval(radian_permissions::ApprovalOutcome::Deny {
                reason: Some(format!(
                    "the permission request failed: {}",
                    error.message()
                )),
            }),
            false,
        ),
    }
}

async fn handle_question(
    state: &State,
    session_id: &str,
    queue_id: u64,
    request: QuestionRequest,
) -> (PendingResponse, bool) {
    let mut answers = Vec::new();
    for question in &request.questions {
        // `request_permission` carries one selection and no free text; a text question has
        // nothing to offer, so it fails the tool instead of inventing an answer.
        if question.kind == QuestionKind::Text {
            return (
                PendingResponse::Question(Err(PromptError::Failed(
                    "a free-text question cannot be answered over ACP's request_permission"
                        .to_owned(),
                ))),
                false,
            );
        }
        let mut options = Vec::new();
        for option in &question.options {
            options.push(json!({
                "optionId": option_id(question, &option.id),
                "name": option.label,
                "kind": "allow_once",
            }));
        }
        let decline = decline_id(question);
        options.push(json!({
            "optionId": decline,
            "name": "Decline",
            "kind": "reject_once",
        }));
        let params = json!({
            "sessionId": session_id,
            "toolCall": protocol::permission_tool_call(
                &format!("ask-{queue_id}-{}", question.id),
                &question.prompt,
                "other",
                serde_json::to_value(question).unwrap_or(Value::Null),
                json!([]),
            ),
            "options": options,
        });
        match state.request("session/request_permission", params).await {
            Ok(value) => match wire::permission_outcome(&value) {
                PermissionOutcome::Selected(option) if option == decline => {
                    return (PendingResponse::Question(Err(PromptError::Declined)), false);
                }
                PermissionOutcome::Selected(option) => {
                    answers.push(QuestionReply {
                        id: question.id.clone(),
                        selected: vec![selected_option(question, &option)],
                        ..QuestionReply::default()
                    });
                }
                PermissionOutcome::Cancelled => {
                    return (PendingResponse::Question(Err(PromptError::Declined)), true);
                }
                PermissionOutcome::Invalid(message) => {
                    return (
                        PendingResponse::Question(Err(PromptError::Failed(message))),
                        false,
                    );
                }
            },
            Err(RequestError::Cancelled) => {
                return (PendingResponse::Question(Err(PromptError::Declined)), true);
            }
            Err(RequestError::Rpc(error)) => {
                return (
                    PendingResponse::Question(Err(PromptError::Failed(error.message()))),
                    false,
                );
            }
        }
    }
    (
        PendingResponse::Question(Ok(QuestionAnswer::new(format!("q-{queue_id}"), answers))),
        false,
    )
}

/// The wire id of one question option. The question id is the namespace, so two questions can
/// both offer `yes`.
fn option_id(question: &Question, option: &str) -> String {
    format!("{}:{option}", question.id)
}

fn decline_id(question: &Question) -> String {
    format!("{}:__decline", question.id)
}

/// The option id the client selected, without the question's namespace.
fn selected_option(question: &Question, selected: &str) -> String {
    selected
        .strip_prefix(&format!("{}:", question.id))
        .unwrap_or(selected)
        .to_owned()
}
