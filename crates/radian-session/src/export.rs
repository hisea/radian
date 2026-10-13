//! JSON and Markdown export of a session (ADR 0013 T4 / decision 3).
//!
//! Two opt-in switches, both per export and both off by default:
//! [`ExportOptions::include_provider_payload`] (raw provider payloads, ADR 0013 decision 3)
//! and [`ExportOptions::include_reasoning`] (a reasoning model's thinking, ADR 0014: it is
//! model-internal, can be long, and can echo whatever the user pasted). **Redaction has no
//! switch.** Known
//! secrets (API keys, custom sensitive header values) plus `sk-`/`Bearer`/`Authorization`
//! patterns are always stripped, whether or not the payload is included.
//!
//! Export reads the session and writes an output string; it never touches the session file.

use std::path::Path;

use agent_core::{ContentPart, Message};
use serde_json::{Map, Value, json};

use crate::reader::{EventData, Session};

/// Why an export failed.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("serializing the session: {0}")]
    Json(#[from] serde_json::Error),
    #[error("writing the export: {0}")]
    Io(#[from] std::io::Error),
}

/// Options for one export.
///
/// `include_provider_payload` (ADR 0013 decision 3) and `include_reasoning` (ADR 0014) both
/// default to `false`. `secrets` is a list of always-redacted strings; there is deliberately
/// no "disable redaction" flag, and opting in to either kind of content never turns it off.
#[derive(Debug, Clone, Default)]
pub struct ExportOptions {
    include_provider_payload: bool,
    include_reasoning: bool,
    secrets: Vec<String>,
}

impl ExportOptions {
    pub fn new() -> Self {
        Self::default()
    }

    /// Include raw provider payloads in this export. Opt-in, per export.
    pub fn include_provider_payload(mut self, yes: bool) -> Self {
        self.include_provider_payload = yes;
        self
    }

    /// Include assistant reasoning in this export. Opt-in, per export. JSON and Markdown
    /// honour it the same way: without it, neither format carries reasoning.
    pub fn include_reasoning(mut self, yes: bool) -> Self {
        self.include_reasoning = yes;
        self
    }

    pub fn includes_reasoning(&self) -> bool {
        self.include_reasoning
    }

    /// Always redact this known secret (an API key, an `Authorization` value, a custom
    /// header value). Additive only: nothing turns redaction off.
    pub fn with_secret(mut self, secret: impl Into<String>) -> Self {
        self.secrets.push(secret.into());
        self
    }

    pub fn includes_provider_payload(&self) -> bool {
        self.include_provider_payload
    }

    pub fn secrets(&self) -> &[String] {
        &self.secrets
    }
}

/// The `seq` of every `MessageAppended` that is part of the committed history (ADR 0019).
/// Messages inside an uncommitted replacement segment are not in the current history, so the
/// export treats them as absent.
fn committed_message_seqs(session: &Session) -> std::collections::HashSet<u64> {
    let mut committed = std::collections::HashSet::new();
    let mut pending: Option<Vec<u64>> = None;
    for event in session.events() {
        match &event.data {
            EventData::MessageAppended { .. } => match &mut pending {
                Some(pending) => pending.push(event.seq),
                None => {
                    committed.insert(event.seq);
                }
            },
            EventData::TranscriptReplaced { .. } => pending = Some(Vec::new()),
            EventData::TranscriptReplacementCommitted { .. } => {
                if let Some(pending) = pending.take() {
                    committed.extend(pending);
                }
            }
            EventData::TranscriptReplacementAborted { .. } => pending = None,
            _ => {}
        }
    }
    committed
}

/// The JSON export: session metadata plus every accepted event, payload-stripped and
/// redacted.
pub fn export_json(session: &Session, opts: &ExportOptions) -> Result<String, ExportError> {
    let committed = committed_message_seqs(session);
    let mut events = Vec::with_capacity(session.events().len());
    for event in session.events() {
        if let EventData::MessageAppended { .. } = &event.data
            && !committed.contains(&event.seq)
        {
            continue;
        }
        let mut data = event_data_value(event, opts)?;
        redact_value(&mut data, opts.secrets());
        events.push(json!({
            "seq": event.seq,
            "ts": event.ts,
            "event": event.event,
            "critical": event.critical,
            "data": data,
        }));
    }
    let doc = json!({
        "session_id": session.session_id(),
        "schema_version": session.schema_version(),
        "workspace": workspace_of(session),
        "include_provider_payload": opts.includes_provider_payload(),
        "events": events,
        "skipped": session.skipped().iter().map(|(seq, name)| json!({"seq": seq, "event": name})).collect::<Vec<_>>(),
    });
    let mut text = serde_json::to_string_pretty(&doc)?;
    text.push('\n');
    Ok(text)
}

/// The Markdown export: a readable transcript of the messages, payload-stripped and
/// redacted.
pub fn export_markdown(session: &Session, opts: &ExportOptions) -> Result<String, ExportError> {
    let mut out = String::new();
    out.push_str(&format!("# Session {}\n\n", session.session_id()));
    if let Some(workspace) = workspace_of(session) {
        out.push_str(&format!("- workspace: `{workspace}`\n"));
    }
    out.push_str(&format!("- schema_version: {}\n", session.schema_version()));
    out.push_str(&format!(
        "- provider payloads: {}\n",
        if opts.includes_provider_payload() {
            "included"
        } else {
            "omitted"
        }
    ));
    out.push_str(&format!(
        "- events: {} (skipped {})\n\n",
        session.events().len(),
        session.skipped().len()
    ));

    let committed = committed_message_seqs(session);
    // ADR 0018: only a committed compaction is shown; a candidate with no commit is invisible.
    let committed_revisions: std::collections::HashSet<u64> = session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::TranscriptReplacementCommitted { revision, .. } => Some(*revision),
            _ => None,
        })
        .collect();
    for event in session.events() {
        match &event.data {
            EventData::ContextCompacted {
                revision,
                tokens_before,
                tokens_after,
                ..
            } if committed_revisions.contains(revision) => {
                out.push_str(&format!(
                    "_context compacted ({tokens_before} → {tokens_after} tokens)_\n\n"
                ));
            }
            EventData::MessageAppended { message, .. } => {
                if !committed.contains(&event.seq) {
                    continue;
                }
                let role = match message {
                    Message::User { .. } => "user",
                    Message::Assistant { .. } => "assistant",
                    Message::Tool { .. } => "tool",
                };
                out.push_str(&format!("## {role} (seq {})\n\n", event.seq));
                out.push_str(&render_message(message, opts));
                out.push('\n');
            }
            // ADR 0021 D6: the injected set is audit, not transcript. The Markdown export
            // shows only its size; the ids/revisions/scopes are in the JSON export.
            EventData::MemoryInjected { records } => {
                out.push_str(&format!(
                    "- memory injected: {} record(s) (seq {})\n",
                    records.len(),
                    event.seq
                ));
            }
            _ => {}
        }
    }

    // Redaction is a whole-document pass as well, so a secret in a field this renderer did
    // not special-case still cannot reach the file.
    Ok(redact_text(&out, opts.secrets()))
}

/// Writes a JSON export to `path` (created/truncated). The session file is not touched.
pub fn export_json_to(
    session: &Session,
    opts: &ExportOptions,
    path: impl AsRef<Path>,
) -> Result<(), ExportError> {
    std::fs::write(path, export_json(session, opts)?)?;
    Ok(())
}

/// Writes a Markdown export to `path` (created/truncated). The session file is not touched.
pub fn export_markdown_to(
    session: &Session,
    opts: &ExportOptions,
    path: impl AsRef<Path>,
) -> Result<(), ExportError> {
    std::fs::write(path, export_markdown(session, opts)?)?;
    Ok(())
}

fn workspace_of(session: &Session) -> Option<String> {
    session.events().iter().find_map(|event| {
        if event.event == "SessionCreated"
            && let EventData::Other(data) = &event.data
        {
            return data
                .get("workspace")
                .and_then(|w| w.as_str())
                .map(str::to_owned);
        }
        None
    })
}

fn event_data_value(
    event: &crate::reader::SessionEvent,
    opts: &ExportOptions,
) -> Result<Value, ExportError> {
    let value = match &event.data {
        EventData::MessageAppended { message, .. } => {
            let mut value = serde_json::to_value(message)?;
            if let Some(assistant) = value.get_mut("assistant").and_then(Value::as_object_mut) {
                // Dropping these is the whole point of the opt-in flags.
                if !opts.includes_provider_payload() {
                    assistant.remove("provider_payload");
                }
                if !opts.includes_reasoning() {
                    assistant.remove("reasoning");
                }
            }
            value
        }
        EventData::TranscriptReplaced { revision, len } => {
            json!({"revision": revision, "len": len})
        }
        EventData::TranscriptReplacementCommitted { revision, len } => {
            json!({"revision": revision, "len": len})
        }
        EventData::TranscriptReplacementAborted { revision } => {
            json!({"revision": revision})
        }
        EventData::ToolExecutionStarted {
            call_id,
            name,
            arguments,
        } => json!({"call_id": call_id, "name": name, "arguments": arguments}),
        EventData::ToolExecutionSettled { call_id, result } => {
            json!({"call_id": call_id, "result": serde_json::to_value(result)?})
        }
        EventData::ModelUsageRecorded { usage } => json!({"usage": serde_json::to_value(usage)?}),
        EventData::CompactionUsageRecorded {
            revision,
            model,
            usage,
        } => json!({
            "revision": revision,
            "model": model,
            "usage": serde_json::to_value(usage)?,
        }),
        EventData::ContextCompacted {
            revision,
            source_revision,
            source_len,
            summary,
            first_kept,
            tokens_before,
            tokens_after,
            below_target,
            model,
        } => json!({
            "revision": revision,
            "source_revision": source_revision,
            "source_len": source_len,
            "summary": summary,
            "first_kept": first_kept,
            "tokens_before": tokens_before,
            "tokens_after": tokens_after,
            "below_target": below_target,
            "model": model,
        }),
        EventData::InboxEnqueued {
            message,
            message_id,
            lane,
            source,
        } => json!({
            "message": serde_json::to_value(message)?,
            "message_id": message_id,
            "lane": lane,
            "source": source,
        }),
        EventData::InputClaimed {
            message_ids,
            turn_id,
        } => json!({"message_ids": message_ids, "turn_id": turn_id}),
        EventData::InputAdmitted {
            message_id,
            transcript_index,
        } => json!({"message_id": message_id, "transcript_index": transcript_index}),
        EventData::TurnStarted { turn_id } => json!({"turn_id": turn_id}),
        EventData::TurnFinished {
            turn_id,
            outcome,
            reason,
            stop_reason,
        } => {
            let mut value = json!({"turn_id": turn_id, "outcome": outcome});
            if let Some(reason) = reason {
                value["reason"] = serde_json::to_value(reason)?;
            }
            if let Some(stop_reason) = stop_reason {
                value["stop_reason"] = json!(stop_reason);
            }
            value
        }
        EventData::TurnFailed { turn_id, message } => {
            json!({"turn_id": turn_id, "message": message})
        }
        EventData::ConfigChanged { revision, change } => {
            json!({"revision": revision, "change": change})
        }
        EventData::ContextBudgetReached {
            turn_id,
            source,
            projected_tokens,
            window_tokens,
            threshold_percent,
            baseline_input_tokens,
            estimated_growth_tokens,
        } => json!({
            "turn_id": turn_id,
            "source": source,
            "projected_tokens": projected_tokens,
            "window_tokens": window_tokens,
            "threshold_percent": threshold_percent,
            "baseline_input_tokens": baseline_input_tokens,
            "estimated_growth_tokens": estimated_growth_tokens,
        }),
        EventData::PermissionResolved {
            request_id,
            call_id,
            decision,
            approver,
            reason,
        } => json!({
            "request_id": request_id,
            "call_id": call_id,
            "decision": decision,
            "approver": approver,
            "reason": reason,
        }),
        EventData::MemoryInjected { records } => json!({"records": records}),
        EventData::Other(value) => value.clone(),
    };
    Ok(value)
}

fn render_message(message: &Message, opts: &ExportOptions) -> String {
    match message {
        Message::User { content, .. } => {
            format!("{}\n", redact_text(&plain_text(content), opts.secrets()))
        }
        Message::Assistant {
            content,
            tool_calls,
            provider_payload,
            reasoning,
            ..
        } => {
            let mut out = String::new();
            if opts.includes_reasoning() && !reasoning.is_empty() {
                // Shown apart from the answer and marked, never folded into it (ADR 0014).
                out.push_str("<details><summary>reasoning</summary>\n\n");
                out.push_str(&redact_text(&plain_text(reasoning), opts.secrets()));
                out.push_str("\n\n</details>\n\n");
            }
            out.push_str(&redact_text(&plain_text(content), opts.secrets()));
            if !tool_calls.is_empty() {
                out.push_str("\n\nTool calls:\n");
                for call in tool_calls {
                    out.push_str(&format!(
                        "- `{}` `{}` {}\n",
                        call.name, call.id, call.arguments
                    ));
                }
            }
            if opts.includes_provider_payload()
                && let Some(payload) = provider_payload
            {
                out.push_str("\n\n<details><summary>provider_payload</summary>\n\n```json\n");
                out.push_str(&redact_text(payload.get(), opts.secrets()));
                out.push_str("\n```\n</details>\n");
            }
            out
        }
        Message::Tool { result, .. } => {
            let body = if result.is_error {
                format!(
                    "error ({}): {}",
                    result.error_code.as_deref().unwrap_or("?"),
                    result.text()
                )
            } else {
                result.text()
            };
            format!("{}\n", redact_text(&body, opts.secrets()))
        }
    }
}

fn plain_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .map(|part| match part {
            ContentPart::Text { text, .. } => text.clone(),
            ContentPart::Image { mime_type, .. } => format!("[{mime_type} image omitted]"),
            ContentPart::Audio { mime_type, .. } => format!("[{mime_type} audio omitted]"),
            ContentPart::ResourceLink { uri, .. } => format!("[resource: {uri}]"),
            ContentPart::EmbeddedResource { resource, .. } => resource
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("[embedded resource]")
                .to_owned(),
            ContentPart::Opaque { .. } => "[unsupported content omitted]".into(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// JSON keys whose string values are always replaced. Case-insensitive.
const SENSITIVE_KEYS: [&str; 12] = [
    "api_key",
    "apikey",
    "authorization",
    "x-api-key",
    "cookie",
    "set-cookie",
    "secret",
    "token",
    "access_token",
    "refresh_token",
    "password",
    "client_secret",
];

/// Recursively redacts a JSON value: known secrets and `sk-`/`Bearer`/`Authorization`
/// patterns in every string, and the values of [`SENSITIVE_KEYS`] (or every value under a
/// `headers` object) replaced outright.
pub fn redact_value(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(s) => *s = redact_text(s, secrets),
        Value::Array(items) => {
            for item in items {
                redact_value(item, secrets);
            }
        }
        Value::Object(map) => redact_object(map, secrets),
        _ => {}
    }
}

fn redact_object(map: &mut Map<String, Value>, secrets: &[String]) {
    for (key, value) in map.iter_mut() {
        let lower = key.to_ascii_lowercase();
        if SENSITIVE_KEYS.contains(&lower.as_str()) {
            *value = Value::String("<redacted>".into());
            continue;
        }
        if lower == "headers" {
            redact_headers(value, secrets);
            continue;
        }
        redact_value(value, secrets);
    }
}

/// Every header value is treated as sensitive: we cannot know which are secrets. Config
/// snapshots carry headers as `[(name, value)]`; other callers may use an object.
fn redact_headers(value: &mut Value, secrets: &[String]) {
    match value {
        Value::Object(headers) => {
            for header_value in headers.values_mut() {
                *header_value = Value::String("<redacted>".into());
            }
        }
        Value::Array(pairs) => {
            for pair in pairs {
                if let Value::Array(kv) = pair
                    && kv.len() == 2
                {
                    kv[1] = Value::String("<redacted>".into());
                }
            }
        }
        other => redact_value(other, secrets),
    }
}

/// Redacts a plain string: known secrets first, then the secret-looking patterns.
pub fn redact_text(input: &str, secrets: &[String]) -> String {
    let mut text = input.to_owned();
    for secret in secrets {
        if !secret.is_empty() {
            text = text.replace(secret.as_str(), "<redacted>");
        }
    }
    let text = redact_prefixed_token(&text, "bearer ");
    redact_sk_tokens(&text)
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')
}

/// Replaces the token after `<prefix>` (case-insensitive) with `<redacted>`.
fn redact_prefixed_token(input: &str, prefix: &str) -> String {
    let bytes = input.as_bytes();
    let lower = input.to_ascii_lowercase();
    let plen = prefix.len();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if lower[i..].starts_with(prefix) {
            let start = i + plen;
            let mut end = start;
            while end < bytes.len() && is_token_char(bytes[end]) {
                end += 1;
            }
            out.push_str(&input[i..start]);
            if end > start {
                out.push_str("<redacted>");
            }
            i = end;
        } else {
            // Copy one UTF-8 char.
            let ch_len = utf8_len(bytes[i]);
            out.push_str(&input[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

/// Replaces every `sk-<token>` occurrence with `<redacted>`.
fn redact_sk_tokens(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"sk-") {
            let mut end = i + 3;
            while end < bytes.len() && is_token_char(bytes[end]) {
                end += 1;
            }
            if end > i + 3 {
                out.push_str("<redacted>");
                i = end;
                continue;
            }
        }
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&input[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}
