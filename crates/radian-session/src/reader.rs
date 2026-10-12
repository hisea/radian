//! The read side: streaming parse with the ADR 0006 §7 unknown-content policy, and
//! transcript folding through `agent_core`'s own `replay`/`repair`/`recover`.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use agent_core::{JournalRecord, Message, ModelUsage, ToolResult, recover};
use serde::Deserialize;
use serde_json::Value;

use crate::error::ReadError;
use crate::writer::SCHEMA_VERSION;

/// Refuse lines larger than this instead of growing the buffer without bound (ADR 0006
/// §3.1 suggests a configurable cap; this slice uses a fixed 64 MiB default).
const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// The v1 events known to a schema-v1 reader (ADR 0006 §4). Anything else is unknown and
/// handled by the `critical` policy.
const KNOWN_EVENTS: [&str; 16] = [
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
    Other(Value),
}

/// One accepted envelope line.
#[derive(Debug, Clone)]
pub struct SessionEvent {
    pub seq: u64,
    pub event: String,
    pub critical: bool,
    pub data: EventData,
}

/// A fully parsed session file. Unknown non-critical events are not part of `events`; they
/// are listed in [`Session::skipped`].
#[derive(Debug, Clone)]
pub struct Session {
    path: PathBuf,
    session_id: String,
    schema_version: u64,
    events: Vec<SessionEvent>,
    skipped: Vec<(u64, String)>,
}

impl Session {
    /// Opens and fully validates a session file per ADR 0006 §7. The file is streamed
    /// line by line; it is never assumed to fit in memory.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ReadError> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).map_err(|source| ReadError::Open {
            path: path.clone(),
            source,
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
        let mut events = Vec::new();
        let mut skipped = Vec::new();

        loop {
            line.clear();
            let n = {
                // Bounded read: never grow `line` past the cap + 1.
                let mut limited = reader.by_ref().take((MAX_LINE_BYTES + 1) as u64);
                limited
                    .read_until(b'\n', &mut line)
                    .map_err(|source| ReadError::Open {
                        path: path.clone(),
                        source,
                    })?
            };
            if n == 0 {
                break;
            }
            if !line.ends_with(b"\n") {
                if line.len() > MAX_LINE_BYTES {
                    return Err(ReadError::Corrupt {
                        path,
                        line: lineno + 1,
                        column: MAX_LINE_BYTES,
                        message: format!("line exceeds the {MAX_LINE_BYTES}-byte cap"),
                    });
                }
                // No LF means no commit: the trailing bytes are a torn tail (ADR 0006
                // §3.2) and are dropped without being parsed or counted.
                break;
            }
            lineno += 1;
            let raw =
                std::str::from_utf8(&line[..line.len() - 1]).map_err(|e| ReadError::Corrupt {
                    path: path.clone(),
                    line: lineno,
                    column: e.valid_up_to(),
                    message: "line is not valid UTF-8".into(),
                })?;
            let header: Header = serde_json::from_str(raw).map_err(|e| ReadError::Corrupt {
                path: path.clone(),
                line: lineno,
                column: e.column(),
                message: e.to_string(),
            })?;

            match schema_version {
                None => {
                    if header.schema_version > SCHEMA_VERSION {
                        return Err(ReadError::SchemaTooNew {
                            path,
                            found: header.schema_version,
                            supported: SCHEMA_VERSION,
                        });
                    }
                    if header.schema_version != SCHEMA_VERSION {
                        return Err(ReadError::UnsupportedSchema {
                            path,
                            found: header.schema_version,
                        });
                    }
                    schema_version = Some(header.schema_version);
                    session_id = header.session_id.clone();
                }
                Some(version) => {
                    if header.schema_version != version || header.session_id != session_id {
                        return Err(ReadError::Envelope {
                            path,
                            line: lineno,
                            message: "schema_version or session_id changed mid-file".into(),
                        });
                    }
                }
            }
            if header.seq != expect_seq {
                return Err(ReadError::BadSeq {
                    path,
                    line: lineno,
                    expected: expect_seq,
                    found: header.seq,
                });
            }
            expect_seq += 1;
            if header.event == "SessionCreated" && header.seq != 0 {
                return Err(ReadError::SessionCreatedNotFirst { path, line: lineno });
            }

            if !KNOWN_EVENTS.contains(&header.event.as_str()) {
                if header.critical {
                    return Err(ReadError::UnknownCritical {
                        path,
                        line: lineno,
                        event: header.event,
                    });
                }
                skipped.push((header.seq, header.event));
                continue;
            }

            let seq = header.seq;
            let data = match header.event.as_str() {
                "MessageAppended" => {
                    let d: MessageData = data_of(raw, lineno, &path)?;
                    if d.index != index_next {
                        return Err(ReadError::BadIndex {
                            path,
                            line: lineno,
                            expected: index_next,
                            found: d.index,
                        });
                    }
                    index_next += 1;
                    messages_since += 1;
                    EventData::MessageAppended {
                        index: d.index,
                        message: d.message,
                    }
                }
                "TranscriptReplaced" => {
                    let d: ReplacedData = data_of(raw, lineno, &path)?;
                    if let Some(len) = pending_len
                        && messages_since != len
                    {
                        return Err(ReadError::BadReplacementLen {
                            path,
                            len,
                            count: messages_since,
                        });
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
                    let d: StartedData = data_of(raw, lineno, &path)?;
                    EventData::ToolExecutionStarted {
                        call_id: d.call_id,
                        name: d.name,
                        arguments: d.arguments,
                    }
                }
                "ToolExecutionSettled" => {
                    let d: SettledData = data_of(raw, lineno, &path)?;
                    EventData::ToolExecutionSettled {
                        call_id: d.call_id,
                        result: d.result,
                    }
                }
                "ModelUsageRecorded" => {
                    let d: UsageData = data_of(raw, lineno, &path)?;
                    EventData::ModelUsageRecorded { usage: d.usage }
                }
                _ => EventData::Other(data_value(raw, lineno, &path)?),
            };
            events.push(SessionEvent {
                seq,
                event: header.event,
                critical: header.critical,
                data,
            });
        }

        if let Some(len) = pending_len
            && messages_since != len
        {
            return Err(ReadError::BadReplacementLen {
                path,
                len,
                count: messages_since,
            });
        }

        Ok(Session {
            path,
            session_id,
            schema_version: schema_version.unwrap_or(SCHEMA_VERSION),
            events,
            skipped,
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
                EventData::Other(_) => None,
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
    #[allow(dead_code)]
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
