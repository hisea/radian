//! ADR 0006 §15-2/4/5/7: the reader must process every repository fixture exactly as
//! `fixtures/README.md` declares, enforce `MessageAppended.index` continuity, and refuse
//! a newer `schema_version` without ever writing.

mod common;

use agent_core::{InputSource, Message, recover};
use common::{TempDir, fixture};
use radian_session::{
    EventData, InboxLane, PermissionApprover, PermissionDecision, ReadError, ReplacementEnd,
    Session, SessionJournal, TurnOutcome, TurnReason, WriteError,
};
use serde_json::json;

#[test]
fn v1_minimal_is_accepted_with_a_contiguous_transcript() {
    let session = Session::open(fixture("v1-minimal.jsonl")).unwrap();
    assert_eq!(session.session_id(), "11111111-1111-4111-8111-111111111111");
    assert_eq!(session.events().len(), 9);
    assert!(session.skipped().is_empty());

    // §15-7: the folded transcript equals the messages we expect from the file, without
    // relying on a second copy of the file.
    let history = session.history().unwrap();
    assert_eq!(history.len(), 4);
    assert_eq!(
        history[0],
        Message::user("Read /work/example/a.txt and fix the typo.")
    );
    assert!(matches!(&history[1], Message::Assistant { tool_calls, .. }
        if tool_calls.len() == 1 && tool_calls[0].id == "call-1"));
    assert!(matches!(&history[2], Message::Tool { result, .. } if result.call_id == "call-1"));
    assert_eq!(history[3], Message::assistant("Fixed: wolrd -> world."));

    // And the fold is exactly `recover` over the events mapped back to core records.
    assert_eq!(history, recover(&session.journal_records()).unwrap());
}

#[test]
fn v1_forward_skips_the_unknown_event_and_ignores_reserved_fields() {
    let session = Session::open(fixture("v1-forward.jsonl")).unwrap();
    assert_eq!(session.skipped(), &[(3, "HeartbeatRecorded".to_string())]);
    assert_eq!(session.events().len(), 5);
    assert_eq!(session.history().unwrap().len(), 2);

    let EventData::Other(data) = &session.events()[0].data else {
        panic!("SessionCreated is stored opaquely in this slice");
    };
    for reserved in ["agent_id", "profile_ref", "workspace_binding", "parent"] {
        assert!(
            data.get(reserved).is_some(),
            "missing reserved field {reserved}"
        );
    }
}

#[test]
fn unknown_optional_fields_are_ignored() {
    let session = Session::open(fixture("unknown-optional-field.jsonl")).unwrap();
    assert!(session.skipped().is_empty());
    assert_eq!(session.events().len(), 4);
    assert_eq!(session.history().unwrap(), [Message::user("hi")]);
}

#[test]
fn an_unknown_noncritical_event_is_skipped_and_reported() {
    let session = Session::open(fixture("unknown-event-noncritical.jsonl")).unwrap();
    assert_eq!(
        session.skipped(),
        &[(1, "SpinProgressRecorded".to_string())]
    );
    assert_eq!(session.events().len(), 4);
}

#[test]
fn an_unknown_critical_event_refuses_the_file_with_line_and_event() {
    let err = Session::open(fixture("unknown-event-critical.jsonl")).unwrap_err();
    let ReadError::UnknownCritical { line, event, .. } = &err else {
        panic!("expected UnknownCritical, got {err:?}");
    };
    assert_eq!(*line, 2, "the 1-based line must be reported");
    assert_eq!(event, "LedgerCommitted");
    let text = err.to_string();
    assert!(text.contains("line 2"), "{text}");
    assert!(text.contains("LedgerCommitted"), "{text}");
    assert!(text.contains("critical"), "{text}");
}

/// ADR 0006 §7(d): a known field carrying an unknown enum value is mapped to `Unknown`, not
/// a parse error. A v1 reader must still open a file written by a newer v1-compatible writer.
#[test]
fn unknown_enum_values_are_mapped_to_unknown_not_refused() {
    let session = Session::open(fixture("unknown-enum-values.jsonl")).unwrap();
    assert!(session.skipped().is_empty(), "no line may be skipped");
    assert_eq!(session.events().len(), 6);

    // InboxEnqueued.lane: a future lane maps to Unknown.
    let lane = session.events().iter().find_map(|event| match &event.data {
        EventData::InboxEnqueued { lane, .. } => Some(*lane),
        _ => None,
    });
    assert_eq!(lane, Some(InboxLane::Unknown));

    // TurnFinished.outcome / reason: future values map to Unknown.
    let outcomes: Vec<(TurnOutcome, Option<TurnReason>)> = session
        .events()
        .iter()
        .filter_map(|event| match &event.data {
            EventData::TurnFinished {
                outcome, reason, ..
            } => Some((*outcome, reason.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        outcomes,
        vec![
            (TurnOutcome::Unknown, None),
            (TurnOutcome::Cancelled, Some(TurnReason::Unknown)),
        ]
    );

    // PermissionResolved.decision / approver: future values map to Unknown.
    let permission = session.events().iter().find_map(|event| match &event.data {
        EventData::PermissionResolved {
            decision, approver, ..
        } => Some((*decision, *approver)),
        _ => None,
    });
    assert_eq!(
        permission,
        Some((PermissionDecision::Unknown, PermissionApprover::Unknown))
    );

    // None of these events is a transcript record, so replay/recover is unaffected.
    assert!(session.history().unwrap().is_empty());
    assert!(session.journal_records().is_empty());
}

#[test]
fn a_newer_schema_version_is_refused_and_never_written() {
    let path = fixture("schema-version-newer.jsonl");
    let before = std::fs::read(&path).unwrap();

    let err = Session::open(&path).unwrap_err();
    assert!(
        matches!(
            err,
            ReadError::SchemaTooNew {
                found: 2,
                supported: 1,
                ..
            }
        ),
        "{err:?}"
    );

    // The write path must refuse as well, and touch nothing.
    match SessionJournal::resume(&path, "11111111-1111-4111-8111-111111111111") {
        Err(WriteError::Policy(e)) => assert!(matches!(e, ReadError::SchemaTooNew { .. }), "{e:?}"),
        Err(other) => panic!("expected a policy refusal, got {other:?}"),
        Ok(_) => panic!("resume must refuse a newer schema version"),
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "the file must be untouched"
    );
}

#[test]
fn a_torn_tail_is_dropped_at_read_time() {
    let session = Session::open(fixture("truncated-tail.jsonl")).unwrap();
    let seqs: Vec<u64> = session.events().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [0, 1, 2, 3, 4]);
}

#[test]
fn crash_after_started_repairs_the_unsettled_call() {
    let session = Session::open(fixture("crash-after-started.jsonl")).unwrap();
    assert_eq!(session.events().len(), 8);

    let history = session.history().unwrap();
    // call-1 keeps its real result; call-2, which only started, gets the ADR 0002 code.
    let call_1 = history
        .iter()
        .find_map(|m| match m {
            Message::Tool { result, .. } if result.call_id == "call-1" => Some(result),
            _ => None,
        })
        .unwrap();
    assert_eq!(call_1.text(), "check a passed");
    let call_2 = history
        .iter()
        .find_map(|m| match m {
            Message::Tool { result, .. } if result.call_id == "call-2" => Some(result),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        call_2.error_code.as_deref(),
        Some("outcome_unknown_after_crash")
    );
}

#[test]
fn replace_resets_the_index_and_repairs_the_new_revision_only() {
    let session = Session::open(fixture("replace-resets-index.jsonl")).unwrap();
    assert_eq!(session.events().len(), 11);
    assert!(session.skipped().is_empty());

    // The two messages before the replacement are gone from the transcript.
    let history = session.history().unwrap();
    assert_eq!(history[0].text(), "sync from scheduler");
    match &history[0] {
        Message::User { meta, .. } => {
            assert_eq!(meta.id.as_ref().map(|m| m.0.as_str()), Some("sync-1"));
            assert_eq!(meta.source, Some(InputSource::Scheduler));
            assert!(meta.origin.is_some());
        }
        other => panic!("expected the replacement user message, got {other:?}"),
    }
    assert_eq!(
        history[1],
        Message::Assistant {
            reasoning: Vec::new(),
            content: vec![],
            tool_calls: vec![agent_core::ToolCall {
                id: "call-9".into(),
                name: "check_a".into(),
                arguments: json!({}),
            }],
            provider_payload: None,
            interrupted: false,
            meta: Default::default(),
        }
    );
    assert_eq!(history.len(), 3, "call-9 gets one repaired result");
    // call-9 has a Started marker, so its outcome is unknown, not "not started".
    assert!(matches!(&history[2], Message::Tool { result, .. }
        if result.error_code.as_deref() == Some("outcome_unknown_after_crash")));
}

#[test]
fn a_message_index_gap_is_corruption() {
    let dir = TempDir::new("index-gap");
    let path = dir.file("bad.jsonl");
    let head = r#"{"schema_version":1,"session_id":"sid","seq":0,"ts":"2026-10-12T00:00:00.000000Z","event":"MessageAppended","critical":true,"data":{"index":0,"message":{"user":{"content":[{"text":{"text":"hi"}}]}}}}"#;
    let gap = r#"{"schema_version":1,"session_id":"sid","seq":1,"ts":"2026-10-12T00:00:01.000000Z","event":"MessageAppended","critical":true,"data":{"index":2,"message":{"assistant":{"content":[]}}}}"#;
    std::fs::write(&path, format!("{head}\n{gap}\n")).unwrap();

    let err = Session::open(&path).unwrap_err();
    let ReadError::BadIndex {
        line,
        expected,
        found,
        ..
    } = &err
    else {
        panic!("expected BadIndex, got {err:?}");
    };
    assert_eq!((*line, *expected, *found), (2, 1, 2));
}

#[test]
fn a_schema_version_change_mid_file_is_corruption() {
    let dir = TempDir::new("version-mid");
    let path = dir.file("bad.jsonl");
    let a = r#"{"schema_version":1,"session_id":"sid","seq":0,"ts":"2026-10-12T00:00:00.000000Z","event":"MessageAppended","critical":true,"data":{"index":0,"message":{"user":{"content":[]}}}}"#;
    let b = r#"{"schema_version":2,"session_id":"sid","seq":1,"ts":"2026-10-12T00:00:01.000000Z","event":"MessageAppended","critical":true,"data":{"index":1,"message":{"user":{"content":[]}}}}"#;
    std::fs::write(&path, format!("{a}\n{b}\n")).unwrap();
    let err = Session::open(&path).unwrap_err();
    assert!(
        matches!(err, ReadError::Envelope { line: 2, .. }),
        "{err:?}"
    );
}

#[test]
fn a_bad_json_line_names_its_line_number() {
    let dir = TempDir::new("bad-json");
    let path = dir.file("bad.jsonl");
    let ok = r#"{"schema_version":1,"session_id":"sid","seq":0,"ts":"2026-10-12T00:00:00.000000Z","event":"MessageAppended","critical":true,"data":{"index":0,"message":{"user":{"content":[]}}}}"#;
    std::fs::write(&path, format!("{ok}\n{{not json}}\n")).unwrap();
    let err = Session::open(&path).unwrap_err();
    let ReadError::Corrupt { line, .. } = err else {
        panic!("expected Corrupt");
    };
    assert_eq!(line, 2);
}

/// ADR 0019 fixtures: the reader accepts a committed replacement and a file that ends inside
/// a pending segment (diagnosing it), and rejects the two corrupt shapes. This is the Rust
/// half of the differential check with `docs/schema/check-fixtures.sh`.
#[test]
fn the_replacement_fixtures_agree_with_the_reference_checker() {
    let committed = Session::open(fixture("replace-committed.jsonl")).unwrap();
    assert_eq!(
        committed.history().unwrap(),
        vec![
            Message::user("one"),
            Message::assistant("two"),
            Message::user("after"),
        ]
    );
    assert!(committed.uncommitted_replacements().is_empty());

    let pending = Session::open(fixture("replace-uncommitted-eof.jsonl")).unwrap();
    assert_eq!(pending.history().unwrap(), vec![Message::user("old")]);
    assert_eq!(
        pending.uncommitted_replacements(),
        &[radian_session::UncommittedReplacement {
            revision: 1,
            declared_len: 2,
            written: 1,
            line: 3,
            end: ReplacementEnd::EndOfFile,
        }]
    );

    let aborted = Session::open(fixture("replace-aborted-then-append.jsonl")).unwrap();
    assert_eq!(
        aborted.history().unwrap(),
        vec![Message::user("old"), Message::user("after")]
    );
    assert_eq!(
        aborted.uncommitted_replacements()[0].end,
        ReplacementEnd::Aborted
    );

    let other = Session::open(fixture("replace-pending-other-event.jsonl")).unwrap_err();
    assert!(
        matches!(other, ReadError::Corrupt { line: 3, .. }),
        "{other:?}"
    );
    let mismatch = Session::open(fixture("replace-commit-mismatch.jsonl")).unwrap_err();
    assert!(
        matches!(mismatch, ReadError::Corrupt { line: 4, .. }),
        "{mismatch:?}"
    );
}

/// ADR 0018 fixtures: the reader parses the compaction usage, the candidate and the committed
/// replacement; a candidate with no commit leaves the old history. This is the Rust half of the
/// differential check with `docs/schema/check-fixtures.sh`.
#[test]
fn the_compaction_fixtures_agree_with_the_reference_checker() {
    let committed = Session::open(fixture("compaction-committed.jsonl")).unwrap();
    assert_eq!(committed.last_committed_revision(), 1);
    let history = committed.history().unwrap();
    assert_eq!(history.len(), 2, "{history:?}");
    assert!(agent_core::compaction::is_compaction_summary(&history[0]));
    assert_eq!(history[1].text(), "old two");

    // The candidate-only file never replaced anything: the old history stands.
    let candidate = Session::open(fixture("compaction-candidate-only.jsonl")).unwrap();
    assert_eq!(candidate.last_committed_revision(), 0);
    assert_eq!(candidate.history().unwrap(), vec![Message::user("old")]);
    assert!(candidate.uncommitted_replacements().is_empty());
}
