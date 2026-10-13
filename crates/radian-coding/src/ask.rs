//! `ask_user`: the model asks the human a structured question and waits (ADR 0024).
//!
//! The tool itself knows nothing about a UI. The host injects a [`Prompter`] through
//! [`AskTools`]; a host without one does not mount the provider at all, so a model never asks
//! into the void. The wait is raced against [`agent_core::ToolContext::cancellation`] and
//! **never** fabricates an answer.
//!
//! `ask_user` is its **own** provider, not a member of [`crate::CodingTools`]: a host with no
//! `--workspace` has no coding tools, but a model can still ask a question (ADR 0024 Q5).
//!
//! The question and the answer are durable without a new record: the question is the call's
//! `arguments` and the answer its final result, both already journaled (ADR 0002 E1).

use std::collections::HashSet;
use std::sync::Arc;

use agent_core::{
    ContentPart, ToolCall, ToolConcurrency, ToolContext, ToolError, ToolFailure, ToolOutput,
    ToolProvider, ToolSession, ToolSpec,
};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::fail;

/// At most this many questions per request (ADR 0024 Q2).
pub const MAX_QUESTIONS: usize = 8;
/// At most this many options per question.
pub const MAX_OPTIONS: usize = 12;
/// Longest `prompt`, in characters.
pub const MAX_PROMPT_CHARS: usize = 500;
/// Longest option `label`, in characters.
pub const MAX_LABEL_CHARS: usize = 80;
/// Longest option `description`, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 200;
/// Longest id (a question's or an option's), in characters.
pub const MAX_ID_CHARS: usize = 32;

/// The three question shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// Exactly one option (plus `Other` when `allow_other`).
    Single,
    /// Zero or more options (plus `Other` when `allow_other`).
    Multi,
    /// Free text; carries no `options`.
    Text,
}

/// One selectable answer to a [`QuestionKind::Single`]/[`QuestionKind::Multi`] question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One question the model asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub prompt: String,
    pub kind: QuestionKind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<QuestionOption>,
    /// `single`/`multi`: let the user type a free-text `Other` answer.
    #[serde(default)]
    pub allow_other: bool,
    /// The user may not submit without answering this one.
    #[serde(default)]
    pub required: bool,
}

/// The validated questions a [`Prompter`] is asked. Built from the tool call's arguments with
/// [`QuestionRequest::parse`], which enforces every ADR 0024 Q2 limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionRequest {
    pub questions: Vec<Question>,
}

impl QuestionRequest {
    pub fn new(questions: Vec<Question>) -> Self {
        Self { questions }
    }

    /// Parses the tool arguments and validates every limit. Every failure is
    /// `invalid_arguments`: the limits are never applied by truncation.
    pub fn parse(arguments: &Value) -> Result<Self, ToolFailure> {
        let args: AskArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| fail("invalid_arguments", e.to_string()))?;
        let request = Self {
            questions: args.questions,
        };
        request.validate()?;
        Ok(request)
    }

    /// The Q2 limits. `Err` is always `invalid_arguments`.
    pub fn validate(&self) -> Result<(), ToolFailure> {
        if self.questions.is_empty() {
            return Err(fail("invalid_arguments", "`questions` must not be empty"));
        }
        if self.questions.len() > MAX_QUESTIONS {
            return Err(fail(
                "invalid_arguments",
                format!(
                    "{} questions exceeds the limit of {MAX_QUESTIONS}",
                    self.questions.len()
                ),
            ));
        }
        let mut ids: HashSet<&str> = HashSet::with_capacity(self.questions.len());
        for question in &self.questions {
            check_id(&question.id, "question id")?;
            if !ids.insert(question.id.as_str()) {
                return Err(fail(
                    "invalid_arguments",
                    format!("question id `{}` is used more than once", question.id),
                ));
            }
            check_chars(&question.prompt, MAX_PROMPT_CHARS, "prompt")?;
            if question.kind == QuestionKind::Text {
                if !question.options.is_empty() {
                    return Err(fail(
                        "invalid_arguments",
                        format!(
                            "question `{}` is `text` and must not carry options",
                            question.id
                        ),
                    ));
                }
                continue;
            }
            if question.options.len() > MAX_OPTIONS {
                return Err(fail(
                    "invalid_arguments",
                    format!(
                        "question `{}` has {} options, over the limit of {MAX_OPTIONS}",
                        question.id,
                        question.options.len()
                    ),
                ));
            }
            let mut option_ids: HashSet<&str> = HashSet::with_capacity(question.options.len());
            for option in &question.options {
                check_id(&option.id, "option id")?;
                if !option_ids.insert(option.id.as_str()) {
                    return Err(fail(
                        "invalid_arguments",
                        format!(
                            "option id `{}` is used more than once in question `{}`",
                            option.id, question.id
                        ),
                    ));
                }
                check_chars(&option.label, MAX_LABEL_CHARS, "label")?;
                if let Some(description) = &option.description {
                    check_chars(description, MAX_DESCRIPTION_CHARS, "description")?;
                }
            }
        }
        Ok(())
    }
}

/// The `questions` wrapper the tool arguments deserialize into.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskArgs {
    questions: Vec<Question>,
}

fn check_id(id: &str, what: &str) -> Result<(), ToolFailure> {
    let count = id.chars().count();
    let valid = (1..=MAX_ID_CHARS).contains(&count)
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(fail(
            "invalid_arguments",
            format!("{what} `{id}` must match [a-z0-9_-]{{1,{MAX_ID_CHARS}}}"),
        ))
    }
}

fn check_chars(text: &str, max: usize, what: &str) -> Result<(), ToolFailure> {
    let count = text.chars().count();
    if count <= max {
        Ok(())
    } else {
        Err(fail(
            "invalid_arguments",
            format!("{what} is {count} characters, over the limit of {max}"),
        ))
    }
}

/// One question's answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionReply {
    /// The question id this answers.
    pub id: String,
    /// The selected option ids, for `single`/`multi`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected: Vec<String>,
    /// The free text, for `text` (or a `single`/`multi` answered only in words).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The free text the user typed in `Other`, when `allow_other` was set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<String>,
}

/// The host's answer to a [`QuestionRequest`]. `request_id` is the host's process-unique id
/// (ADR 0024 Q2/Q3), echoed into the tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionAnswer {
    pub request_id: String,
    /// One reply per question, in any order; the ids must match the request exactly.
    pub answers: Vec<QuestionReply>,
}

impl QuestionAnswer {
    pub fn new(request_id: impl Into<String>, answers: Vec<QuestionReply>) -> Self {
        Self {
            request_id: request_id.into(),
            answers,
        }
    }
}

/// Why an ask produced no answers. Each variant becomes one tool-failure code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromptError {
    /// The user explicitly declined (Esc).
    #[error("the user declined to answer")]
    Declined,
    /// The prompter's channel is gone: the UI exited, or the session closed while the
    /// question was still pending.
    #[error("the prompter is gone")]
    Gone,
    /// The prompter itself failed.
    #[error("{0}")]
    Failed(String),
}

/// One host-implemented question channel (TUI modal, remote). The future must return when the
/// answer arrives; the tool races it against the turn's cancellation, so a wait that never
/// ends is interrupted, not hung. The same shape as
/// [`radian_permissions::Approver`](../radian_permissions/trait.Approver.html).
pub trait Prompter: Send + Sync {
    fn ask(&self, request: QuestionRequest) -> BoxFuture<'_, Result<QuestionAnswer, PromptError>>;
}

/// The model-facing spec. Only advertised when a [`Prompter`] is injected.
pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "ask_user".into(),
        description: "Ask the user one or more structured questions and wait for the answers. \
                      Use it when a decision needs the human: one choice, several choices, or \
                      free text. The turn pauses until the user answers or declines; the user \
                      may also cancel the turn. Only available when an interactive user is \
                      present."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_QUESTIONS,
                    "description": "The questions to ask, shown in this order.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {
                                "type": "string",
                                "pattern": "^[a-z0-9_-]{1,32}$",
                                "description": "Answer key for this question; unique in the request."
                            },
                            "prompt": {
                                "type": "string",
                                "maxLength": MAX_PROMPT_CHARS,
                                "description": "The question as the user should read it."
                            },
                            "kind": {
                                "type": "string",
                                "enum": ["single", "multi", "text"],
                                "description": "One choice, several choices, or free text."
                            },
                            "options": {
                                "type": "array",
                                "maxItems": MAX_OPTIONS,
                                "description": "The choices, for `single`/`multi`. Not allowed for `text`.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": { "type": "string", "pattern": "^[a-z0-9_-]{1,32}$" },
                                        "label": { "type": "string", "maxLength": MAX_LABEL_CHARS },
                                        "description": { "type": "string", "maxLength": MAX_DESCRIPTION_CHARS }
                                    },
                                    "required": ["id", "label"],
                                    "additionalProperties": false
                                }
                            },
                            "allow_other": {
                                "type": "boolean",
                                "description": "`single`/`multi`: also offer a free-text `Other` answer."
                            },
                            "required": {
                                "type": "boolean",
                                "description": "The user may not submit without answering this one."
                            }
                        },
                        "required": ["id", "prompt", "kind"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["questions"],
            "additionalProperties": false
        }),
        concurrency: ToolConcurrency::Exclusive,
    }
}

/// Asks the prompter and turns its outcome into the tool result (ADR 0024 Q3/Q4).
pub(crate) async fn run(
    prompter: &Arc<dyn Prompter>,
    arguments: &Value,
    cx: &ToolContext<'_>,
) -> Result<ToolOutput, ToolFailure> {
    let request = QuestionRequest::parse(arguments)?;
    let answer = tokio::select! {
        biased;
        _ = cx.cancellation.cancelled() => {
            // The turn was cancelled while waiting. Never build an answer: the model must not
            // read a fabricated one as the user's reply.
            return Err(fail(
                "cancelled",
                "the turn was cancelled before the user answered",
            ));
        }
        result = prompter.ask(request.clone()) => match result {
            Ok(answer) => answer,
            Err(PromptError::Declined) => {
                return Err(fail("user_declined", "the user declined to answer"));
            }
            Err(PromptError::Gone) => {
                return Err(fail(
                    "prompter_gone",
                    "the question could not be delivered: the prompter is gone",
                ));
            }
            Err(PromptError::Failed(message)) => {
                return Err(fail("prompter_error", message));
            }
        },
    };
    validate_answer(&request, &answer)?;
    Ok(output(&request, &answer))
}

/// The answer's ids must correspond one-to-one with the questions (ADR 0024 Q3): a missing
/// question, a duplicate, or an unknown id is a broken prompter, not a user choice.
fn validate_answer(request: &QuestionRequest, answer: &QuestionAnswer) -> Result<(), ToolFailure> {
    let mut seen: HashSet<&str> = HashSet::with_capacity(answer.answers.len());
    for reply in &answer.answers {
        if !seen.insert(reply.id.as_str()) {
            return Err(fail(
                "invalid_answer",
                format!("more than one answer for question `{}`", reply.id),
            ));
        }
        if !request.questions.iter().any(|q| q.id == reply.id) {
            return Err(fail(
                "invalid_answer",
                format!("answer for unknown question `{}`", reply.id),
            ));
        }
    }
    for question in &request.questions {
        if !seen.contains(question.id.as_str()) {
            return Err(fail(
                "invalid_answer",
                format!("no answer for question `{}`", question.id),
            ));
        }
    }
    Ok(())
}

/// The Q3 result: the answers as structured JSON, plus a human-readable line per question.
fn output(request: &QuestionRequest, answer: &QuestionAnswer) -> ToolOutput {
    let lines: Vec<String> = request
        .questions
        .iter()
        .map(|question| {
            let reply = answer.answers.iter().find(|r| r.id == question.id);
            format!("{} → {}", question.prompt, render_reply(question, reply))
        })
        .collect();
    ToolOutput {
        content: vec![ContentPart::text(lines.join("\n"))],
        structured: Some(json!({
            "request_id": answer.request_id,
            "answers": answer.answers,
        })),
    }
}

/// One answer as readable text, using option labels rather than ids.
fn render_reply(question: &Question, reply: Option<&QuestionReply>) -> String {
    let Some(reply) = reply else {
        return "(no answer)".into();
    };
    let mut parts: Vec<String> = reply
        .selected
        .iter()
        .map(|id| {
            question
                .options
                .iter()
                .find(|option| &option.id == id)
                .map(|option| option.label.clone())
                .unwrap_or_else(|| id.clone())
        })
        .collect();
    if let Some(text) = &reply.text
        && !text.is_empty()
    {
        parts.push(text.clone());
    }
    if let Some(other) = &reply.other
        && !other.is_empty()
    {
        parts.push(format!("other: {other}"));
    }
    if parts.is_empty() {
        "(no answer)".into()
    } else {
        parts.join(", ")
    }
}

/// `ask_user` on its own (ADR 0024 Q1/Q5): a host mounts this when a human is there to answer,
/// whether or not it also mounts the coding tools.
pub struct AskTools {
    prompter: Arc<dyn Prompter>,
}

impl AskTools {
    pub fn new(prompter: Arc<dyn Prompter>) -> Self {
        Self { prompter }
    }
}

/// One session's view of `ask_user`. Holds only the prompter and the spec.
pub struct AskSession {
    prompter: Arc<dyn Prompter>,
    specs: Vec<ToolSpec>,
}

impl ToolProvider for AskTools {
    type Session = AskSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        Ok(AskSession {
            prompter: Arc::clone(&self.prompter),
            specs: vec![spec()],
        })
    }
}

impl ToolSession for AskSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        call: &ToolCall,
        cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        match call.name.as_str() {
            "ask_user" => run(&self.prompter, &call.arguments, &cx).await,
            other => Err(fail(
                "unknown_tool",
                format!("`{other}` is not one of the question tools"),
            )),
        }
    }
}
