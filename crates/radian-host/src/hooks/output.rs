//! The process result and the hook JSON output shapes.

use super::*;

/// One finished hook process.
pub(super) struct ProcOutput {
    pub(super) code: Option<i32>,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

/// One `PreToolUse` hook's verdict.
pub(super) enum PreVerdict {
    Continue,
    Updated(serde_json::Value),
    Deny(String),
}

/// One `PermissionRequest` hook's verdict (ADR 0045 §4).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PermissionVerdict {
    NoDecision,
    /// `session_grant` is the one supported `updatedPermissions` item: this call also
    /// records a session grant, like the approval prompt's "allow for this session".
    Allow {
        session_grant: bool,
    },
    Deny(Option<String>),
}

/// An exit-0 hook stdout, Claude's field names (kept verbatim, hence non-snake-case).
#[derive(Debug, Default, Deserialize)]
#[allow(non_snake_case)]
pub(super) struct HookOutput {
    #[serde(default)]
    pub(super) hookSpecificOutput: Option<HookSpecificOutput>,
    #[serde(default)]
    pub(super) reason: Option<String>,
    #[serde(default)]
    pub(super) decision: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[allow(non_snake_case)]
pub(super) struct HookSpecificOutput {
    #[serde(default)]
    pub(super) permissionDecision: Option<String>,
    #[serde(default)]
    pub(super) reason: Option<String>,
    #[serde(default)]
    pub(super) updatedInput: Option<serde_json::Value>,
    #[serde(default)]
    pub(super) additionalContext: Option<String>,
    /// `PermissionRequest` only: `decision { behavior, message, … }`. Every other event
    /// ignores the field.
    #[serde(default)]
    pub(super) decision: Option<PermissionDecisionOutput>,
    /// `Option<Option<_>>` with [`present`]: `None` when the field is absent, `Some(None)`
    /// for a present JSON `null`, `Some(Some(value))` otherwise. The other option fields
    /// here may treat `null` as absence; only this one must fail closed on every
    /// non-string value.
    #[serde(default, deserialize_with = "present")]
    pub(super) updatedToolOutput: Option<Option<serde_json::Value>>,
}

/// One `PermissionRequest` `hookSpecificOutput.decision` (ADR 0045 §4). `updatedPermissions`
/// is read only for the one item shape this version acts on; it is kept as
/// `Option<Option<_>>` (see [`present`]) so a present `null` warns once instead of reading
/// as an absent field.
#[derive(Debug, Default, Deserialize)]
#[allow(non_snake_case)]
pub(super) struct PermissionDecisionOutput {
    #[serde(default)]
    pub(super) behavior: Option<String>,
    #[serde(default)]
    pub(super) message: Option<String>,
    /// Parsed by [`CommandHooks::session_grant_requested`], so a malformed value warns
    /// instead of failing the whole decision closed.
    #[serde(default, deserialize_with = "present")]
    pub(super) updatedPermissions: Option<Option<serde_json::Value>>,
    #[serde(default, deserialize_with = "present")]
    pub(super) updatedInput: Option<Option<serde_json::Value>>,
    #[serde(default, deserialize_with = "present")]
    pub(super) interrupt: Option<Option<serde_json::Value>>,
}

/// Deserializes a possibly-`null` field while keeping "present" apart from "absent"
/// ([`HookSpecificOutput::updatedToolOutput`],
/// [`PermissionDecisionOutput::updatedPermissions`]): a plain `Option` maps a present JSON
/// `null` to `None`, so the field's own validation never runs. `T::deserialize` here is
/// `Option<Value>`, so `null` becomes `Some(None)` and any value `Some(Some(value))`.
pub(super) fn present<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    T::deserialize(deserializer).map(Some)
}
