//! The read side: streaming parse with the ADR 0006 §7 unknown-content policy, and
//! transcript folding through `agent_core`'s own `replay`/`repair`/`recover`.
//!
//! [`scan_stream`] is the single parser. [`Session::open`] uses it to retain every event;
//! the index cache (ADR 0006 §10) uses the same function to fold rows without retaining the
//! session — so both agree on the LF-commit / torn-tail rule by construction.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use agent_core::{
    InputSource, JournalRecord, Message, MessageId, ModelUsage, ToolResult, UsageSource, recover,
};
use serde::Deserialize;
use serde_json::Value;

use crate::error::ReadError;
use crate::runtime::{InboxLane, PermissionApprover, PermissionDecision, TurnOutcome, TurnReason};
use crate::writer::SCHEMA_VERSION;

/// Refuse lines larger than this instead of growing the buffer without bound (ADR 0006
/// §3.1 suggests a configurable cap; this slice uses a fixed 64 MiB default).
pub(crate) const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// The v1 events known to a schema-v1 reader (ADR 0006 §4). Anything else is unknown and
/// handled by the `critical` policy.
pub(crate) const KNOWN_EVENTS: [&str; 17] = [
    "SessionCreated",
    "InboxEnqueued",
    "InputClaimed",
    "InputAdmitted",
    "TurnStarted",
    "MessageAppended",
    "ToolExecutionStarted",
    "ToolExecutionSettled",
    "TranscriptReplaced",
    "TurnFinished",
    "ConfigChanged",
    "ContextCompacted",
    "Rewound",
    "ContextBudgetReached",
    "PermissionResolved",
    "ModelUsageRecorded",
    "SessionForked",
];

/// The parsed body of one known event. Events this slice does not interpret (for example
/// `SessionCreated`) keep their raw `data` under [`EventData::Other`].
#[derive(Debug, Clone)]
pub enum EventData {
    MessageAppended {
        index: u64,
        message: Message,
    },
    TranscriptReplaced {
        revision: u64,
        len: u64,
    },
    ToolExecutionStarted {
        call_id: String,
        name: String,
        arguments: Value,
    },
    ToolExecutionSettled {
        call_id: String,
        result: ToolResult,
    },
    ModelUsageRecorded {
        usage: ModelUsage,
    },
    // Runtime events (ADR 0006 §5.5–§5.7). They are typed so a round trip is exact, and
    // they are deliberately **not** mapped to `JournalRecord`: they are not transcript
    // records, and `replay`/`repair` must keep ignoring them (ADR 0006 §12).
    InboxEnqueued {
        message: Message,
        message_id: MessageId,
        lane: InboxLane,
        source: InputSource,
    },
    InputClaimed {
        message_ids: Vec<MessageId>,
        turn_id: String,
    },
    InputAdmitted {
        message_id: MessageId,
        transcript_index: u64,
    },
    TurnStarted {
        turn_id: String,
    },
    TurnFinished {
        turn_id: String,
        outcome: TurnOutcome,
        reason: Option<TurnReason>,
        stop_reason: Option<String>,
    },
    ConfigChanged {
        revision: u64,
        change: Value,
    },
    /// A context-budget stop (ADR 0015 C6). Typed for an exact round trip; audit only,
    /// never a transcript record.
    ContextBudgetReached {
        turn_id: String,
        source: UsageSource,
        projected_tokens: u64,
        window_tokens: u64,
        threshold_percent: u8,
        baseline_input_tokens: Option<u64>,
        estimated_growth_tokens: Option<u64>,
    },
    /// One permission decision (ADR 0006 §5.9). Typed for an exact round trip; audit only,
    /// never a transcript record.
    PermissionResolved {
        request_id: String,
        call_id: String,
        decision: PermissionDecision,
        approver: PermissionApprover,
        reason: Option<String>,
    },
    /// A known event this crate does not interpret (`SessionCreated`) or an unknown
    /// non-critical event.
    Other(Value),
}

/// One accepted envelope line, with the fields an index can derive from the file alone
/// (ADR 0006 §10): the envelope plus the line's byte offset and length.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    pub seq: u64,
    pub ts: String,
    pub event: String,
    pub critical: bool,
    /// Byte offset of the line's first byte in the file.
    pub byte_offset: u64,
    /// Length in bytes of the line, including its terminating LF (the commit marker).
    pub line_len: u64,
    pub data: EventData,
}

/// What a full scan learned about the file. `committed_bytes` is the size of the committed
/// prefix: everything after the last LF is a torn tail and is not counted (ADR 0006 §3.2).
#[derive(Debug, Clone)]
pub(crate) struct ScanSummary {
    pub(crate) session_id: String,
    pub(crate) schema_version: u64,
    pub(crate) committed_bytes: u64,
    pub(crate) event_count: u64,
    pub(crate) skipped: Vec<(u64, String)>,
}

/// Streams a session file, applying the ADR 0006 §7 policy and the §3.2 torn-tail rule,
/// and calls `on_event` for every accepted event in order. Never loads the whole file (or
/// the whole session) into memory; the per-line buffer is bounded by
/// [`MAX_LINE_BYTES`].
pub(crate) fn scan_stream<E, F>(path: &Path, mut on_event: F) -> Result<ScanSummary, E>
where
    E: From<ReadError>,
    F: FnMut(SessionEvent) -> Result<(), E>,
{
    let file = File::open(path).map_err(|source| {
        E::from(ReadError::Open {
            path: path.to_path_buf(),
            source,
        })
    })?;
    let mut reader = BufReader::new(file);
    let mut line: Vec<u8> = Vec::new();
    let mut lineno: u64 = 0;
    let mut expect_seq: u64 = 0;
    let mut index_next: u64 = 0;
    let mut pending_len: Option<u64> = None;
    let mut messages_since: u64 = 0;
    let mut schema_version: Option<u64> = None;
    let mut session_id = String::new();
    let mut skipped = Vec::new();
    let mut event_count: u64 = 0;
    // Byte offset where the current line starts, and the end of the last committed line.
    let mut line_offset: u64 = 0;
    let mut committed_bytes: u64 = 0;

    loop {
        line.clear();
        let n = {
            // Bounded read: never grow `line` past the cap + 1.
            let mut limited = reader.by_ref().take((MAX_LINE_BYTES + 1) as u64);
            limited.read_until(b'\n', &mut line).map_err(|source| {
                E::from(ReadError::Open {
                    path: path.to_path_buf(),
                    source,
                })
            })?
        };
        if n == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            if line.len() > MAX_LINE_BYTES {
                return Err(E::from(ReadError::Corrupt {
                    path: path.to_path_buf(),
                    line: lineno + 1,
                    column: MAX_LINE_BYTES,
                    message: format!("line exceeds the {MAX_LINE_BYTES}-byte cap"),
                }));
            }
            // No LF means no commit: the trailing bytes are a torn tail (ADR 0006 §3.2)
            // and are dropped without being parsed or counted. `committed_bytes` keeps the
            // end of the previous complete line.
            break;
        }
        lineno += 1;
        let raw = std::str::from_utf8(&line[..line.len() - 1]).map_err(|e| {
            E::from(ReadError::Corrupt {
                path: path.to_path_buf(),
                line: lineno,
                column: e.valid_up_to(),
                message: "line is not valid UTF-8".into(),
            })
        })?;
        let header: Header = serde_json::from_str(raw).map_err(|e| {
            E::from(ReadError::Corrupt {
                path: path.to_path_buf(),
                line: lineno,
                column: e.column(),
                message: e.to_string(),
            })
        })?;

        match schema_version {
            None => {
                if header.schema_version > SCHEMA_VERSION {
                    return Err(E::from(ReadError::SchemaTooNew {
                        path: path.to_path_buf(),
                        found: header.schema_version,
                        supported: SCHEMA_VERSION,
                    }));
                }
                if header.schema_version != SCHEMA_VERSION {
                    return Err(E::from(ReadError::UnsupportedSchema {
                        path: path.to_path_buf(),
                        found: header.schema_version,
                    }));
                }
                schema_version = Some(header.schema_version);
                session_id = header.session_id.clone();
            }
            Some(version) => {
                if header.schema_version != version || header.session_id != session_id {
                    return Err(E::from(ReadError::Envelope {
                        path: path.to_path_buf(),
                        line: lineno,
                        message: "schema_version or session_id changed mid-file".into(),
                    }));
                }
            }
        }
        if header.seq != expect_seq {
            return Err(E::from(ReadError::BadSeq {
                path: path.to_path_buf(),
                line: lineno,
                expected: expect_seq,
                found: header.seq,
            }));
        }
        expect_seq += 1;
        if header.event == "SessionCreated" && header.seq != 0 {
            return Err(E::from(ReadError::SessionCreatedNotFirst {
                path: path.to_path_buf(),
                line: lineno,
            }));
        }

        let seq = header.seq;
        let ts = header.ts;
        let event = header.event;
        let critical = header.critical;
        // The accepted/committed line ends here, whatever it turns out to be.
        committed_bytes = line_offset + n as u64;

        if !KNOWN_EVENTS.contains(&event.as_str()) {
            if critical {
                return Err(E::from(ReadError::UnknownCritical {
                    path: path.to_path_buf(),
                    line: lineno,
                    event,
                }));
            }
            skipped.push((seq, event));
            line_offset = committed_bytes;
            continue;
        }

        let data = match event.as_str() {
            "MessageAppended" => {
                let d: MessageData = data_of(raw, lineno, path)?;
                if d.index != index_next {
                    return Err(E::from(ReadError::BadIndex {
                        path: path.to_path_buf(),
                        line: lineno,
                        expected: index_next,
                        found: d.index,
                    }));
                }
                index_next += 1;
                messages_since += 1;
                EventData::MessageAppended {
                    index: d.index,
                    message: d.message,
                }
            }
            "TranscriptReplaced" => {
                let d: ReplacedData = data_of(raw, lineno, path)?;
                if let Some(len) = pending_len
                    && messages_since != len
                {
                    return Err(E::from(ReadError::BadReplacementLen {
                        path: path.to_path_buf(),
                        len,
                        count: messages_since,
                    }));
                }
                pending_len = Some(d.len);
                messages_since = 0;
                index_next = 0;
                EventData::TranscriptReplaced {
                    revision: d.revision,
                    len: d.len,
                }
            }
            "ToolExecutionStarted" => {
                let d: StartedData = data_of(raw, lineno, path)?;
                EventData::ToolExecutionStarted {
                    call_id: d.call_id,
                    name: d.name,
                    arguments: d.arguments,
                }
            }
            "ToolExecutionSettled" => {
                let d: SettledData = data_of(raw, lineno, path)?;
                EventData::ToolExecutionSettled {
                    call_id: d.call_id,
                    result: d.result,
                }
            }
            "ModelUsageRecorded" => {
                let d: UsageData = data_of(raw, lineno, path)?;
                EventData::ModelUsageRecorded { usage: d.usage }
            }
            "PermissionResolved" => {
                let d: PermissionResolvedData = data_of(raw, lineno, path)?;
                EventData::PermissionResolved {
                    request_id: d.request_id,
                    call_id: d.call_id,
                    decision: d.decision,
                    approver: d.approver,
                    reason: d.reason,
                }
            }
            "InboxEnqueued" => {
                let d: InboxEnqueuedData = data_of(raw, lineno, path)?;
                EventData::InboxEnqueued {
                    message: d.message,
                    message_id: d.message_id,
                    lane: d.lane,
                    source: d.source,
                }
            }
            "InputClaimed" => {
                let d: InputClaimedData = data_of(raw, lineno, path)?;
                EventData::InputClaimed {
                    message_ids: d.message_ids,
                    turn_id: d.turn_id,
                }
            }
            "InputAdmitted" => {
                let d: InputAdmittedData = data_of(raw, lineno, path)?;
                EventData::InputAdmitted {
                    message_id: d.message_id,
                    transcript_index: d.transcript_index,
                }
            }
            "TurnStarted" => {
                let d: TurnStartedData = data_of(raw, lineno, path)?;
                EventData::TurnStarted { turn_id: d.turn_id }
            }
            "TurnFinished" => {
                let d: TurnFinishedData = data_of(raw, lineno, path)?;
                EventData::TurnFinished {
                    turn_id: d.turn_id,
                    outcome: d.outcome,
                    reason: d.reason,
                    stop_reason: d.stop_reason,
                }
            }
            "ConfigChanged" => {
                let d: ConfigChangedData = data_of(raw, lineno, path)?;
                EventData::ConfigChanged {
                    revision: d.revision,
                    change: d.change,
                }
            }
            "ContextBudgetReached" => {
                let d: ContextBudgetReachedData = data_of(raw, lineno, path)?;
                EventData::ContextBudgetReached {
                    turn_id: d.turn_id,
                    source: d.source,
                    projected_tokens: d.projected_tokens,
                    window_tokens: d.window_tokens,
                    threshold_percent: d.threshold_percent,
                    baseline_input_tokens: d.baseline_input_tokens,
                    estimated_growth_tokens: d.estimated_growth_tokens,
                }
            }
            _ => {
                let value = data_value(raw, lineno, path).map_err(E::from)?;
                EventData::Other(value)
            }
        };

        on_event(SessionEvent {
            seq,
            ts,
            event,
            critical,
            byte_offset: line_offset,
            line_len: n as u64,
            data,
        })?;
        event_count += 1;
        line_offset = committed_bytes;
    }

    if let Some(len) = pending_len
        && messages_since != len
    {
        return Err(E::from(ReadError::BadReplacementLen {
            path: path.to_path_buf(),
            len,
            count: messages_since,
        }));
    }

    Ok(ScanSummary {
        session_id,
        schema_version: schema_version.unwrap_or(SCHEMA_VERSION),
        committed_bytes,
        event_count,
        skipped,
    })
}

/// A fully parsed session file. Unknown non-critical events are not part of `events`; they
/// are listed in [`Session::skipped`].
#[derive(Debug, Clone)]
pub struct Session {
    path: PathBuf,
    session_id: String,
    schema_version: u64,
    committed_bytes: u64,
    events: Vec<SessionEvent>,
    skipped: Vec<(u64, String)>,
}

impl Session {
    /// Opens and fully validates a session file per ADR 0006 §7.
    ///
    /// Lines are read one at a time with a bounded read, so a single oversized line is
    /// rejected rather than grown into memory. The parsed events are **retained**, so a
    /// `Session`'s memory grows with the length of the session: this is fine for Phase 1,
    /// where a session is opened to fold it. Code that only needs a summary (the index
    /// cache) uses [`scan_stream`] instead and does not retain.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ReadError> {
        let path = path.as_ref().to_path_buf();
        let mut events = Vec::new();
        let summary = scan_stream(&path, |event| -> Result<(), ReadError> {
            events.push(event);
            Ok(())
        })?;
        Ok(Session {
            path,
            session_id: summary.session_id,
            schema_version: summary.schema_version,
            committed_bytes: summary.committed_bytes,
            events,
            skipped: summary.skipped,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn schema_version(&self) -> u64 {
        self.schema_version
    }

    /// The size of the committed prefix: every byte through the last LF. Bytes after it are
    /// a torn tail (ADR 0006 §3.2) and are not part of the session.
    pub fn committed_bytes(&self) -> u64 {
        self.committed_bytes
    }

    pub fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    /// `(seq, event)` of every unknown non-critical event that was skipped (ADR 0006 §7b).
    pub fn skipped(&self) -> &[(u64, String)] {
        &self.skipped
    }

    /// Maps the parsed events back to core records. This is the only place that knows the
    /// disk-event shape; folding then reuses `agent_core::{replay, repair, recover}`.
    pub fn journal_records(&self) -> Vec<JournalRecord> {
        self.events
            .iter()
            .filter_map(|event| match &event.data {
                EventData::MessageAppended { index, message } => Some(JournalRecord::Message {
                    index: *index as usize,
                    message: message.clone(),
                }),
                EventData::TranscriptReplaced { revision, len } => {
                    Some(JournalRecord::TranscriptReplaced {
                        revision: *revision,
                        len: *len as usize,
                    })
                }
                EventData::ToolExecutionStarted {
                    call_id,
                    name,
                    arguments,
                } => Some(JournalRecord::ToolExecutionStarted {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
                EventData::ToolExecutionSettled { call_id, result } => {
                    Some(JournalRecord::ToolExecutionSettled {
                        call_id: call_id.clone(),
                        result: result.clone(),
                    })
                }
                EventData::ModelUsageRecorded { usage } => Some(JournalRecord::ModelUsage {
                    usage: usage.clone(),
                }),
                // Runtime events are not transcript records: `replay`/`repair`/`recover`
                // must never see them (ADR 0006 §12).
                EventData::InboxEnqueued { .. }
                | EventData::InputClaimed { .. }
                | EventData::InputAdmitted { .. }
                | EventData::TurnStarted { .. }
                | EventData::TurnFinished { .. }
                | EventData::ConfigChanged { .. }
                | EventData::ContextBudgetReached { .. }
                | EventData::PermissionResolved { .. }
                | EventData::Other(_) => None,
            })
            .collect()
    }

    /// The repaired transcript: `recover` over the mapped records, so unpaired tool calls
    /// get the ADR 0002 synthetic results. Empty file → empty history.
    pub fn history(&self) -> Result<Vec<Message>, ReadError> {
        Ok(recover(&self.journal_records())?)
    }
}

#[derive(Deserialize)]
struct Header {
    schema_version: u64,
    session_id: String,
    seq: u64,
    ts: String,
    event: String,
    critical: bool,
}

#[derive(Deserialize)]
struct Envelope<D> {
    #[allow(dead_code)]
    schema_version: u64,
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    seq: u64,
    #[allow(dead_code)]
    ts: String,
    #[allow(dead_code)]
    event: String,
    #[allow(dead_code)]
    critical: bool,
    data: D,
}

#[derive(Deserialize)]
struct MessageData {
    index: u64,
    message: Message,
}

#[derive(Deserialize)]
struct ReplacedData {
    revision: u64,
    len: u64,
}

#[derive(Deserialize)]
struct StartedData {
    call_id: String,
    name: String,
    arguments: Value,
}

#[derive(Deserialize)]
struct SettledData {
    call_id: String,
    result: ToolResult,
}

#[derive(Deserialize)]
struct UsageData {
    usage: ModelUsage,
}

#[derive(Deserialize)]
struct InboxEnqueuedData {
    message: Message,
    message_id: MessageId,
    lane: InboxLane,
    source: InputSource,
}

#[derive(Deserialize)]
struct InputClaimedData {
    message_ids: Vec<MessageId>,
    turn_id: String,
}

#[derive(Deserialize)]
struct InputAdmittedData {
    message_id: MessageId,
    transcript_index: u64,
}

#[derive(Deserialize)]
struct TurnStartedData {
    turn_id: String,
}

#[derive(Deserialize)]
struct TurnFinishedData {
    turn_id: String,
    outcome: TurnOutcome,
    #[serde(default)]
    reason: Option<TurnReason>,
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
struct ConfigChangedData {
    revision: u64,
    change: Value,
}

#[derive(Deserialize)]
struct ContextBudgetReachedData {
    turn_id: String,
    source: UsageSource,
    projected_tokens: u64,
    window_tokens: u64,
    threshold_percent: u8,
    #[serde(default)]
    baseline_input_tokens: Option<u64>,
    #[serde(default)]
    estimated_growth_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct PermissionResolvedData {
    request_id: String,
    call_id: String,
    decision: PermissionDecision,
    approver: PermissionApprover,
    #[serde(default)]
    reason: Option<String>,
}

/// Deserializes just the `data` field from the already-read line. Deserializing from the
/// raw text (not through a `Value`) is what keeps `OpaqueJson` byte fidelity.
fn data_of<D: for<'de> Deserialize<'de>>(
    raw: &str,
    lineno: u64,
    path: &Path,
) -> Result<D, ReadError> {
    let envelope: Envelope<D> = serde_json::from_str(raw).map_err(|e| ReadError::Corrupt {
        path: path.to_path_buf(),
        line: lineno,
        column: e.column(),
        message: e.to_string(),
    })?;
    Ok(envelope.data)
}

fn data_value(raw: &str, lineno: u64, path: &Path) -> Result<Value, ReadError> {
    data_of::<Value>(raw, lineno, path)
}
