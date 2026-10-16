//! The typed runtime-event writer (ADR 0006 §5.5–§5.7): `seq` continuity when interleaved
//! with transcript appends, `critical` from the event type, and exclusion from transcript
//! folding.

mod common;

use agent_core::{
    InputSource, JournalRecord, Message, MessageId, TranscriptJournal, recover, replay,
};
use common::{TempDir, created};
use radian_session::{
    EventData, ExportOptions, InboxLane, Index, MemoryInjectedRecord, RuntimeEvent, Session,
    SessionJournal, TurnOutcome, TurnReason, WriteError,
};
use serde_json::json;

fn inbox_message(text: &str, id: &str) -> (Message, MessageId) {
    let id = MessageId(id.into());
    let message = Message::user(text);
    (message, id)
}

fn all_events() -> Vec<RuntimeEvent> {
    let (message, message_id) = inbox_message("hello", "inbox-1");
    vec![
        RuntimeEvent::InboxEnqueued {
            message,
            message_id: message_id.clone(),
            lane: InboxLane::NextTurn,
            source: InputSource::User,
        },
        RuntimeEvent::InputClaimed {
            message_ids: vec![message_id.clone()],
            turn_id: "t-1".into(),
        },
        RuntimeEvent::InputAdmitted {
            message_id,
            transcript_index: 0,
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
            revision: 3,
            change: json!({"kind": "ModelRoute", "route": "fast"}),
        },
    ]
}

#[tokio::test]
async fn runtime_events_keep_seq_continuous_and_do_not_touch_the_message_index() {
    let dir = TempDir::new("runtime-seq");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid", created()).unwrap();

    // Interleave the two write paths. `seq` is the line counter; `index` is the transcript
    // subscript — two independent counters (ADR 0006 §3).
    for (n, event) in all_events().into_iter().enumerate() {
        journal.append_runtime_event(event).await.unwrap();
        journal
            .append(JournalRecord::Message {
                index: n,
                message: Message::user(format!("m{n}")),
            })
            .await
            .unwrap();
    }

    let session = Session::open(&path).unwrap();
    let seqs: Vec<u64> = session.events().iter().map(|e| e.seq).collect();
    assert_eq!(
        seqs,
        (0..=12).collect::<Vec<_>>(),
        "seq must be strictly continuous across both writers"
    );
    let indexes: Vec<u64> = session
        .events()
        .iter()
        .filter_map(|e| match &e.data {
            EventData::MessageAppended { index, .. } => Some(*index),
            _ => None,
        })
        .collect();
    assert_eq!(
        indexes,
        (0..6).collect::<Vec<_>>(),
        "MessageAppended.index stays contiguous"
    );
    assert_eq!(journal.next_seq(), 13);
}

#[tokio::test]
async fn every_runtime_event_writes_its_documented_critical_flag() {
    let dir = TempDir::new("runtime-critical");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid", created()).unwrap();
    for event in all_events() {
        // The flag is a property of the type (ADR 0006 §4); every runtime event is critical.
        assert!(event.critical(), "{} must be critical", event.name());
        journal.append_runtime_event(event).await.unwrap();
    }

    let session = Session::open(&path).unwrap();
    let by_name: Vec<(String, bool)> = session
        .events()
        .iter()
        .filter(|e| e.event != "SessionCreated")
        .map(|e| (e.event.clone(), e.critical))
        .collect();
    assert_eq!(
        by_name,
        [
            ("InboxEnqueued".to_string(), true),
            ("InputClaimed".to_string(), true),
            ("InputAdmitted".to_string(), true),
            ("TurnStarted".to_string(), true),
            ("TurnFinished".to_string(), true),
            ("ConfigChanged".to_string(), true),
        ]
    );
}

#[tokio::test]
async fn runtime_events_round_trip_through_typed_reader_variants() {
    let dir = TempDir::new("runtime-roundtrip");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid", created()).unwrap();
    for event in all_events() {
        journal.append_runtime_event(event).await.unwrap();
    }
    let session = Session::open(&path).unwrap();

    match &session.events()[1].data {
        EventData::InboxEnqueued {
            message,
            message_id,
            lane,
            source,
        } => {
            assert_eq!(message.text(), "hello");
            assert_eq!(message_id.0, "inbox-1");
            assert_eq!(*lane, InboxLane::NextTurn);
            assert_eq!(*source, InputSource::User);
        }
        other => panic!("expected InboxEnqueued, got {other:?}"),
    }
    match &session.events()[3].data {
        EventData::InputAdmitted {
            message_id,
            transcript_index,
        } => {
            assert_eq!(message_id.0, "inbox-1");
            assert_eq!(*transcript_index, 0);
        }
        other => panic!("expected InputAdmitted, got {other:?}"),
    }
    match &session.events()[5].data {
        EventData::TurnFinished {
            outcome,
            reason,
            stop_reason,
            ..
        } => {
            assert_eq!(*outcome, TurnOutcome::Completed);
            assert!(reason.is_none() && stop_reason.is_none());
        }
        other => panic!("expected TurnFinished, got {other:?}"),
    }
    match &session.events()[6].data {
        EventData::ConfigChanged { revision, change } => {
            assert_eq!(*revision, 3);
            assert_eq!(change["kind"], "ModelRoute");
        }
        other => panic!("expected ConfigChanged, got {other:?}"),
    }
}

/// ADR 0021 D6: `MemoryInjected` carries only ids/revisions/scopes, is audit (`critical:
/// false`), never enters the transcript fold, and the exports show it without content.
#[tokio::test]
async fn memory_injected_round_trips_without_content_and_is_not_critical() {
    let dir = TempDir::new("memory-injected");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid", created()).unwrap();
    let event = RuntimeEvent::MemoryInjected {
        records: vec![
            MemoryInjectedRecord {
                id: "mem-1".into(),
                revision: 3,
                scope: json!("global"),
            },
            MemoryInjectedRecord {
                id: "mem-2".into(),
                revision: 1,
                scope: json!({"project": "prj-1"}),
            },
        ],
    };
    assert_eq!(event.name(), "MemoryInjected");
    assert!(
        !event.critical(),
        "the injected set is audit only (ADR 0021 D6)"
    );
    journal.append_runtime_event(event).await.unwrap();

    let session = Session::open(&path).unwrap();
    let injected = session
        .events()
        .iter()
        .find(|e| e.event == "MemoryInjected")
        .expect("the event round-trips");
    assert!(!injected.critical);
    match &injected.data {
        EventData::MemoryInjected { records } => {
            assert_eq!(records.len(), 2);
            assert_eq!(records[0].id, "mem-1");
            assert_eq!(records[0].revision, 3);
            assert_eq!(records[0].scope, json!("global"));
            assert_eq!(records[1].scope, json!({"project": "prj-1"}));
        }
        other => panic!("expected MemoryInjected, got {other:?}"),
    }
    // Audit only: never a transcript record.
    assert!(session.journal_records().is_empty());
    // Markdown shows the count; JSON carries the ids. Neither carries content (there is none).
    let opts = ExportOptions::default();
    let markdown = radian_session::export_markdown(&session, &opts).unwrap();
    assert!(
        markdown.contains("memory injected: 2 record(s)"),
        "{markdown}"
    );
    let json = radian_session::export_json(&session, &opts).unwrap();
    assert!(json.contains("mem-1"), "{json}");
    assert!(json.contains("prj-1"), "{json}");
}

#[tokio::test]
async fn an_unknown_stop_reason_is_written_as_reason_plus_stop_reason() {
    let dir = TempDir::new("runtime-stop-reason");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid", created()).unwrap();
    journal
        .append_runtime_event(RuntimeEvent::TurnFinished {
            turn_id: "t-9".into(),
            outcome: TurnOutcome::Incomplete,
            reason: Some(TurnReason::UnknownStopReason),
            stop_reason: Some("weird_new_reason".into()),
        })
        .await
        .unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    let line = raw.lines().nth(1).unwrap();
    let value: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(value["data"]["outcome"], "incomplete");
    assert_eq!(value["data"]["reason"], "unknown_stop_reason");
    assert_eq!(value["data"]["stop_reason"], "weird_new_reason");
    // The common case omits both optional fields entirely.
    let completed = Session::open(&path).unwrap();
    drop(completed);
}

#[tokio::test]
async fn runtime_events_never_enter_the_transcript_fold() {
    let dir = TempDir::new("runtime-fold");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid", created()).unwrap();
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("real message"),
        })
        .await
        .unwrap();
    for event in all_events() {
        journal.append_runtime_event(event).await.unwrap();
    }

    let session = Session::open(&path).unwrap();
    let records = session.journal_records();
    assert_eq!(
        records.len(),
        1,
        "only the Message record is a transcript record"
    );
    assert!(matches!(
        records[0],
        JournalRecord::Message { index: 0, .. }
    ));
    assert_eq!(
        replay(&records).unwrap(),
        [Message::user("real message")],
        "replay ignores runtime events"
    );
    assert_eq!(session.history().unwrap(), [Message::user("real message")]);
    assert_eq!(recover(&records).unwrap(), [Message::user("real message")]);
}

/// ADR 0030 O5: the two lead-journal task events round-trip typed, are `critical = false`,
/// skip their optional fields when absent, and never enter the transcript fold.
#[tokio::test]
async fn the_task_events_round_trip_and_are_not_critical() {
    let dir = TempDir::new("task-events");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid-task", created()).unwrap();

    let spawned = RuntimeEvent::TaskSpawned {
        agent_session_id: "task-1".into(),
        worktree: Some("/work/example/.radian-worktrees/task-1".into()),
        label: Some("slice 1".into()),
    };
    assert_eq!(spawned.name(), "TaskSpawned");
    assert!(!spawned.critical(), "a task event is audit (ADR 0030 O5)");
    journal.append_runtime_event(spawned).await.unwrap();
    // A `shared`-isolation task has no worktree and may have no label; both are skipped.
    journal
        .append_runtime_event(RuntimeEvent::TaskSpawned {
            agent_session_id: "task-2".into(),
            worktree: None,
            label: None,
        })
        .await
        .unwrap();
    let finished = RuntimeEvent::TaskFinished {
        agent_session_id: "task-1".into(),
        outcome: TurnOutcome::Completed,
    };
    assert_eq!(finished.name(), "TaskFinished");
    assert!(!finished.critical(), "a task event is audit (ADR 0030 O5)");
    journal.append_runtime_event(finished).await.unwrap();
    drop(journal);

    let raw = std::fs::read_to_string(&path).unwrap();
    let bare = raw
        .lines()
        .find(|line| line.contains("\"agent_session_id\":\"task-2\""))
        .expect("the bare TaskSpawned line");
    assert!(!bare.contains("worktree"), "{bare}");
    assert!(!bare.contains("label"), "{bare}");

    let session = Session::open(&path).unwrap();
    assert!(session.skipped().is_empty());
    let spawned: Vec<(String, Option<String>, Option<String>)> = session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::TaskSpawned {
                agent_session_id,
                worktree,
                label,
            } => Some((agent_session_id.clone(), worktree.clone(), label.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        spawned,
        vec![
            (
                "task-1".to_string(),
                Some("/work/example/.radian-worktrees/task-1".to_string()),
                Some("slice 1".to_string()),
            ),
            ("task-2".to_string(), None, None),
        ]
    );
    let finished: Vec<(String, TurnOutcome)> = session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::TaskFinished {
                agent_session_id,
                outcome,
            } => Some((agent_session_id.clone(), *outcome)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        vec![("task-1".to_string(), TurnOutcome::Completed)]
    );
    // Audit only: never a transcript record, so replay/recover see nothing.
    assert!(session.journal_records().is_empty());
    assert!(session.history().unwrap().is_empty());
}

#[tokio::test]
async fn the_new_method_respects_the_single_writer_lock() {
    let dir = TempDir::new("runtime-lock");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid", created()).unwrap();
    // A second writer cannot even open the file.
    match SessionJournal::resume(&path, "sid") {
        Err(WriteError::Locked { .. }) => {}
        Err(other) => panic!("expected Locked, got {other:?}"),
        Ok(_) => panic!("a second writer acquired the lock"),
    }
    journal
        .append_runtime_event(RuntimeEvent::TurnStarted {
            turn_id: "t".into(),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn the_new_method_truncates_a_torn_tail_like_every_other_line() {
    let dir = TempDir::new("runtime-torn");
    let path = dir.file("s.jsonl");
    {
        let journal = SessionJournal::create(&path, "sid", created()).unwrap();
        journal
            .append_runtime_event(RuntimeEvent::TurnStarted {
                turn_id: "t-1".into(),
            })
            .await
            .unwrap();
    }
    // A half-written line, as a kill would leave it.
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(br#"{"schema_version":1,"session_id":"sid","seq":2,"ts":"TORN"#)
            .unwrap();
    }
    let journal = SessionJournal::resume(&path, "sid").unwrap();
    assert_eq!(
        journal.next_seq(),
        2,
        "the torn line is truncated, seq continues"
    );
    journal
        .append_runtime_event(RuntimeEvent::TurnFinished {
            turn_id: "t-1".into(),
            outcome: TurnOutcome::Cancelled,
            reason: Some(TurnReason::Requested),
            stop_reason: None,
        })
        .await
        .unwrap();
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("TORN"), "the torn fragment is gone");
    let session = Session::open(&path).unwrap();
    assert_eq!(
        session.events().iter().map(|e| e.seq).collect::<Vec<_>>(),
        [0, 1, 2]
    );
}

#[tokio::test]
async fn the_index_includes_runtime_events_like_any_other_line() {
    let dir = TempDir::new("runtime-index");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid", created()).unwrap();
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("m"),
        })
        .await
        .unwrap();
    for event in all_events() {
        journal.append_runtime_event(event).await.unwrap();
    }

    let mut index = Index::open(dir.file("index.db")).unwrap();
    index.rebuild(std::slice::from_ref(&path)).unwrap();
    let rows = index.indexed_events("sid").unwrap();
    let names: Vec<String> = rows.iter().map(|r| r.event.clone()).collect();
    for expected in [
        "SessionCreated",
        "MessageAppended",
        "InboxEnqueued",
        "InputClaimed",
        "InputAdmitted",
        "TurnStarted",
        "TurnFinished",
        "ConfigChanged",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing {expected} in {names:?}"
        );
    }
    // The index derives everything from the envelope, so a runtime event indexes with the
    // same seq/offset as any other line.
    let inbox = rows.iter().find(|r| r.event == "InboxEnqueued").unwrap();
    assert_eq!(inbox.seq, 2);
    assert!(inbox.byte_offset > 0);
}

/// ADR 0006 §7(d): a known field with an unknown enum value maps to `Unknown`; it is not
/// corruption and does not stop the file from opening.
#[tokio::test]
async fn an_unknown_enum_value_in_a_runtime_event_maps_to_unknown() {
    let dir = TempDir::new("runtime-unknown-value");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid", created()).unwrap();
    drop(journal);
    // Hand-write a v2-style outcome the v1 reader does not know.
    let mut raw = std::fs::read_to_string(&path).unwrap();
    raw.push_str(
        r#"{"schema_version":1,"session_id":"sid","seq":1,"ts":"2026-10-12T00:00:00.000000Z","event":"TurnFinished","critical":true,"data":{"turn_id":"t","outcome":"future_outcome"}}"#,
    );
    raw.push('\n');
    std::fs::write(&path, raw).unwrap();

    let session = Session::open(&path).unwrap();
    assert!(session.skipped().is_empty());
    let outcome = session.events().iter().find_map(|event| match &event.data {
        EventData::TurnFinished { outcome, .. } => Some(*outcome),
        _ => None,
    });
    assert_eq!(outcome, Some(TurnOutcome::Unknown));
}

/// The runtime appends its events while the core is appending journal records, through the
/// same `SessionJournal`. Both paths must agree on one order: a line's `seq` has to be the
/// order it reaches the file, or the file is corrupt (ADR 0006 §3: `seq` contiguous, no
/// gaps, never backwards).
///
/// This is a regression test for a race found in review: `seq` used to be taken with an
/// atomic `fetch_add` *before* the writer lock, so two concurrent appends could take seqs
/// in one order and write them in the other. The driver's steering test hit it about once
/// in thirty runs. Here many writers hammer both paths at once, so the old code fails on
/// essentially every run.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_runtime_and_core_appends_keep_seq_in_file_order() {
    use std::sync::Arc;

    for round in 0..8 {
        let dir = TempDir::new("seq-race");
        let path = dir.file("s.jsonl");
        let journal = Arc::new(SessionJournal::create(&path, "sid-race", created()).unwrap());

        let mut tasks = Vec::new();
        for w in 0..8u64 {
            let journal = Arc::clone(&journal);
            tasks.push(tokio::spawn(async move {
                for i in 0..25u64 {
                    if (w + i) % 2 == 0 {
                        journal
                            .append_runtime_event(RuntimeEvent::TurnStarted {
                                turn_id: format!("w{w}-{i}"),
                            })
                            .await
                            .unwrap();
                    } else {
                        // Usage records carry no transcript index, so concurrent core-side
                        // appends are legal here and exercise the TranscriptJournal path
                        // without needing a declaring assistant message.
                        journal
                            .append(JournalRecord::ModelUsage {
                                usage: agent_core::ModelUsage::default(),
                            })
                            .await
                            .unwrap();
                    }
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        // `Session::open` enforces seq continuity line by line, so a reordering shows up as
        // `BadSeq` here rather than as a silently wrong file.
        let session = Session::open(&path)
            .unwrap_or_else(|e| panic!("round {round}: the file is not well-ordered: {e}"));
        assert_eq!(session.events().len(), 1 + 8 * 25, "round {round}");
        for (i, event) in session.events().iter().enumerate() {
            assert_eq!(event.seq, i as u64, "round {round}");
        }
        assert_eq!(journal.next_seq(), 1 + 8 * 25, "round {round}");
    }
}
