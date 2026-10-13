use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

/// JSON kept exactly as received. The core never parses, re-serializes or rewrites it.
///
/// Used for provider data that must be sent back verbatim, such as signed reasoning blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OpaqueJson(Box<RawValue>);

impl OpaqueJson {
    pub fn from_raw(raw: Box<RawValue>) -> Self {
        Self(raw)
    }

    /// Validates `json` and stores it without normalizing whitespace or key order.
    pub fn parse(json: impl Into<String>) -> Result<Self, serde_json::Error> {
        RawValue::from_string(json.into()).map(Self)
    }

    pub fn get(&self) -> &str {
        self.0.get()
    }

    pub fn into_raw(self) -> Box<RawValue> {
        self.0
    }
}

impl PartialEq for OpaqueJson {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

/// One block of message content. Covers the MCP content kinds; anything else is kept opaque.
///
/// Externally tagged on purpose: serde buffers internally tagged enums, and a buffered
/// `RawValue` cannot be deserialized, which would break `Opaque` round trips.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
        /// Protocol-level `_meta`, kept as received.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    Image {
        mime_type: String,
        /// Base64-encoded bytes.
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
        /// Protocol-level `_meta`, kept as received.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    Audio {
        mime_type: String,
        /// Base64-encoded bytes.
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
        /// Protocol-level `_meta`, kept as received.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    ResourceLink {
        uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
        /// Protocol-level `_meta`, kept as received.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    EmbeddedResource {
        resource: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        annotations: Option<Value>,
        /// Protocol-level `_meta`, kept as received.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        meta: Option<Value>,
    },
    /// A block the core does not understand, such as a provider's server-side tool block.
    Opaque { raw: OpaqueJson },
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            annotations: None,
            meta: None,
        }
    }

    /// A text part that is an **attached file** at `path` (ADR 0018). The host
    /// writes this; Core only reads the annotation, to keep the user's words verbatim across
    /// compaction without carrying the file's body.
    pub fn attachment(text: impl Into<String>, path: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            annotations: Some(serde_json::json!({
                "kind": ATTACHMENT_KIND,
                "path": path.into(),
            })),
            meta: None,
        }
    }

    /// The attached file's path when this part carries the [`ATTACHMENT_KIND`] annotation.
    pub fn attachment_path(&self) -> Option<&str> {
        let Self::Text {
            annotations: Some(value),
            ..
        } = self
        else {
            return None;
        };
        if value.get("kind").and_then(Value::as_str) != Some(ATTACHMENT_KIND) {
            return None;
        }
        value.get("path").and_then(Value::as_str)
    }
}

/// The `annotations.kind` a host sets on a text part that is an attached file (ADR 0018
/// revision 6). Core reads it when it keeps the user's words verbatim; it never writes it.
pub const ATTACHMENT_KIND: &str = "attachment";

/// A tool call requested by the model, to be executed by the host.
///
/// Tools the provider runs on its own side are never represented as `ToolCall`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    /// The model-facing tool name.
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: Vec<ContentPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    pub is_error: bool,
    /// Machine-readable reason for runtime-generated errors, e.g. `unknown_tool`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

impl ToolResult {
    pub fn success(call_id: impl Into<String>, content: Vec<ContentPart>) -> Self {
        Self {
            call_id: call_id.into(),
            content,
            structured: None,
            is_error: false,
            error_code: None,
        }
    }

    pub fn error(call_id: impl Into<String>, code: &str, message: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            content: vec![ContentPart::text(message)],
            structured: None,
            is_error: true,
            error_code: Some(code.to_owned()),
        }
    }

    /// Concatenated text parts, for display and tests.
    pub fn text(&self) -> String {
        joined_text(&self.content)
    }
}

fn joined_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// A host-owned opaque identifier for a message. The core carries it, never interprets it
/// and never generates one.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageId(pub String);

/// Who produced a message, as the host sees it. The core makes no permission decision on
/// this basis, and deliberately provides no default: `None` means "the host did not say",
/// which is not the same as [`InputSource::User`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InputSource {
    User,
    SlashCommand,
    PermissionDecision,
    Scheduler,
    Mcp,
    Subagent,
    System,
    /// A value this version does not know: a newer writer's source. A v1 reader maps it
    /// here instead of refusing the line (ADR 0006 §7(d)); the raw JSONL keeps the original
    /// string, but this in-memory value does not.
    #[serde(other)]
    Unknown,
}

/// Host-owned metadata attached to any message. Invisible to model adapters; carried by
/// the transcript, the journal and the events unchanged.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<InputSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<OpaqueJson>,
}

impl MessageMeta {
    /// True when no field is set; such a meta is not serialized at all.
    pub fn is_empty(&self) -> bool {
        self.id.is_none() && self.source.is_none() && self.origin.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Message {
    User {
        content: Vec<ContentPart>,
        #[serde(default, skip_serializing_if = "MessageMeta::is_empty")]
        meta: MessageMeta,
    },
    Assistant {
        content: Vec<ContentPart>,
        /// The provider's separate reasoning channel for this message, if any. **Provider
        /// invisible** (the same rule as `meta`, ADR 0005 G4): adapters must not send it
        /// back. Kept as `ContentPart`s so a future non-text reasoning block (a signature,
        /// a structured reason) does not need a new shape; empty for providers that have no
        /// reasoning channel.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reasoning: Vec<ContentPart>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_payload: Option<OpaqueJson>,
        /// True only for the prefix Core commits when a model call is cancelled mid-stream
        /// (A3): the text is what the core had emitted, never a full response.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        interrupted: bool,
        #[serde(default, skip_serializing_if = "MessageMeta::is_empty")]
        meta: MessageMeta,
    },
    Tool {
        result: ToolResult,
        #[serde(default, skip_serializing_if = "MessageMeta::is_empty")]
        meta: MessageMeta,
    },
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![ContentPart::text(text)],
            meta: MessageMeta::default(),
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant {
            content: vec![ContentPart::text(text)],
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
            provider_payload: None,
            interrupted: false,
            meta: MessageMeta::default(),
        }
    }

    pub fn is_user(&self) -> bool {
        matches!(self, Self::User { .. })
    }

    /// Concatenated text parts, for display and tests.
    pub fn text(&self) -> String {
        match self {
            Self::User { content, .. } | Self::Assistant { content, .. } => joined_text(content),
            Self::Tool { result, .. } => result.text(),
        }
    }
}

impl From<&str> for Message {
    fn from(text: &str) -> Self {
        Self::user(text)
    }
}

impl From<String> for Message {
    fn from(text: String) -> Self {
        Self::user(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interrupted_assistant(text: &str) -> Message {
        Message::Assistant {
            content: vec![ContentPart::text(text)],
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
            provider_payload: None,
            interrupted: true,
            meta: MessageMeta::default(),
        }
    }

    #[test]
    fn interrupted_is_absent_when_false_and_present_when_true() {
        let plain = Message::assistant("hello");
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("interrupted"), "{json}");
        let json = serde_json::to_string(&interrupted_assistant("half")).unwrap();
        assert!(json.contains("\"interrupted\":true"), "{json}");
    }

    #[test]
    fn old_json_without_the_field_reads_as_false() {
        // Externally tagged parts: {"text": {"text": ...}}.
        let json = r#"{"assistant":{"content":[{"text":{"text":"hello"}}]}}"#;
        let message: Message = serde_json::from_str(json).unwrap();
        assert_eq!(message, Message::assistant("hello"));
    }

    #[test]
    fn interrupted_assistants_round_trip() {
        let message = interrupted_assistant("你😀好");
        let round: Message =
            serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
        assert_eq!(round, message);
    }

    #[test]
    fn reasoning_is_absent_when_empty_and_present_when_set() {
        let plain = Message::assistant("hello");
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("reasoning"), "{json}");

        let with_reasoning = Message::Assistant {
            content: vec![ContentPart::text("hello")],
            reasoning: vec![ContentPart::text("because")],
            tool_calls: Vec::new(),
            provider_payload: None,
            interrupted: false,
            meta: MessageMeta::default(),
        };
        let json = serde_json::to_string(&with_reasoning).unwrap();
        assert!(json.contains("\"reasoning\""), "{json}");
    }

    #[test]
    fn old_json_without_reasoning_reads_as_empty() {
        let json = r#"{"assistant":{"content":[{"text":{"text":"hello"}}]}}"#;
        let message: Message = serde_json::from_str(json).unwrap();
        assert_eq!(message, Message::assistant("hello"));
        let Message::Assistant { reasoning, .. } = &message else {
            unreachable!()
        };
        assert!(reasoning.is_empty());
    }

    #[test]
    fn reasoning_round_trips() {
        let message = Message::Assistant {
            content: vec![ContentPart::text("answer")],
            reasoning: vec![ContentPart::text("思考 😀")],
            tool_calls: Vec::new(),
            provider_payload: None,
            interrupted: true,
            meta: MessageMeta::default(),
        };
        let round: Message =
            serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
        assert_eq!(round, message);
    }

    #[test]
    fn input_sources_round_trip_as_snake_case_and_map_unknown_values() {
        let cases = [
            (InputSource::User, "user"),
            (InputSource::SlashCommand, "slash_command"),
            (InputSource::PermissionDecision, "permission_decision"),
            (InputSource::Scheduler, "scheduler"),
            (InputSource::Mcp, "mcp"),
            (InputSource::Subagent, "subagent"),
            (InputSource::System, "system"),
        ];
        for (source, wire) in cases {
            assert_eq!(
                serde_json::to_string(&source).unwrap(),
                format!("\"{wire}\"")
            );
            assert_eq!(
                serde_json::from_str::<InputSource>(&format!("\"{wire}\"")).unwrap(),
                source
            );
        }
        // A value from a future writer is read as `Unknown`, never a parse error (ADR 0006
        // §7(d)); refusing it would make an old reader unable to open a newer session.
        assert_eq!(
            serde_json::from_str::<InputSource>("\"future_source\"").unwrap(),
            InputSource::Unknown
        );
    }

    #[test]
    fn message_meta_omits_empty_fields_and_reads_older_messages() {
        let empty = Message::user("hello");
        let encoded = serde_json::to_value(&empty).unwrap();
        assert!(encoded["user"].get("meta").is_none(), "{encoded}");

        let old: Message =
            serde_json::from_str(r#"{"user":{"content":[{"text":{"text":"hello"}}]}}"#).unwrap();
        assert_eq!(old, empty);

        let partial = Message::User {
            content: vec![ContentPart::text("hello")],
            meta: MessageMeta {
                id: None,
                source: Some(InputSource::Subagent),
                origin: None,
            },
        };
        let encoded = serde_json::to_value(&partial).unwrap();
        assert_eq!(
            encoded["user"]["meta"],
            serde_json::json!({"source": "subagent"})
        );
        assert_eq!(serde_json::from_value::<Message>(encoded).unwrap(), partial);
    }
}
