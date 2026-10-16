//! ADR 0040 end-to-end: a system crash injected between every pair of consecutive sync points
//! over a scripted turn, and the per-step fsync count.
//!
//! The writer's schema is group-committed: a record is `write`+`flush`ed on every append, and
//! `fsync`ed only at a sync point (a boundary, or before an external side effect). A system
//! crash therefore loses the bytes written since the last fsync — modeled by
//! `SessionJournal::crash_after_syncs`, which drops them at the next write and ends the turn
//! like a dead process. The invariant under test: an executed side effect (a written file) can
//! only exist when its `ToolExecutionStarted` is already on disk.

mod common;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{Agent, InputSource, Message, TurnOptions};
use common::{MemTools, Recorder, ScriptedModel, TempDir, call, calls, created, text};
use radian_runtime::{Driver, Inbox};
use radian_session::{EventData, Session, SessionJournal};
use serde_json::json;

const SID: &str = "group-commit-sid";

/// Two calls, each with an observable side effect (a written file), then a final answer.
fn two_write_calls(effects: [&PathBuf; 2]) -> Vec<agent_core::ModelResponse> {
    vec![
        calls(vec![call(
            "c1",
            "write",
            json!({"path": effects[0], "text": "one"}),
        )]),
        calls(vec![call(
            "c2",
            "write",
            json!({"path": effects[1], "text": "two"}),
        )]),
        text("done"),
    ]
}

struct TurnRun {
    path: PathBuf,
    /// Fsyncs the writer performed during the turn.
    syncs: u64,
    /// Writes (attempted appends) during the turn.
    writes: u64,
    /// Whether the injected crash fired.
    crashed: bool,
}

/// Runs the scripted turn over a fresh session file, optionally arming the crash seam so that
/// the `crash_at`-th write of the turn crashes. Every writer handle is dropped on return, so
/// the file can be resumed.
async fn run_turn(dir: &TempDir, effects: [&PathBuf; 2], crash_at: Option<u64>) -> TurnRun {
    let path = dir.file("s.jsonl");
    let journal = Arc::new(SessionJournal::create(&path, SID, created()).unwrap());
    let inbox = Inbox::durable(16, Arc::clone(&journal));
    let recorder = Arc::new(Recorder::default());
    let tools = MemTools::new();
    let model = ScriptedModel::new(two_write_calls(effects));
    let agent = Arc::new(Agent::new(model, tools));
    let mut driver = Driver::open(
        &agent,
        Arc::clone(&journal),
        Arc::clone(&inbox),
        Arc::clone(&recorder) as Arc<dyn agent_core::EventSink>,
        TurnOptions::default(),
    )
    .await
    .unwrap();

    // The input is enqueued before the crash seam is armed: the seam is about the turn's own
    // writes (the sync points between which the crash is injected).
    inbox.followup("go", InputSource::User).await.unwrap();
    let before_syncs = journal.sync_count();
    let before_writes = journal.write_count();
    if let Some(n) = crash_at {
        journal.crash_at_write(n);
    }

    let mut handle = tokio::spawn(async move { driver.run().await });
    let crashed = {
        let finished = recorder.wait_finished();
        tokio::pin!(finished);
        tokio::select! {
            _ = &mut handle => true,
            _ = &mut finished => false,
        }
    };
    if !crashed {
        handle.abort();
        let _ = handle.await;
    }
    let syncs = journal.sync_count() - before_syncs;
    let writes = journal.write_count() - before_writes;
    // The crash seam's own flag: the core's `RunFinished` is emitted before the driver writes
    // `TurnFinished`, so waiting on the live events would race the crash. The seam truncates
    // synchronously under the writer lock, so once it has fired the file is stable.
    let crashed = journal.crash_fired();

    // Release the single-writer lock before the caller resumes the file.
    drop(inbox);
    drop(recorder);
    drop(journal);
    TurnRun {
        path,
        syncs,
        writes,
        crashed,
    }
}

fn started_on_disk(session: &Session) -> HashSet<String> {
    session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::ToolExecutionStarted { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect()
}

fn settled_on_disk(session: &Session) -> HashSet<String> {
    session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::ToolExecutionSettled { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect()
}

/// The result recorded for `call_id` in the folded history, or `None` if the call was not
/// declared yet.
fn recorded_result(history: &[Message], call_id: &str) -> Option<agent_core::ToolResult> {
    history.iter().find_map(|message| match message {
        Message::Tool { result, .. } if result.call_id == call_id => Some(result.clone()),
        _ => None,
    })
}

fn declared_calls(history: &[Message]) -> Vec<String> {
    let mut calls = Vec::new();
    for message in history {
        if let Message::Assistant { tool_calls, .. } = message {
            calls.extend(tool_calls.iter().map(|call| call.id.clone()));
        }
    }
    calls
}

/// The crash traversal: crash at every record boundary — a superset of "between every pair of
/// consecutive sync points" — resume, and check the ADR 0002/0040 invariants. Crashing at a
/// non-sync record boundary is what catches a missing sync point: with `ToolExecutionStarted`
/// un-synced the tool runs and its file exists while the marker is still only in the page cache.
#[tokio::test]
async fn a_crash_between_sync_points_keeps_the_invariant() {
    // The reference turn: no crash, learn how many record boundaries it has.
    let ref_dir = TempDir::new("gc-ref");
    let ref_effects_dir = TempDir::new("gc-ref-effects");
    let ref_effects = [ref_effects_dir.file("e1"), ref_effects_dir.file("e2")];
    let reference = run_turn(&ref_dir, [&ref_effects[0], &ref_effects[1]], None).await;
    assert!(!reference.crashed, "the reference turn must complete");
    assert!(
        ref_effects[0].exists() && ref_effects[1].exists(),
        "the reference turn ran both tools"
    );
    assert!(
        reference.writes >= 10,
        "the reference turn should have many records, got {}",
        reference.writes
    );

    let mut crashed_cases = 0;
    for k in 1..=reference.writes {
        let dir = TempDir::new("gc-case");
        let effects_dir = TempDir::new("gc-effects");
        let effects = [effects_dir.file("e1"), effects_dir.file("e2")];
        let run = run_turn(&dir, [&effects[0], &effects[1]], Some(k)).await;
        if run.crashed {
            crashed_cases += 1;
        }

        // The truncation is line-aligned, so the file is a valid session.
        let session = Session::open(&run.path)
            .unwrap_or_else(|e| panic!("k={k}: the crashed file must read back: {e}"));
        let started = started_on_disk(&session);
        let settled = settled_on_disk(&session);

        // 1. No executed side effect lacks its ToolExecutionStarted (ADR 0040 decision 4).
        for (call_id, effect) in [("c1", &effects[0]), ("c2", &effects[1])] {
            if effect.exists() {
                assert!(
                    started.contains(call_id),
                    "k={k}: {call_id} ran (its file exists) but ToolExecutionStarted is not on disk"
                );
            }
        }

        // 2. Every declared call is answered by the fold, and an unpaired call is classified by
        //    its marker (ADR 0002): settled → the real result; only started →
        //    outcome_unknown_after_crash; neither → not_started_after_crash.
        let history = session
            .history()
            .unwrap_or_else(|e| panic!("k={k}: the crashed transcript must fold: {e}"));
        for call_id in declared_calls(&history) {
            let result = recorded_result(&history, &call_id)
                .unwrap_or_else(|| panic!("k={k}: {call_id} has no result: {history:?}"));
            if settled.contains(&call_id) {
                // The real result survived; the synthetic codes must not be used.
                assert!(
                    !matches!(
                        result.error_code.as_deref(),
                        Some("outcome_unknown_after_crash" | "not_started_after_crash")
                    ),
                    "k={k}: {call_id} settled, so its result must be real: {result:?}"
                );
            } else {
                let expected = if started.contains(&call_id) {
                    "outcome_unknown_after_crash"
                } else {
                    "not_started_after_crash"
                };
                assert_eq!(
                    result.error_code.as_deref(),
                    Some(expected),
                    "k={k}: {call_id} without a settle must be {expected}: {result:?}"
                );
            }
        }
    }
    assert_eq!(
        crashed_cases, reference.writes,
        "the injected crash must have fired at every record boundary"
    );
}

/// The per-step fsync count: a two-tool turn fsyncs for `ToolExecutionStarted` × 2 and
/// `TurnFinished` — not once per record.
#[tokio::test]
async fn a_step_fsyncs_only_at_its_sync_points() {
    let dir = TempDir::new("gc-count");
    let effects_dir = TempDir::new("gc-count-effects");
    let effects = [effects_dir.file("e1"), effects_dir.file("e2")];
    let run = run_turn(&dir, [&effects[0], &effects[1]], None).await;
    assert!(!run.crashed);

    let session = Session::open(&run.path).unwrap();
    let records = session.events().len() as u64;
    assert!(
        run.syncs <= 3,
        "a two-tool turn must fsync at most 3 times (2 × ToolExecutionStarted + TurnFinished), \
         got {} across {records} records",
        run.syncs
    );
    assert!(
        run.syncs >= 2,
        "the sync points must still fsync, got {}",
        run.syncs
    );
    println!(
        "group commit: {records} records, {} fsyncs in the turn ({:.1} records per fsync)",
        run.syncs,
        records as f64 / run.syncs as f64
    );
}
