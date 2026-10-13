//! The CLI's two ends of the permission layer: the terminal approver (or the `--yes`
//! auto-approver) and the durable audit sink.
//!
//! The layer itself (`radian-permissions`) owns the rules, the grants and the audit shape;
//! this module only answers "how does a human say yes" and "where does the record go".

use std::io::Write;
use std::sync::Arc;

use agent_core::InputSource;
use futures_util::future::BoxFuture;
use radian_permissions::{
    ApprovalError, ApprovalOutcome, ApprovalRequest, Approver, AuditSink, PermissionResolved,
};
use tokio::sync::mpsc;

use crate::LineHub;

/// Asks the user on stderr and reads the answer from the shared line hub.
///
/// `can_ask` is false in one-shot mode without a terminal: there is nobody to ask, so the
/// call is denied with a message that says how to allow it. Waiting here is interrupted by
/// the permission layer, which races `decide` against the turn's cancellation.
pub(crate) struct TerminalApprover {
    pub(crate) lines: LineHub,
    pub(crate) can_ask: bool,
}

impl Approver for TerminalApprover {
    fn decide(
        &self,
        request: ApprovalRequest,
    ) -> BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>> {
        Box::pin(async move {
            if !self.can_ask {
                return Ok(ApprovalOutcome::Deny {
                    reason: Some(
                        "not approved: this run has no terminal to ask in; \
                         pass --yes to allow tool calls"
                            .into(),
                    ),
                });
            }
            // The resource is the full command for `shell` and the path for the file tools;
            // a human must see exactly what would run.
            eprint!(
                "[approve] tool {}\n  {}\nrun it? [y/N] (a = always this session) ",
                request.tool, request.resource
            );
            let _ = std::io::stderr().flush();
            match self.lines.next().await.as_deref() {
                Some("y") | Some("Y") => Ok(ApprovalOutcome::AllowOnce),
                Some("a") | Some("A") | Some("always") => Ok(ApprovalOutcome::AllowForSession),
                Some(other) => Ok(ApprovalOutcome::Deny {
                    reason: Some(format!(
                        "the user denied this call (answer: {other:?}; `y` allows once, \
                         `a` allows for this session)"
                    )),
                }),
                None => Ok(ApprovalOutcome::Deny {
                    reason: Some("no input left to approve with; pass --yes".into()),
                }),
            }
        })
    }
}

/// `--yes`: answer every approval with "allow once" without asking.
///
/// This is *not* "allow everything": `deny` rules still deny, `allow` rules still allow, and
/// the sandbox is untouched. It only removes the question.
pub(crate) struct AutoApprover;

impl Approver for AutoApprover {
    fn decide(
        &self,
        _request: ApprovalRequest,
    ) -> BoxFuture<'_, Result<ApprovalOutcome, ApprovalError>> {
        Box::pin(async { Ok(ApprovalOutcome::AllowOnce) })
    }
}

/// Writes every decision into the session journal as a `PermissionResolved` (ADR 0006 §5.9).
///
/// The sink is synchronous but the journal append is not, so decisions go into a channel that
/// a small task drains. The caller flushes by dropping the senders and awaiting the task
/// before the process exits.
pub(crate) struct JournalAudit {
    tx: mpsc::UnboundedSender<radian_session::RuntimeEvent>,
}

impl JournalAudit {
    pub(crate) fn new(tx: mpsc::UnboundedSender<radian_session::RuntimeEvent>) -> Arc<Self> {
        Arc::new(Self { tx })
    }
}

impl AuditSink for JournalAudit {
    fn record(&self, event: &PermissionResolved) {
        // Best effort: the decision has already been made and returned to the core. A closed
        // channel means the session is going away; a journal failure surfaces on the turn's
        // own writes.
        let _ = self.tx.send(radian_permissions::runtime_event(event));
    }
}

/// The source a terminal-triggered turn runs with. Kept as one value so the permission layer
/// and the inbox enqueue cannot disagree.
pub(crate) const TERMINAL_SOURCE: InputSource = InputSource::User;
