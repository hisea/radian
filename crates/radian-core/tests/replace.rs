//! Host transcript replacement: journal reset semantics, failure boundaries and session state.

mod common;

use radian_core::testing::{FailingJournal, RecordingJournal};
use radian_core::*;
use common::*;
use serde_json::json;
use std::sync::Arc;

async fn journaled_session(
    model: Arc<ScriptedModel>,
    history: Vec<Message>,
    journal: Arc<dyn TranscriptJournal>,
) -> AgentSession<Arc<ScriptedModel>, NoTools, ()> {
    agent(model, NoTools)
        .open_session_with_journal(history, journal)
        .await
        .unwrap()
}

fn assistant_call(id: &str) -> Message {
    Message::Assistant {
        reasoning: Vec::new(),
        content: Vec::new(),
        tool_calls: vec![call(id, "missing", json!({}))],
        provider_payload: None,
        interrupted: false,
        meta: MessageMeta::default(),
    }
}

#[tokio::test]
async fn replacing_the_transcript_journals_from_zero_emits_one_event_and_continues() {
    let model = ScriptedModel::new([text("next")]);
    let journal = Arc::new(RecordingJournal::new());
    let mut session = journaled_session(model.clone(), Vec::new(), journal.clone()).await;
    let replacement = vec![Message::user("new start"), Message::assistant("old reply")];
    let events = Recorder::default();

    session
        .replace_transcript(replacement.clone(), 17, &events)
        .await
        .unwrap();

    assert_eq!(session.transcript(), replacement);
    assert_eq!(
        journal.records(),
        vec![
            JournalRecord::TranscriptReplaced {
                revision: 17,
                len: 2,
            },
            JournalRecord::Message {
                index: 0,
                message: replacement[0].clone(),
            },
            JournalRecord::Message {
                index: 1,
                message: replacement[1].clone(),
            },
            JournalRecord::TranscriptReplacementCommitted {
                revision: 17,
                len: 2,
            },
        ]
    );
    assert_eq!(
        events.events(),
        vec![AgentEvent::TranscriptReplaced {
            revision: 17,
            len: 2,
        }]
    );

    session
        .run_turn("continue", TurnOptions::default(), &events)
        .await
        .unwrap();
    assert_eq!(&model.requests()[0].messages[..2], replacement.as_slice());
}

#[tokio::test]
async fn replacement_obeys_the_journal_selfcheck() {
    let mut session = agent(ScriptedModel::new([]), NoTools)
        .open_session()
        .await
        .unwrap();
    session
        .replace_transcript(vec![Message::user("replacement")], 3, &NoopSink)
        .await
        .unwrap();
    session.close().await.unwrap();
}

#[tokio::test]
async fn replay_and_recover_reset_at_replacement_and_repair_the_new_revision() {
    let replacement = vec![Message::user("new"), assistant_call("new-call")];
    let records = vec![
        JournalRecord::Message {
            index: 0,
            message: Message::user("old"),
        },
        // This old revision is intentionally incomplete; it must disappear at replacement.
        JournalRecord::Message {
            index: 1,
            message: assistant_call("old-call"),
        },
        JournalRecord::ToolExecutionStarted {
            call_id: "old-call".into(),
            name: "missing".into(),
            arguments: json!({}),
        },
        JournalRecord::TranscriptReplaced {
            revision: 9,
            len: replacement.len(),
        },
        JournalRecord::Message {
            index: 0,
            message: replacement[0].clone(),
        },
        JournalRecord::Message {
            index: 1,
            message: replacement[1].clone(),
        },
        JournalRecord::TranscriptReplacementCommitted {
            revision: 9,
            len: replacement.len(),
        },
        JournalRecord::ToolExecutionStarted {
            call_id: "new-call".into(),
            name: "missing".into(),
            arguments: json!({}),
        },
    ];

    assert_eq!(replay(&records).unwrap(), replacement);
    let recovered = recover(&records).unwrap();
    assert_eq!(&recovered[..2], replacement.as_slice());
    assert_eq!(
        match &recovered[2] {
            Message::Tool { result, .. } => result.error_code.as_deref(),
            other => panic!("expected repaired tool result, got {other:?}"),
        },
        Some("outcome_unknown_after_crash")
    );
    agent(ScriptedModel::new([]), NoTools)
        .open_session_with_history(recovered)
        .await
        .unwrap();
}

#[tokio::test]
async fn invalid_replacements_are_rejected_before_the_journal() {
    let journal = Arc::new(RecordingJournal::new());
    let original = vec![Message::user("original")];
    let mut session =
        journaled_session(ScriptedModel::new([]), original.clone(), journal.clone()).await;
    let events = Recorder::default();
    let invalid = vec![Message::user("new"), assistant_call("unanswered")];

    assert!(matches!(
        session.replace_transcript(invalid, 1, &events).await,
        Err(ReplaceError::InvalidHistory(_))
    ));
    assert_eq!(session.transcript(), original);
    assert!(journal.records().is_empty());
    assert!(events.events().is_empty());

    let duplicate = vec![Message::Assistant {
        reasoning: Vec::new(),
        content: Vec::new(),
        tool_calls: vec![
            call("same", "missing", json!({})),
            call("same", "missing", json!({})),
        ],
        provider_payload: None,
        interrupted: false,
        meta: MessageMeta::default(),
    }];
    assert!(matches!(
        session.replace_transcript(duplicate, 2, &events).await,
        Err(ReplaceError::InvalidHistory(_))
    ));
    assert_eq!(session.transcript(), original);
    assert!(journal.records().is_empty());
}

#[tokio::test]
async fn a_failed_replacement_marker_keeps_memory_and_requires_recovery() {
    let journal = Arc::new(FailingJournal::fail_at(1));
    let original = vec![Message::user("original")];
    let mut session =
        journaled_session(ScriptedModel::new([]), original.clone(), journal.clone()).await;

    assert!(matches!(
        session
            .replace_transcript(vec![Message::user("new")], 1, &NoopSink)
            .await,
        Err(ReplaceError::Journal(_))
    ));
    assert_eq!(session.transcript(), original);
    assert!(journal.records().is_empty());
    assert!(matches!(
        session.start_turn("later"),
        Err(SessionError::RecoveryRequired)
    ));
}

#[tokio::test]
async fn a_failed_replacement_message_keeps_memory_and_requires_recovery() {
    // Marker and message zero land; message one fails. The all-before-swap implementation
    // leaves live memory as the old transcript, while the journal is intentionally recoverable.
    let journal = Arc::new(FailingJournal::fail_at(3));
    let original = vec![Message::user("original")];
    let replacement = vec![Message::user("new"), Message::assistant("reply")];
    let mut session =
        journaled_session(ScriptedModel::new([]), original.clone(), journal.clone()).await;

    assert!(matches!(
        session.replace_transcript(replacement, 1, &NoopSink).await,
        Err(ReplaceError::Journal(_))
    ));
    assert_eq!(session.transcript(), original);
    assert!(matches!(
        journal.records().as_slice(),
        [
            JournalRecord::TranscriptReplaced {
                revision: 1,
                len: 2
            },
            JournalRecord::Message { index: 0, .. }
        ]
    ));
    assert!(matches!(
        session.start_turn("later"),
        Err(SessionError::RecoveryRequired)
    ));
}

#[tokio::test]
async fn unusable_sessions_reject_replacement_without_journaling() {
    let events = Recorder::default();

    let poisoned_journal = Arc::new(RecordingJournal::new());
    let poisoned_model = ScriptedModel::new([Reply::Hang]);
    let mut poisoned =
        journaled_session(poisoned_model.clone(), Vec::new(), poisoned_journal.clone()).await;
    let mut run = poisoned.start_turn("go").unwrap();
    let mut step = Box::pin(run.step(&NoopSink));
    // `requests` is the model's state gate: only drop once the step has reached its hang.
    while poisoned_model.requests().is_empty() {
        tokio::select! {
            result = &mut step => panic!("the hanging step unexpectedly finished: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
    }
    drop(step);
    drop(run);
    let poisoned_before = poisoned.transcript().to_vec();
    assert!(poisoned.is_poisoned());
    assert!(matches!(
        poisoned
            .replace_transcript(vec![Message::user("new")], 1, &events)
            .await,
        Err(ReplaceError::Unusable)
    ));
    assert_eq!(poisoned.transcript(), poisoned_before);
    // The pending turn's input is the only journal write; replacement adds none.
    assert_eq!(poisoned_journal.calls(), 1);

    let closed_journal = Arc::new(RecordingJournal::new());
    let mut closed =
        journaled_session(ScriptedModel::new([]), Vec::new(), closed_journal.clone()).await;
    closed.close().await.unwrap();
    let closed_before = closed.transcript().to_vec();
    assert!(matches!(
        closed
            .replace_transcript(vec![Message::user("new")], 1, &events)
            .await,
        Err(ReplaceError::Unusable)
    ));
    assert_eq!(closed.transcript(), closed_before);
    assert!(closed_journal.records().is_empty());

    let recovery_journal = Arc::new(FailingJournal::fail_at(1));
    let mut recovery =
        journaled_session(ScriptedModel::new([]), Vec::new(), recovery_journal.clone()).await;
    assert!(
        recovery
            .replace_transcript(vec![Message::user("first")], 1, &events)
            .await
            .is_err()
    );
    let recovery_before = recovery.transcript().to_vec();
    assert!(matches!(
        recovery
            .replace_transcript(vec![Message::user("new")], 2, &events)
            .await,
        Err(ReplaceError::Unusable)
    ));
    assert_eq!(recovery.transcript(), recovery_before);
    assert_eq!(recovery_journal.calls(), 1);
    assert!(events.events().is_empty());
}

/// A marker written before a replacement describes a transcript that no longer exists. If the
/// new revision happens to reuse a call id, the stale outcome must not answer the new call:
/// that would invent a result for work this revision never dispatched.
#[tokio::test]
async fn markers_from_before_a_replacement_never_repair_a_reused_call_id() {
    let replacement = vec![Message::user("new"), assistant_call("shared")];
    let stale = ToolResult::success("shared", vec![ContentPart::text("from the old revision")]);
    let records = vec![
        JournalRecord::Message {
            index: 0,
            message: assistant_call("shared"),
        },
        JournalRecord::ToolExecutionStarted {
            call_id: "shared".into(),
            name: "missing".into(),
            arguments: json!({}),
        },
        JournalRecord::ToolExecutionSettled {
            call_id: "shared".into(),
            result: stale.clone(),
        },
        JournalRecord::TranscriptReplaced {
            revision: 2,
            len: replacement.len(),
        },
        JournalRecord::Message {
            index: 0,
            message: replacement[0].clone(),
        },
        JournalRecord::Message {
            index: 1,
            message: replacement[1].clone(),
        },
        JournalRecord::TranscriptReplacementCommitted {
            revision: 2,
            len: replacement.len(),
        },
    ];

    let recovered = recover(&records).unwrap();

    assert_eq!(&recovered[..2], replacement.as_slice());
    let Message::Tool { result, .. } = &recovered[2] else {
        panic!("the unpaired call must be answered: {recovered:?}");
    };
    // Neither the stale result nor "outcome unknown": this revision never started the call.
    assert_eq!(
        result.error_code.as_deref(),
        Some("not_started_after_crash")
    );
    assert_ne!(result.content, stale.content);
}

// ---------- ADR 0019 §2/§3: the pending segment and its commit/abort ----------

#[test]
fn a_pending_segment_at_end_of_file_recovers_the_old_history() {
    let old = vec![Message::user("old")];
    let records = vec![
        JournalRecord::Message {
            index: 0,
            message: old[0].clone(),
        },
        JournalRecord::TranscriptReplaced {
            revision: 1,
            len: 2,
        },
        JournalRecord::Message {
            index: 0,
            message: Message::user("pending zero"),
        },
        JournalRecord::Message {
            index: 1,
            message: Message::user("pending one"),
        },
    ];
    assert_eq!(replay(&records).unwrap(), old);
}

#[test]
fn an_aborted_segment_recovers_the_old_history_and_the_next_append_is_normal() {
    let old = vec![Message::user("old zero"), Message::user("old one")];
    let records = vec![
        JournalRecord::Message {
            index: 0,
            message: old[0].clone(),
        },
        JournalRecord::Message {
            index: 1,
            message: old[1].clone(),
        },
        JournalRecord::TranscriptReplaced {
            revision: 7,
            len: 4,
        },
        JournalRecord::Message {
            index: 0,
            message: Message::user("pending zero"),
        },
        JournalRecord::TranscriptReplacementAborted { revision: 7 },
        // Index 2 continues the old history (length 2), not the dropped segment.
        JournalRecord::Message {
            index: 2,
            message: Message::user("normal two"),
        },
    ];
    let mut expected = old;
    expected.push(Message::user("normal two"));
    assert_eq!(replay(&records).unwrap(), expected);
}

/// An abort and an open segment at end of file fold to the **same** history and the same
/// declared calls: the abort only records the segment in `uncommitted`, it never touches the
/// history. The host relies on this (a resume that writes the abort may reuse the parse taken
/// before it), so this test makes the assumption explicit: if the abort ever starts changing
/// the fold, it fails here first.
#[test]
fn an_aborted_segment_and_one_at_end_of_file_fold_identically() {
    let fold = |abort: bool| {
        let mut fold = TranscriptFold::new();
        // A committed assistant call: the abort must not clear its declared marker.
        fold.push(&JournalRecord::Message {
            index: 0,
            message: assistant_call("old-call"),
        })
        .unwrap();
        fold.push(&JournalRecord::TranscriptReplaced {
            revision: 5,
            len: 2,
        })
        .unwrap();
        fold.push(&JournalRecord::Message {
            index: 0,
            message: assistant_call("pending-call"),
        })
        .unwrap();
        if abort {
            fold.push(&JournalRecord::TranscriptReplacementAborted { revision: 5 })
                .unwrap();
        }
        fold.finish()
    };
    let aborted = fold(true);
    let at_end_of_file = fold(false);
    assert_eq!(aborted.history(), at_end_of_file.history());
    assert_eq!(aborted.history(), &[assistant_call("old-call")]);
    for call in ["old-call", "pending-call"] {
        assert_eq!(
            aborted.is_call_declared(call),
            at_end_of_file.is_call_declared(call),
            "{call}"
        );
    }
    assert!(aborted.is_call_declared("old-call"));
    assert!(!aborted.is_call_declared("pending-call"));
}

#[test]
fn a_committed_segment_recovers_the_new_history() {
    let records = vec![
        JournalRecord::Message {
            index: 0,
            message: Message::user("old"),
        },
        JournalRecord::TranscriptReplaced {
            revision: 2,
            len: 1,
        },
        JournalRecord::Message {
            index: 0,
            message: Message::user("new"),
        },
        JournalRecord::TranscriptReplacementCommitted {
            revision: 2,
            len: 1,
        },
    ];
    assert_eq!(replay(&records).unwrap(), vec![Message::user("new")]);
}

#[test]
fn a_commit_without_a_segment_is_a_replay_error() {
    let records = vec![JournalRecord::TranscriptReplacementCommitted {
        revision: 1,
        len: 0,
    }];
    assert!(matches!(
        replay(&records),
        Err(ReplayError::Replacement { .. })
    ));
}

#[test]
fn a_second_marker_inside_a_pending_segment_is_a_replay_error() {
    let records = vec![
        JournalRecord::TranscriptReplaced {
            revision: 1,
            len: 1,
        },
        JournalRecord::TranscriptReplaced {
            revision: 2,
            len: 1,
        },
    ];
    assert!(matches!(
        replay(&records),
        Err(ReplayError::Replacement { .. })
    ));
}

#[test]
fn a_mismatched_commit_is_a_replay_error() {
    let records = vec![
        JournalRecord::TranscriptReplaced {
            revision: 1,
            len: 1,
        },
        JournalRecord::Message {
            index: 0,
            message: Message::user("a"),
        },
        JournalRecord::TranscriptReplacementCommitted {
            revision: 2,
            len: 1,
        },
    ];
    assert!(matches!(
        replay(&records),
        Err(ReplayError::Replacement { .. })
    ));
}

/// ADR 0019 §3 row 5: a failed commit append leaves memory as the old history and requires
/// recovery. The marker and the messages landed; nothing is patched after the failure.
#[tokio::test]
async fn a_failed_commit_append_keeps_memory_and_requires_recovery() {
    // marker = 1, message 0 = 2, commit = 3.
    let journal = Arc::new(FailingJournal::fail_at(3));
    let original = vec![Message::user("original")];
    let mut session =
        journaled_session(ScriptedModel::new([]), original.clone(), journal.clone()).await;

    assert!(matches!(
        session
            .replace_transcript(vec![Message::user("new")], 1, &NoopSink)
            .await,
        Err(ReplaceError::Journal(_))
    ));
    assert_eq!(session.transcript(), original);
    assert!(matches!(
        journal.records().as_slice(),
        [
            JournalRecord::TranscriptReplaced {
                revision: 1,
                len: 1
            },
            JournalRecord::Message { index: 0, .. }
        ]
    ));
    assert!(matches!(
        session.start_turn("later"),
        Err(SessionError::RecoveryRequired)
    ));
}
