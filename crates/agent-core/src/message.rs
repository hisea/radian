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
}

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Message {
    User {
        content: Vec<ContentPart>,
    },
    Assistant {
        content: Vec<ContentPart>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_payload: Option<OpaqueJson>,
    },
    Tool {
        result: ToolResult,
    },
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self::User {
            content: vec![ContentPart::text(text)],
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant {
            content: vec![ContentPart::text(text)],
            tool_calls: Vec::new(),
            provider_payload: None,
        }
    }

    pub fn is_user(&self) -> bool {
        matches!(self, Self::User { .. })
    }

    /// Concatenated text parts, for display and tests.
    pub fn text(&self) -> String {
        match self {
            Self::User { content } | Self::Assistant { content, .. } => joined_text(content),
            Self::Tool { result } => result.text(),
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
