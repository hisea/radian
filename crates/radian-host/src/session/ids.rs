//! The session's identity and commands (ADR 0023 U5): [`SessionId`], what a facade
//! can do ([`SessionCapability`]), what the UI asks ([`SessionCommand`]), and why a
//! command was refused ([`CommandRefused`]).
//!
//! Split from `session/mod.rs` by pure move (ADR 0043 decision 5): no behaviour change.

use std::fmt;

use agent_core::Message;

/// A session's stable identifier: the session file's name and `SessionCreated.session_id`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a facade can do (ADR 0023 U5). The UI must not infer capability from the concrete
/// type; it reads this list, and the facade re-checks it when a command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCapability {
    /// Read the projection.
    Observe,
    /// Start a new turn.
    SendMessage,
    /// Redirect the running turn.
    Steer,
    /// Cancel the running turn.
    Cancel,
    /// Answer a pending approval or question.
    AnswerRequest,
}

impl SessionCapability {
    /// Every capability the in-process session serves; what a normal host declares.
    pub const ALL: [SessionCapability; 5] = [
        SessionCapability::Observe,
        SessionCapability::SendMessage,
        SessionCapability::Steer,
        SessionCapability::Cancel,
        SessionCapability::AnswerRequest,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Observe => "observe",
            Self::SendMessage => "send",
            Self::Steer => "steer",
            Self::Cancel => "cancel",
            Self::AnswerRequest => "answer",
        }
    }
}

/// A command the UI asks a session to perform.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionCommand {
    /// Start a new turn: `Inbox::followup`. A [`Message`], not a string: a send can carry an
    /// attached file as its own part (ADR 0018).
    SendFollowup(Message),
    /// Redirect the running turn: `Inbox::steer`.
    Steer(Message),
    /// Cancel the running turn.
    Cancel,
    /// Enqueue a message that does **not** wake an idle driver (ADR 0033's `!` output, ADR
    /// 0011's `inject`): it joins the next turn's context without starting one.
    Inject(Message),
}

/// One `!`/`!!` command's audit record (ADR 0033): the durable `UserBash` journal entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserBashRecord {
    pub command: String,
    pub exclude_from_context: bool,
    pub exit_code: Option<i32>,
    pub cancelled: bool,
}

impl SessionCommand {
    /// The capability a session must declare to accept this command.
    pub fn capability(&self) -> SessionCapability {
        match self {
            Self::SendFollowup(_) | Self::Inject(_) => SessionCapability::SendMessage,
            Self::Steer(_) => SessionCapability::Steer,
            Self::Cancel => SessionCapability::Cancel,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::SendFollowup(_) => "send",
            Self::Inject(_) => "inject",
            Self::Steer(_) => "steer",
            Self::Cancel => "cancel",
        }
    }
}

/// A command was refused. `undeclared` is the capability check; any other reason is the inbox
/// rejecting the input (closed, full, not a user message).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{command}` was refused: {reason}")]
pub struct CommandRefused {
    pub command: String,
    /// `Some` when the facade does not declare the capability; `None` when the inbox rejected
    /// the input (closed, full, not a user message).
    pub capability: Option<String>,
    pub reason: String,
}

impl CommandRefused {
    /// The facade does not declare the capability the command needs.
    pub fn undeclared(command: &SessionCommand) -> Self {
        let capability = command.capability();
        Self {
            command: command.name().to_string(),
            capability: Some(capability.as_str().to_string()),
            reason: format!(
                "this session does not declare the `{}` capability",
                capability.as_str()
            ),
        }
    }

    /// The capability check passed; the inbox refused the input.
    pub fn inbox(command: &str, error: &radian_runtime::InboxError) -> Self {
        Self {
            command: command.to_string(),
            capability: None,
            reason: error.to_string(),
        }
    }
}
