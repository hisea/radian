//! Phase 7: crash injection at every writer write point.
//!
//! A real session is written with the writer (a turn with a settled call, a call that only
//! started, and a runtime event). Then, for every committed line `k` and every prefix length
//! `N` of line `k`, the file is rebuilt as "lines `0..k` committed, then `N` bytes of line `k`
//! written and the process died". Reopening with `resume` must:
//!
//! - drop the torn tail exactly (a valid JSON line without a final LF is still torn: no LF,
//!   no commit, ADR 0006 §3.2);
//! - continue `seq` after the last committed line, so the next append cannot be spliced onto
//!   the half line;
//! - leave a transcript that follows the ADR 0001/0002 recovery rules.
//!
//! What the existing crash tests already cover, and this does not repeat:
//! `radian-session/tests/append.rs` (one synthetic torn fragment; one killed writer),
//! `radian-runtime/tests/recovery.rs` (`crash_traversal_recovers_every_prefix` truncates at
//! line boundaries only; one genuinely half-written line), and
//! `radian-session/tests/e2e.rs` (repair write-back). This file adds every line `k` crossed
//! with every partial length `N`, including `N == line.len()` (a complete line that lost its
//! LF).

mod common;

use agent_core::{
    ContentPart, JournalRecord, Message, ModelUsage, ToolCall, ToolResult, TranscriptJournal,
};
use common::{TempDir, created};
use radian_session::{RuntimeEvent, Session, SessionJournal, WriteError};
use serde_json::json;

/// The session id and the line count of the base built by [`base_lines`].
const SID: &str = "crash-sid";

/// Builds the base session and returns its lines without the LF. The shape is deliberate:
/// line 2 declares two tool calls, `call-1` is settled at line 4, `call-2` only starts at
/// line 6, and line 7 is a runtime event. Every prefix therefore lands on a different ADR
/// 0002 classification.
async fn base_lines(dir: &TempDir) -> Vec<Vec<u8>> {
    let path = dir.file("base.jsonl");
    let journal = SessionJournal::create(&path, SID, created()).unwrap();
    let assistant = Message::Assistant {
        content: vec![ContentPart::text("thinking")],
        reasoning: Vec::new(),
        tool_calls: vec![
            ToolCall {
                id: "call-1".into(),
                name: "echo".into(),
                arguments: json!({"n": 1}),
            },
            ToolCall {
                id: "call-2".into(),
                name: "echo".into(),
                arguments: json!({"n": 2}),
            },
        ],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    };
    let result = || ToolResult::success("call-1", vec![ContentPart::text("out-1")]);
    // The real order: the assistant declares the calls, the markers bracket the execution,
    // and the result message follows the settled marker.
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("go"),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::Message {
            index: 1,
            message: assistant,
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::ToolExecutionStarted {
            call_id: "call-1".into(),
            name: "echo".into(),
            arguments: json!({"n": 1}),
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
        .append(JournalRecord::Message {
            index: 2,
            message: Message::Tool {
                result: result(),
                meta: Default::default(),
            },
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::ToolExecutionStarted {
            call_id: "call-2".into(),
            name: "echo".into(),
            arguments: json!({"n": 2}),
        })
        .await
        .unwrap();
    journal
        .append_runtime_event(RuntimeEvent::TurnStarted {
            turn_id: "t-1".into(),
        })
        .await
        .unwrap();
    drop(journal); // release the lock

    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.ends_with(b"\n"));
    bytes[..bytes.len() - 1]
        .split(|b| *b == b'\n')
        .map(<[u8]>::to_vec)
        .collect()
}

/// The committed prefix: the first `k` lines, LF-terminated. `k` is the number of committed
/// lines, so the last committed `seq` is `k - 1`.
fn committed_prefix(lines: &[Vec<u8>], k: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for line in &lines[..k] {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

/// The partial write: the first `n` bytes of line `k`, with no final LF.
fn partial_write(lines: &[Vec<u8>], k: usize, n: usize) -> Vec<u8> {
    lines[k][..n.min(lines[k].len())].to_vec()
}

/// The result recorded for `call_id` after recovery: its text and error code (`None` for a
/// real result).
fn recovered(history: &[Message], call_id: &str) -> (String, Option<String>) {
    let result = history
        .iter()
        .find_map(|message| match message {
            Message::Tool { result, .. } if result.call_id == call_id => Some(result),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no recovered result for {call_id}: {history:?}"));
    (result.text(), result.error_code.clone())
}

/// The ADR 0002 classification expected for a prefix of `k` committed lines.
fn expected_call_1(k: usize) -> Option<&'static str> {
    match k {
        0..=3 => Some("not_started_after_crash"),
        4 => Some("outcome_unknown_after_crash"),
        _ => None, // the real result survives
    }
}

fn expected_call_2(k: usize) -> &'static str {
    if k <= 6 {
        "not_started_after_crash"
    } else {
        "outcome_unknown_after_crash"
    }
}

/// One case: `k` committed lines, then `n` bytes of line `k` and a crash.
#[tokio::test]
async fn every_partial_line_is_truncated_before_the_next_append() {
    let base_dir = TempDir::new("crash-base");
    let lines = base_lines(&base_dir).await;
    assert_eq!(lines.len(), 8, "the base shape changed");

    let mut cases = 0;
    for k in 0..lines.len() {
        // `N == line.len()` is the important case: the torn tail is valid JSON but has no LF.
        let mut lengths = vec![
            1,
            lines[k].len() / 2,
            lines[k].len().saturating_sub(1),
            lines[k].len(),
        ];
        lengths.sort_unstable();
        lengths.dedup();
        for n in lengths {
            let dir = TempDir::new("crash-case");
            let path = dir.file("s.jsonl");
            let prefix = committed_prefix(&lines, k);
            let partial = partial_write(&lines, k, n);
            let mut body = prefix.clone();
            body.extend_from_slice(&partial);
            std::fs::write(&path, &body).unwrap();

            if k == 0 {
                // No committed line: there is no session to resume. The refusal must leave
                // the file exactly as it was.
                match SessionJournal::resume(&path, SID) {
                    Err(WriteError::Empty { .. }) => {}
                    Err(e) => panic!("k=0 must be refused as empty, got {e}"),
                    Ok(_) => panic!("k=0 must be refused as empty, but resume succeeded"),
                }
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    body,
                    "a refusal wrote the file"
                );
                cases += 1;
                continue;
            }

            let journal = SessionJournal::resume(&path, SID)
                .unwrap_or_else(|e| panic!("k={k} n={n}: resume failed: {e}"));
            assert_eq!(
                journal.next_seq(),
                k as u64,
                "k={k} n={n}: seq must continue after the last committed line"
            );
            // The torn tail is gone before anything is appended.
            assert_eq!(
                std::fs::read(&path).unwrap(),
                prefix,
                "k={k} n={n}: the torn tail was not truncated"
            );

            journal
                .append(JournalRecord::ModelUsage {
                    usage: ModelUsage {
                        input: Some(1),
                        output: Some(2),
                        ..Default::default()
                    },
                })
                .await
                .unwrap();
            assert_eq!(journal.next_seq(), k as u64 + 1);
            drop(journal);

            // The new line is not spliced onto the half line: it parses, and its seq is k.
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                bytes.starts_with(&prefix),
                "k={k} n={n}: the committed prefix changed"
            );
            let appended = &bytes[prefix.len()..];
            assert!(
                appended.ends_with(b"\n"),
                "k={k} n={n}: the appended line has no LF"
            );
            let appended: serde_json::Value =
                serde_json::from_slice(&appended[..appended.len() - 1])
                    .unwrap_or_else(|e| panic!("k={k} n={n}: the appended line is not JSON: {e}"));
            assert_eq!(appended["seq"], json!(k as u64));
            assert_eq!(appended["event"], json!("ModelUsageRecorded"));

            // The whole file reads back with a contiguous seq and the recovery rules hold.
            let session = Session::open(&path).unwrap();
            let seqs: Vec<u64> = session.events().iter().map(|e| e.seq).collect();
            assert_eq!(
                seqs,
                (0..=k as u64).collect::<Vec<_>>(),
                "k={k} n={n}: seq is not contiguous"
            );
            let history = session.history().unwrap();

            // The assistant message that declares the two calls is line 2, so only a prefix
            // of at least 3 committed lines has calls to repair.
            if k >= 3 {
                let (text_1, code_1) = recovered(&history, "call-1");
                match expected_call_1(k) {
                    Some(code) => assert_eq!(code_1.as_deref(), Some(code), "k={k} n={n}: call-1"),
                    None => {
                        assert_eq!(text_1, "out-1", "k={k} n={n}: call-1's real result");
                        assert_eq!(code_1, None, "k={k} n={n}: call-1");
                    }
                }
                let (_, code_2) = recovered(&history, "call-2");
                assert_eq!(
                    code_2.as_deref(),
                    Some(expected_call_2(k)),
                    "k={k} n={n}: call-2"
                );
            } else {
                assert!(
                    history.iter().all(|m| !matches!(m, Message::Tool { .. })),
                    "k={k} n={n}: no call was declared yet: {history:?}"
                );
            }

            cases += 1;
        }
    }
    assert!(
        cases >= 28,
        "expected a case per line and partial length, got {cases}"
    );
}
