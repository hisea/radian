#![allow(dead_code)]
//! A synthetic long session, for the long-session measurements (Phase 5 / Phase 7).
//!
//! One generator, shared by the `radian-session`, `radian-tui` and `radian-exec` perf tests via
//! `#[path]`, so all three measure the **same** file. It writes valid v1 JSONL directly (the
//! envelope, `MessageAppended`, and the ADR 0019 replacement/compaction records), without
//! going through the writer's per-record fsync.
//!
//! The mix: every 5th unit is an assistant tool call with a 4 KiB tool result; every 5th unit
//! is an assistant with ~2.4 KiB of text and ~1.6 KiB of reasoning; the rest alternate short
//! user and assistant messages. The compacted variant inserts `compactions` committed
//! compactions (ADR 0018/0019): each keeps a small suffix, so the live transcript stays small
//! while the raw file keeps every message.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use radian_core::{
    ContentPart, InputSource, Message, MessageMeta, OpaqueJson, ToolCall, ToolResult,
};
use serde_json::json;

/// A fixed, format-valid timestamp: the reader checks the shape, not the value.
const TS: &str = "2026-10-12T00:00:00.000000Z";

/// How many messages one compaction keeps (in addition to the summary message). Small, so the
/// live transcript of a compacted session is much smaller than its raw file.
const KEEP: usize = 200;

/// What the generator wrote, for the report.
#[derive(Debug, Clone, Copy)]
pub struct LongSessionStats {
    /// Every `MessageAppended` record written (the raw file's message count).
    pub raw_messages: usize,
    /// The messages `history()` reconstructs after the last committed compaction.
    pub live_messages: usize,
    /// The file size in bytes.
    pub bytes: u64,
}

struct Envelope<'a> {
    out: &'a mut BufWriter<File>,
    session_id: &'a str,
    seq: u64,
}

impl Envelope<'_> {
    fn journal(&mut self, event: &str, data: serde_json::Value) {
        let line = json!({
            "schema_version": 1, "session_id": self.session_id, "seq": self.seq, "ts": TS,
            "event": event, "critical": true, "data": data
        });
        writeln!(self.out, "{line}").unwrap();
        self.seq += 1;
    }
}

/// The summary message a compaction installs (ADR 0018's shape: a system-sourced user
/// message with a `compaction_summary` origin).
fn summary_message() -> Message {
    Message::User {
        content: vec![ContentPart::text(
            "This is a summary of the earlier conversation. Continue the work from it.",
        )],
        meta: MessageMeta {
            id: None,
            source: Some(InputSource::System),
            origin: Some(
                OpaqueJson::parse(r#"{"kind":"compaction_summary"}"#).expect("valid JSON literal"),
            ),
        },
    }
}

/// The unit at counter `i`: one message, or an assistant tool call plus its 4 KiB result.
fn unit(i: usize) -> Vec<Message> {
    match i % 5 {
        0 => {
            let call = ToolCall {
                id: format!("call-{i}"),
                name: "read".into(),
                arguments: json!({"path": "file.txt"}),
            };
            let assistant = Message::Assistant {
                content: Vec::new(),
                reasoning: Vec::new(),
                tool_calls: vec![call],
                provider_payload: None,
                interrupted: false,
                meta: MessageMeta::default(),
            };
            let result = Message::Tool {
                result: ToolResult::success(
                    format!("call-{i}"),
                    vec![ContentPart::text("x".repeat(4 * 1024))],
                ),
                meta: MessageMeta::default(),
            };
            vec![assistant, result]
        }
        1 => vec![Message::Assistant {
            content: vec![ContentPart::text(format!(
                "assistant long reply {i} {}",
                "lorem ipsum ".repeat(200)
            ))],
            reasoning: vec![ContentPart::text(format!(
                "reasoning for {i} {}",
                "because of the previous step ".repeat(60)
            ))],
            tool_calls: Vec::new(),
            provider_payload: None,
            interrupted: false,
            meta: MessageMeta::default(),
        }],
        2 | 4 => vec![Message::user(format!(
            "user message {i} with a few words on it"
        ))],
        _ => vec![Message::assistant(format!(
            "assistant reply {i} with a few words on it"
        ))],
    }
}

/// Appends messages until at least `target` have been written in this call.
fn append_messages(
    env: &mut Envelope<'_>,
    len: &mut usize,
    raw: &mut usize,
    counter: &mut usize,
    target: usize,
) {
    let start = *raw;
    while *raw - start < target {
        for message in unit(*counter) {
            env.journal(
                "MessageAppended",
                json!({"index": *len, "message": serde_json::to_value(&message).unwrap()}),
            );
            *len += 1;
            *raw += 1;
        }
        *counter += 1;
    }
}

/// Writes a valid v1 session with about `n` messages and `compactions` committed compactions
/// (0..=3). Returns what it wrote.
pub fn write_long_session(
    path: &Path,
    session_id: &str,
    n: usize,
    compactions: usize,
) -> LongSessionStats {
    let file = File::create(path).unwrap();
    let mut out = BufWriter::new(file);
    let mut env = Envelope {
        out: &mut out,
        session_id,
        seq: 0,
    };
    env.journal(
        "SessionCreated",
        json!({"workspace": "/work/example", "config_snapshot": {}}),
    );

    let mut len = 0usize;
    let mut raw = 0usize;
    let mut counter = 0usize;
    let segments = compactions + 1;
    let per_segment = n / segments;

    for segment in 0..segments {
        append_messages(&mut env, &mut len, &mut raw, &mut counter, per_segment);
        if segment >= compactions {
            continue;
        }
        let revision = (segment + 1) as u64;
        env.journal(
            "ContextCompacted",
            json!({
                "revision": revision,
                "source_revision": revision - 1,
                "source_len": len,
                "summary": "compacted summary of the earlier conversation",
                "first_kept": len.saturating_sub(KEEP),
                "tokens_before": 100_000,
                "tokens_after": 10_000,
                "below_target": false,
                "model": "mock",
            }),
        );
        let new_len = KEEP + 1;
        env.journal(
            "TranscriptReplaced",
            json!({"revision": revision, "len": new_len}),
        );
        env.journal(
            "MessageAppended",
            json!({"index": 0, "message": serde_json::to_value(summary_message()).unwrap()}),
        );
        for i in 0..KEEP {
            let message = if i % 2 == 0 {
                Message::user(format!("kept user {revision}-{i}"))
            } else {
                Message::assistant(format!("kept assistant {revision}-{i}"))
            };
            env.journal(
                "MessageAppended",
                json!({"index": i + 1, "message": serde_json::to_value(&message).unwrap()}),
            );
            raw += 1;
        }
        env.journal(
            "TranscriptReplacementCommitted",
            json!({"revision": revision, "len": new_len}),
        );
        len = new_len;
    }
    out.flush().unwrap();
    drop(out);
    LongSessionStats {
        raw_messages: raw,
        live_messages: len,
        bytes: std::fs::metadata(path).unwrap().len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generator must produce files the reader accepts; the measurements are meaningless
    /// otherwise. This is a normal (non-`#[ignore]`) test, so the generator is exercised by
    /// the default suite too.
    #[test]
    fn the_generated_file_parses_and_compacts() {
        let dir = std::env::temp_dir().join(format!("long-session-gen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (n, compactions, live_min) in [(60usize, 0usize, 60usize), (600, 3, 1)] {
            let path = dir.join(format!("s-{n}-{compactions}.jsonl"));
            let stats = write_long_session(&path, "gen", n, compactions);
            assert!(stats.raw_messages >= n, "{stats:?}");
            let session = radian_session::Session::open(&path).unwrap();
            let history = session.history().unwrap();
            if compactions == 0 {
                assert_eq!(history.len(), stats.live_messages);
            } else {
                assert!(history.len() >= live_min, "{:?}", history.len());
                assert!(
                    history.len() < stats.raw_messages,
                    "a compacted session's live history must be smaller: {stats:?}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
