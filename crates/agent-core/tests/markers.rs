//! Tool execution markers and the recovery rules (A2).

mod common;

use std::sync::{Arc, Mutex};

use agent_core::testing::RecordingJournal;
use agent_core::*;
use common::*;
use serde_json::json;

/// Opens a session whose every transcript change is journaled to `journal`.
async fn session_with(
    model: Arc<ScriptedModel>,
    tools: impl ToolProvider,
    journal: Arc<dyn TranscriptJournal>,
) -> AgentSession<Arc<ScriptedModel>, impl ToolProvider, ()> {
    Arc::new(Agent::new(model, tools))
        .open_session_with_journal(Vec::new(), journal)
        .await
        .unwrap()
}

fn started(id: &str, name: &str, arguments: serde_json::Value) -> JournalRecord {
    JournalRecord::ToolExecutionStarted {
        call_id: id.into(),
        name: name.into(),
        arguments,
    }
}

fn settled(id: &str, text: &str) -> JournalRecord {
    JournalRecord::ToolExecutionSettled {
        call_id: id.into(),
        result: ToolResult::success(id, vec![ContentPart::text(text)]),
    }
}

fn message(index: usize, text: &str) -> JournalRecord {
    JournalRecord::Message {
        index,
        message: Message::user(text),
    }
}

// ---------- replay / repair / recover as pure functions (AC-A2-10) ----------

#[test]
fn replay_rejects_non_contiguous_indexes() {
    use agent_core::ReplayError;
    // A gap ...
    let records = [message(0, "a"), message(2, "c")];
    assert_eq!(
        replay(&records),
        Err(ReplayError::NotContiguous {
            expected: 1,
            found: 2
        })
    );
    // ... a repeat ...
    let records = [message(0, "a"), message(1, "b"), message(1, "b")];
    assert_eq!(
        replay(&records),
        Err(ReplayError::NotContiguous {
            expected: 2,
            found: 1
        })
    );
    // ... and a wrong start.
    let records = [message(1, "a")];
    assert_eq!(
        replay(&records),
        Err(ReplayError::NotContiguous {
            expected: 0,
            found: 1
        })
    );
}

#[test]
fn replay_ignores_markers_and_accepts_empty_input() {
    let records = [
        message(0, "a"),
        started("c1", "t", json!({})),
        message(1, "b"),
        settled("c1", "ok"),
    ];
    assert_eq!(
        replay(&records).unwrap(),
        vec![Message::user("a"), Message::user("b")]
    );
    assert_eq!(replay(&[]).unwrap(), Vec::<Message>::new());
}

#[test]
fn repair_fills_unpaired_calls_by_the_three_rules() {
    let history = vec![
        Message::user("go"),
        Message::Assistant {
            content: vec![],
            tool_calls: vec![
                call("ran", "a", json!({})),
                call("half", "a", json!({})),
                call("never", "a", json!({})),
            ],
            provider_payload: None,
        },
        // The `ran` result message is already there.
        Message::Tool {
            result: ToolResult::success("ran", vec![ContentPart::text("done")]),
        },
    ];
    let assistant = history[1].clone();
    let records = [
        message(0, "go"),
        JournalRecord::Message {
            index: 1,
            message: assistant,
        },
        started("ran", "a", json!({})),
        started("half", "a", json!({})),
        settled("ran", "done"),
    ];

    let fill = repair(&history, &records);

    assert_eq!(fill.len(), 2);
    // `half` only started: the outcome is unknown and must not be assumed unexecuted.
    let half = result_for(&fill, "half");
    assert_eq!(code(half), Some("outcome_unknown_after_crash"));
    assert!(half.is_error);
    assert!(half.text().to_lowercase().contains("unknown"));
    // `never` never started.
    let never = result_for(&fill, "never");
    assert_eq!(code(never), Some("not_started_after_crash"));
    assert!(never.is_error);

    // `recover` produces a complete, valid history from the records alone.
    let mut recovered = replay(&records).unwrap();
    recovered.extend(repair(&recovered, &records));
    assert!(matches!(
        &recovered[3],
        Message::Tool { result } if result.call_id == "half"
    ));
}

#[test]
fn repair_fills_from_settled_exactly_and_leaves_complete_history_alone() {
    let history = vec![
        Message::user("go"),
        Message::Assistant {
            content: vec![],
            tool_calls: vec![call("c1", "a", json!({}))],
            provider_payload: None,
        },
    ];
    let records = [
        message(0, "go"),
        message(1, ""),
        started("c1", "a", json!({"final": 1})),
        settled("c1", "the settled text"),
    ];
    let fill = repair(&history, &records);
    assert_eq!(
        fill,
        vec![Message::Tool {
            result: ToolResult::success("c1", vec![ContentPart::text("the settled text")]),
        }]
    );

    // A history that already answers every call needs nothing.
    let complete = vec![history[0].clone(), history[1].clone(), fill[0].clone()];
    assert_eq!(repair(&complete, &records), Vec::<Message>::new());
}

#[tokio::test(start_paused = true)]
async fn recover_yields_history_that_validates() {
    let records = [
        message(0, "go"),
        JournalRecord::Message {
            index: 1,
            message: Message::Assistant {
                content: vec![],
                tool_calls: vec![call("c1", "a", json!({})), call("c2", "a", json!({}))],
                provider_payload: None,
            },
        },
        started("c1", "a", json!({})),
    ];
    let history = recover(&records).unwrap();
    // The recovered history opens a new session, i.e. passes the real validation.
    Arc::new(Agent::new(ScriptedModel::new(Vec::new()), NoTools))
        .open_session_with_history(history.clone())
        .await
        .unwrap();
    assert_eq!(result_ids(&history), ["c1", "c2"]);
    assert_eq!(
        code(result_for(&history, "c1")),
        Some("outcome_unknown_after_crash")
    );
    assert_eq!(
        code(result_for(&history, "c2")),
        Some("not_started_after_crash")
    );
}
