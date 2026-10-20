//! Phase 7: deterministic corruption replay.
//!
//! A real session file is written with the writer (all event kinds: transcript messages, a
//! tool call, usage, permission and runtime events), then mutated with a fixed-seed
//! xorshift generator. Every case asserts the same contract (ADR 0006 §3.2/§7/§10):
//!
//! - the read never panics;
//! - it either succeeds (and, where the mutation can only lose a suffix or add skippable
//!   content, the folded records are a prefix of the original), or fails with a definite
//!   error, or skips an unknown non-critical event and reports it;
//! - the JSONL bytes are identical before and after the read;
//! - the SQLite index, deleted and rebuilt from the JSONL, agrees with the read.
//!
//! Determinism: one seed maps to one mutation and one asserted outcome class. The base file
//! is written once per run; its `ts` values vary, but they are fixed-width, so byte offsets
//! and outcome classes do not.

mod common;

use std::panic::AssertUnwindSafe;
use std::path::Path;

use agent_core::{
    ContentPart, InputSource, JournalRecord, Message, MessageId, ModelUsage, ToolCall, ToolResult,
    TranscriptJournal, UsageSource,
};
use common::{TempDir, created};
use radian_session::{
    EventData, InboxLane, Index, IndexedEvent, PermissionApprover, PermissionDecision,
    RuntimeEvent, Session, SessionJournal, TurnOutcome,
};
use serde_json::json;

// ---------- deterministic generator ----------

/// xorshift64. A few lines instead of a dependency, and the same seed always yields the same
/// stream.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

// ---------- the base session ----------

struct Base {
    bytes: Vec<u8>,
    /// The file's lines without their LF.
    lines: Vec<Vec<u8>>,
    session: Session,
}

/// The `SessionCreated` payload used by the fuzz base.
fn base_created() -> radian_session::SessionCreatedData {
    created()
}

async fn build_base(dir: &TempDir) -> Base {
    let path = dir.file("base.jsonl");
    let journal = SessionJournal::create(&path, "fuzz-sid", base_created()).unwrap();
    let assistant = Message::Assistant {
        content: vec![ContentPart::text("thinking")],
        reasoning: Vec::new(),
        tool_calls: vec![ToolCall {
            id: "call-1".into(),
            name: "echo".into(),
            arguments: json!({"a": 1}),
        }],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    };
    let result = || ToolResult::success("call-1", vec![ContentPart::text("out-1")]);
    let messages: [(usize, Message); 3] = [
        (0, Message::user("first")),
        (1, assistant),
        (
            2,
            Message::Tool {
                result: result(),
                meta: Default::default(),
            },
        ),
    ];
    for (index, message) in messages {
        journal
            .append(JournalRecord::Message { index, message })
            .await
            .unwrap();
    }
    journal
        .append(JournalRecord::ToolExecutionStarted {
            call_id: "call-1".into(),
            name: "echo".into(),
            arguments: json!({"a": 1}),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::ToolExecutionSettled {
            call_id: "call-1".into(),
            result: result(),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::ModelUsage {
            usage: ModelUsage {
                input: Some(10),
                output: Some(5),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    for event in [
        RuntimeEvent::InboxEnqueued {
            message: Message::user("next"),
            message_id: MessageId("m-1".into()),
            lane: InboxLane::NextTurn,
            source: InputSource::User,
            wake: true,
            degraded: false,
        },
        RuntimeEvent::InputClaimed {
            message_ids: vec![MessageId("m-1".into())],
            turn_id: "t-1".into(),
        },
        RuntimeEvent::InputAdmitted {
            message_id: MessageId("m-1".into()),
            transcript_index: 3,
        },
        RuntimeEvent::TurnStarted {
            turn_id: "t-1".into(),
        },
        RuntimeEvent::TurnFinished {
            turn_id: "t-1".into(),
            outcome: TurnOutcome::Completed,
            reason: None,
            stop_reason: None,
        },
        RuntimeEvent::ConfigChanged {
            revision: 1,
            change: json!({"k": "v"}),
        },
        RuntimeEvent::ContextBudgetReached {
            turn_id: "t-1".into(),
            source: UsageSource::Estimated,
            projected_tokens: 100,
            window_tokens: 200,
            threshold_percent: 80,
            baseline_input_tokens: None,
            estimated_growth_tokens: None,
        },
        RuntimeEvent::permission_resolved(
            "req-1".into(),
            "call-1".into(),
            PermissionDecision::Allow,
            PermissionApprover::User,
            None,
            None,
        ),
    ] {
        journal.append_runtime_event(event).await.unwrap();
    }
    journal
        .append(JournalRecord::TranscriptReplaced {
            revision: 1,
            len: 1,
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("after replace"),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::TranscriptReplacementCommitted {
            revision: 1,
            len: 1,
        })
        .await
        .unwrap();
    // Normal growth after a committed replacement: the segment committed, so these are
    // ordinary appends whose index continues from `len` (ADR 0019).
    journal
        .append(JournalRecord::Message {
            index: 1,
            message: Message::assistant("grown one"),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::Message {
            index: 2,
            message: Message::user("grown two"),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::ModelUsage {
            usage: ModelUsage {
                input: Some(7),
                output: Some(3),
                ..Default::default()
            },
        })
        .await
        .unwrap();
    drop(journal); // release the writer lock before the read side opens the file

    let bytes = std::fs::read(&path).unwrap();
    let lines = split_lines(&bytes);
    let session = Session::open(&path).unwrap();
    Base {
        bytes,
        lines,
        session,
    }
}

fn split_lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    assert!(bytes.ends_with(b"\n"), "the writer always terminates lines");
    bytes[..bytes.len() - 1]
        .split(|b| *b == b'\n')
        .map(<[u8]>::to_vec)
        .collect()
}

fn join_lines(lines: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for line in lines {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

// ---------- mutations ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Truncate,
    RemoveTrailingLf,
    DuplicateLine,
    SwapLines,
    FlipEnvelopeByte,
    DeleteEnvelopeByte,
    InsertEnvelopeByte,
    FlipDataByte,
    AppendUnknownNonCritical,
    AppendUnknownCritical,
    AppendUnknownField,
    AppendUnknownEnum,
}

const KINDS: [Kind; 12] = [
    Kind::Truncate,
    Kind::RemoveTrailingLf,
    Kind::DuplicateLine,
    Kind::SwapLines,
    Kind::FlipEnvelopeByte,
    Kind::DeleteEnvelopeByte,
    Kind::InsertEnvelopeByte,
    Kind::FlipDataByte,
    Kind::AppendUnknownNonCritical,
    Kind::AppendUnknownCritical,
    Kind::AppendUnknownField,
    Kind::AppendUnknownEnum,
];

/// What the contract requires for a mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expected {
    /// Only committed bytes can be lost (truncation, a removed final LF). The read **must**
    /// succeed: dropping a torn tail is not a reason to refuse the file, and ADR 0005 makes a
    /// replacement cut short at end of file recoverable. The folded records are a prefix of
    /// the base.
    SuffixLoss,
    /// Envelope-only byte damage. A successful read keeps the base's records in order minus
    /// any event the damage turned into an unknown non-critical one, and every vanished event
    /// must be reported in `skipped`.
    EnvelopeEither,
    /// Byte damage inside `data`. Either outcome is allowed; a vanished event must still be
    /// reported, and a kept one may differ from the base.
    DataEither,
    /// Content a v1 reader must skip and report: records unchanged, `skipped` non-empty.
    SkippedAppend,
    /// A known event appended: records unchanged, one more event.
    KnownAppend,
    /// The file must be refused.
    Refused,
}

#[derive(Clone, Debug)]
enum Mutation {
    Truncate(usize),
    RemoveTrailingLf,
    DuplicateLine(usize),
    SwapLines(usize, usize),
    FlipByte {
        line: usize,
        offset: usize,
    },
    DeleteByte {
        line: usize,
        offset: usize,
    },
    InsertByte {
        line: usize,
        offset: usize,
        byte: u8,
    },
    Append(Vec<u8>),
}

impl Kind {
    fn expected(self) -> Expected {
        match self {
            Kind::Truncate | Kind::RemoveTrailingLf => Expected::SuffixLoss,
            Kind::FlipEnvelopeByte | Kind::DeleteEnvelopeByte | Kind::InsertEnvelopeByte => {
                Expected::EnvelopeEither
            }
            Kind::FlipDataByte => Expected::DataEither,
            Kind::DuplicateLine | Kind::SwapLines | Kind::AppendUnknownCritical => {
                Expected::Refused
            }
            Kind::AppendUnknownNonCritical => Expected::SkippedAppend,
            Kind::AppendUnknownField | Kind::AppendUnknownEnum => Expected::KnownAppend,
        }
    }

    /// Builds the mutation for one seed. Parameters come only from the generator and the base
    /// shape, so the same seed and base produce the same mutation.
    fn build(self, rng: &mut Rng, base: &Base) -> Mutation {
        let line_count = base.lines.len();
        let line = rng.below(line_count);
        let envelope_end = envelope_end(&base.lines[line]);
        let next_seq = next_seq(base);
        match self {
            Kind::Truncate => Mutation::Truncate(rng.below(base.bytes.len() + 1)),
            Kind::RemoveTrailingLf => Mutation::RemoveTrailingLf,
            Kind::DuplicateLine => Mutation::DuplicateLine(line),
            Kind::SwapLines => {
                let other = (line + 1 + rng.below(line_count - 1)) % line_count;
                Mutation::SwapLines(line, other)
            }
            Kind::FlipEnvelopeByte => Mutation::FlipByte {
                line,
                offset: rng.below(envelope_end.max(1)),
            },
            Kind::DeleteEnvelopeByte => Mutation::DeleteByte {
                line,
                offset: rng.below(envelope_end.max(1)),
            },
            Kind::InsertEnvelopeByte => Mutation::InsertByte {
                line,
                offset: rng.below(envelope_end.max(1)),
                byte: damage_byte(rng),
            },
            Kind::FlipDataByte => {
                let start = envelope_end;
                let width = base.lines[line].len().saturating_sub(start).max(1);
                Mutation::FlipByte {
                    line,
                    offset: start + rng.below(width),
                }
            }
            Kind::AppendUnknownNonCritical => Mutation::Append(
                line_bytes(
                    next_seq,
                    "FutureAuditRecorded",
                    false,
                    json!({"note": "future"}),
                )
                .into_bytes(),
            ),
            Kind::AppendUnknownCritical => Mutation::Append(
                line_bytes(next_seq, "FutureStateRecorded", true, json!({})).into_bytes(),
            ),
            Kind::AppendUnknownField => Mutation::Append(
                line_bytes(
                    next_seq,
                    "TurnFinished",
                    true,
                    json!({
                        "turn_id": "t-2",
                        "outcome": "completed",
                        "future_field": {"a": 1}
                    }),
                )
                .into_bytes(),
            ),
            Kind::AppendUnknownEnum => Mutation::Append(
                line_bytes(
                    next_seq,
                    "TurnFinished",
                    true,
                    json!({"turn_id": "t-2", "outcome": "future_outcome"}),
                )
                .into_bytes(),
            ),
        }
    }
}

/// The `seq` the next appended line must carry.
fn next_seq(base: &Base) -> u64 {
    base.session
        .events()
        .last()
        .map_or(0, |event| event.seq + 1)
}

/// Where the envelope ends and `data` begins, so byte damage can be aimed at one or the
/// other. Every base line has a `"data":` field.
fn envelope_end(line: &[u8]) -> usize {
    let needle = b"\"data\":";
    line.windows(needle.len())
        .position(|w| w == needle)
        .unwrap_or(line.len())
}

/// A byte that is likely to change meaning: either a JSON-structural byte or a value byte.
fn damage_byte(rng: &mut Rng) -> u8 {
    const CHOICES: [u8; 8] = [b'x', b'0', b'"', b',', b'{', b'}', b'\\', 0xFF];
    CHOICES[rng.below(CHOICES.len())]
}

fn line_bytes(seq: u64, event: &str, critical: bool, data: serde_json::Value) -> String {
    format!(
        r#"{{"schema_version":1,"session_id":"fuzz-sid","seq":{seq},"ts":"2026-10-12T00:00:00.000000Z","event":"{event}","critical":{critical},"data":{data}}}"#
    )
}

fn apply(base: &Base, mutation: &Mutation) -> Vec<u8> {
    match mutation {
        Mutation::Truncate(offset) => base.bytes[..*offset].to_vec(),
        Mutation::RemoveTrailingLf => base.bytes[..base.bytes.len() - 1].to_vec(),
        Mutation::DuplicateLine(i) => {
            let mut lines = base.lines.clone();
            lines.insert(i + 1, lines[*i].clone());
            join_lines(&lines)
        }
        Mutation::SwapLines(i, j) => {
            let mut lines = base.lines.clone();
            lines.swap(*i, *j);
            join_lines(&lines)
        }
        Mutation::FlipByte { line, offset } => {
            let mut lines = base.lines.clone();
            if let Some(byte) = lines[*line].get_mut(*offset) {
                *byte ^= 0x20;
            }
            join_lines(&lines)
        }
        Mutation::DeleteByte { line, offset } => {
            let mut lines = base.lines.clone();
            if *offset < lines[*line].len() {
                lines[*line].remove(*offset);
            }
            join_lines(&lines)
        }
        Mutation::InsertByte { line, offset, byte } => {
            let mut lines = base.lines.clone();
            let at = (*offset).min(lines[*line].len());
            lines[*line].insert(at, *byte);
            join_lines(&lines)
        }
        Mutation::Append(bytes) => {
            let mut out = base.bytes.clone();
            out.extend_from_slice(bytes);
            out.push(b'\n');
            out
        }
    }
}

// ---------- the oracle ----------

/// The index's view of a session, rebuilt from the JSONL. `Refused` mirrors a rebuild
/// warning (the file could not be indexed).
#[derive(Debug, PartialEq, Eq)]
enum IndexOutcome {
    Indexed(Vec<IndexedEvent>),
    Refused(String),
}

/// Order-preserving containment: every item of `needle` appears in `haystack` in order.
fn is_subsequence(needle: &[JournalRecord], haystack: &[JournalRecord]) -> bool {
    let mut rest = haystack.iter();
    needle
        .iter()
        .all(|item| rest.any(|candidate| candidate == item))
}

/// A successful read may drop an event only by reporting it as unknown non-critical (ADR
/// 0006 §7b). An event that vanished without a `skipped` entry would be a silent loss.
fn assert_vanished_events_are_reported(base: &Base, session: &Session, context: &str) {
    let present: std::collections::HashSet<u64> =
        session.events().iter().map(|event| event.seq).collect();
    for event in base.session.events() {
        if present.contains(&event.seq) {
            continue;
        }
        assert!(
            session
                .skipped()
                .iter()
                .any(|(seq, _name)| *seq == event.seq),
            "seq {} ({}) vanished without being reported: {context}",
            event.seq,
            event.event
        );
    }
}

fn index_rows(session: &Session) -> Vec<IndexedEvent> {
    session
        .events()
        .iter()
        .map(|event| IndexedEvent {
            seq: event.seq,
            ts: event.ts.clone(),
            event: event.event.clone(),
            critical: event.critical,
            byte_offset: event.byte_offset,
            line_len: event.line_len,
            message_index: match &event.data {
                EventData::MessageAppended { index, .. } => Some(*index),
                _ => None,
            },
            text: visible_text(&event.data),
        })
        .collect()
}

/// The same rule the index uses (ADR 0006 §10): searchable text is the `Text` parts of user
/// and assistant messages, joined with a newline.
fn visible_text(data: &EventData) -> Option<String> {
    let EventData::MessageAppended { message, .. } = data else {
        return None;
    };
    let content = match message {
        Message::User { content, .. } | Message::Assistant { content, .. } => content,
        Message::Tool { .. } => return None,
    };
    let text = content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() { None } else { Some(text) }
}

/// Rebuilds the index for one file, twice, deleting the database in between: the cache must
/// be disposable and the rebuild reproducible (ADR 0006 §10).
fn rebuild_twice(dir: &TempDir, path: &Path, session_id: &str) -> (IndexOutcome, IndexOutcome) {
    let db = dir.file("index.db");
    let first = rebuild(&db, path, session_id);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.file(&format!("index.db{suffix}")));
    }
    let second = rebuild(&db, path, session_id);
    (first, second)
}

fn rebuild(db: &Path, path: &Path, session_id: &str) -> IndexOutcome {
    let mut index = Index::open(db).expect("open the index");
    let warnings = index.rebuild(&[path.to_path_buf()]).expect("rebuild");
    if let Some(warning) = warnings.first() {
        return IndexOutcome::Refused(warning.message.clone());
    }
    IndexOutcome::Indexed(
        index
            .indexed_events(session_id)
            .expect("read the indexed rows"),
    )
}

/// Runs one mutation and asserts the whole contract. Panics with the seed and mutation when
/// an assertion fails, so a failure can be reproduced from the printed case alone.
fn run_case(base: &Base, seed: u64, kind: Kind) {
    let mut rng = Rng::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (kind as u64 + 1));
    let mutation = kind.build(&mut rng, base);
    let mutated = apply(base, &mutation);
    let context = format!("seed={seed} kind={kind:?} mutation={mutation:?}");

    let dir = TempDir::new("fuzz");
    let path = dir.file("s.jsonl");
    std::fs::write(&path, &mutated).unwrap();
    let before = std::fs::read(&path).unwrap();

    let read = std::panic::catch_unwind(AssertUnwindSafe(|| Session::open(&path)))
        .unwrap_or_else(|_| panic!("Session::open panicked: {context}"));
    let after_read = std::fs::read(&path).unwrap();
    assert_eq!(before, after_read, "the read wrote the file: {context}");

    let base_records = base.session.journal_records();
    match read {
        Ok(session) => {
            match kind.expected() {
                Expected::SuffixLoss => {
                    let records = session.journal_records();
                    assert!(
                        records.len() <= base_records.len(),
                        "more records than the base: {context}"
                    );
                    assert_eq!(
                        records,
                        base_records[..records.len()],
                        "records are not a prefix of the base: {context}"
                    );
                }
                Expected::EnvelopeEither => {
                    assert!(
                        is_subsequence(&session.journal_records(), &base_records),
                        "an envelope mutation changed a record: {context}"
                    );
                    assert_vanished_events_are_reported(base, &session, &context);
                }
                Expected::DataEither => {
                    assert_vanished_events_are_reported(base, &session, &context);
                }
                Expected::SkippedAppend => {
                    assert_eq!(
                        session.journal_records(),
                        base_records,
                        "a skipped append changed the transcript: {context}"
                    );
                    let seq = next_seq(base);
                    assert!(
                        session
                            .skipped()
                            .iter()
                            .any(|(skipped_seq, name)| *skipped_seq == seq
                                && name == "FutureAuditRecorded"),
                        "the unknown event was not reported: {context}"
                    );
                }
                Expected::KnownAppend => {
                    assert_eq!(
                        session.journal_records(),
                        base_records,
                        "an appended audit event changed the transcript: {context}"
                    );
                    assert_eq!(
                        session.events().len(),
                        base.session.events().len() + 1,
                        "the appended event was not read: {context}"
                    );
                }
                Expected::Refused => panic!("the file must be refused: {context}"),
            }

            // The index must agree with the reader. An empty read (no committed line) has no
            // session to index, which is the index's `Empty` refusal, not a disagreement.
            let (first, second) = rebuild_twice(&dir, &path, session.session_id());
            assert_eq!(
                first, second,
                "the index rebuild is not reproducible: {context}"
            );
            if session.events().is_empty() {
                assert!(
                    matches!(first, IndexOutcome::Refused(_)),
                    "an empty session must not be indexed: {context}"
                );
            } else {
                assert_eq!(
                    first,
                    IndexOutcome::Indexed(index_rows(&session)),
                    "the index disagrees with the reader: {context}"
                );
            }
        }
        Err(error) => {
            assert!(
                !error.to_string().is_empty(),
                "an error must say why: {context}"
            );
            match kind.expected() {
                Expected::Refused => {}
                Expected::SuffixLoss => panic!(
                    "losing a committed suffix (a torn tail) must still read: {context}: {error}"
                ),
                Expected::EnvelopeEither | Expected::DataEither => {}
                Expected::SkippedAppend | Expected::KnownAppend => {
                    panic!("appended content must be readable: {context}: {error}")
                }
            }
            let (first, second) = rebuild_twice(&dir, &path, "");
            assert_eq!(
                first, second,
                "the index rebuild is not reproducible: {context}"
            );
            if let IndexOutcome::Indexed(rows) = first {
                assert!(
                    rows.is_empty(),
                    "the index accepted a file the reader refused: {context}: {error}"
                );
            }
        }
    }

    let after_all = std::fs::read(&path).unwrap();
    assert_eq!(
        before, after_all,
        "reading or indexing wrote the file: {context}"
    );
}

// ---------- the tests ----------

/// The base must itself be a valid v1 session, or every prefix assertion below is meaningless.
#[tokio::test]
async fn the_fuzz_base_is_a_valid_v1_session() {
    let dir = TempDir::new("fuzz-base");
    let base = build_base(&dir).await;
    assert!(
        base.lines.len() >= 17,
        "the base should have every event kind"
    );
    assert!(base.session.skipped().is_empty());
    assert!(
        base.session.uncommitted_replacements().is_empty(),
        "the base's replacement segment is committed"
    );
    assert_eq!(base.session.session_id(), "fuzz-sid");
    // Every event kind the brief lists is present at least once.
    for name in [
        "SessionCreated",
        "MessageAppended",
        "ToolExecutionStarted",
        "ToolExecutionSettled",
        "ModelUsageRecorded",
        "InboxEnqueued",
        "InputClaimed",
        "InputAdmitted",
        "TurnStarted",
        "TurnFinished",
        "ConfigChanged",
        "ContextBudgetReached",
        "PermissionResolved",
        "TranscriptReplaced",
        "TranscriptReplacementCommitted",
    ] {
        assert!(
            base.session.events().iter().any(|e| e.event == name),
            "the base is missing {name}"
        );
    }
}

#[tokio::test]
async fn corruption_replay_is_total_and_never_writes_the_file() {
    let dir = TempDir::new("fuzz-default");
    let base = build_base(&dir).await;
    let mut cases = 0;
    for seed in 0..16u64 {
        for kind in KINDS {
            run_case(&base, seed, kind);
            cases += 1;
        }
    }
    assert_eq!(cases, 16 * KINDS.len());
}

#[tokio::test]
#[ignore = "large round: run with `cargo test -p radian-session --test fuzz -- --ignored`"]
async fn corruption_replay_big_round() {
    let dir = TempDir::new("fuzz-big");
    let base = build_base(&dir).await;
    for seed in 0..1000u64 {
        for kind in KINDS {
            run_case(&base, seed, kind);
        }
    }
}
