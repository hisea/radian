//! ADR 0006 §5.9: `PermissionResolved` is written and read back typed, is `critical =
//! false`, and never enters `journal_records`/`replay`.

mod common;

use radian_core::{JournalRecord, Message, TranscriptJournal, recover, replay};
use common::{TempDir, created};
use radian_session::{
    ApprovalPolicyValue, EventData, PermissionApprover, PermissionDecision, RuntimeEvent, Session,
    SessionJournal,
};
use serde_json::json;

#[tokio::test]
async fn permission_resolved_round_trips_and_is_not_replayed() {
    let dir = TempDir::new("perm-event");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid-perm", created()).unwrap();
    journal
        .append_runtime_event(RuntimeEvent::permission_resolved(
            "req-1".into(),
            "call-1".into(),
            PermissionDecision::Deny,
            PermissionApprover::Policy,
            Some("no".into()),
            Some("safe-writes".into()),
        ))
        .await
        .unwrap();
    // A second decision with no policy command: built-in rules and `auto` never name one, so
    // the wire must omit the field entirely (not write `null`).
    journal
        .append_runtime_event(RuntimeEvent::permission_resolved(
            "req-2".into(),
            "call-2".into(),
            PermissionDecision::Allow,
            PermissionApprover::Policy,
            None,
            None,
        ))
        .await
        .unwrap();
    // A transcript message after it, so a round trip that reordered anything would show.
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("one"),
        })
        .await
        .unwrap();
    drop(journal);

    let session = Session::open(&path).unwrap();
    let event = session
        .events()
        .iter()
        .find(|event| event.event == "PermissionResolved")
        .expect("the event was accepted");
    assert!(!event.critical, "PermissionResolved is audit, not critical");
    match &event.data {
        EventData::PermissionResolved {
            request_id,
            call_id,
            decision,
            approver,
            reason,
            policy,
        } => {
            assert_eq!(request_id, "req-1");
            assert_eq!(call_id, "call-1");
            assert_eq!(*decision, PermissionDecision::Deny);
            assert_eq!(*approver, PermissionApprover::Policy);
            assert_eq!(reason.as_deref(), Some("no"));
            assert_eq!(policy.as_deref(), Some("safe-writes"));
        }
        other => panic!("expected a typed PermissionResolved, got {other:?}"),
    }

    // The wire carries `policy` only when a policy command decided: present on the first line
    // and absent on the second (not `"policy":null`). `approver` is `"policy"` on both, so the
    // check is on the `data` key, not the substring.
    let raw = std::fs::read_to_string(&path).unwrap();
    let resolved: Vec<serde_json::Value> = raw
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|value| value["event"] == "PermissionResolved")
        .collect();
    assert_eq!(resolved.len(), 2, "{raw}");
    assert_eq!(resolved[0]["data"]["policy"], json!("safe-writes"));
    assert!(
        resolved[1]["data"].get("policy").is_none(),
        "a decision with no policy command must omit the policy key: {}",
        resolved[1]
    );

    // It is audit, not a transcript record: replay/recover see only the message.
    let records = session.journal_records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert!(matches!(
        &records[0],
        JournalRecord::Message { index: 0, .. }
    ));
    assert_eq!(replay(&records).unwrap(), vec![Message::user("one")]);
    assert_eq!(recover(&records).unwrap(), vec![Message::user("one")]);
    assert_eq!(session.history().unwrap(), vec![Message::user("one")]);
}

/// ADR 0026 D9: `ApprovalPolicyChanged` is the audit record of an in-session switch. It
/// round-trips typed, is `critical = false`, and never enters the transcript fold.
#[tokio::test]
async fn approval_policy_changed_round_trips_and_is_not_replayed() {
    let dir = TempDir::new("approval-policy-event");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid-approval", created()).unwrap();
    let event = RuntimeEvent::ApprovalPolicyChanged {
        policy: ApprovalPolicyValue::Auto,
    };
    assert_eq!(event.name(), "ApprovalPolicyChanged");
    assert!(!event.critical(), "the switch is audit only");
    journal.append_runtime_event(event).await.unwrap();
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("one"),
        })
        .await
        .unwrap();
    drop(journal);

    let session = Session::open(&path).unwrap();
    let event = session
        .events()
        .iter()
        .find(|event| event.event == "ApprovalPolicyChanged")
        .expect("the event was accepted");
    assert!(!event.critical);
    match &event.data {
        EventData::ApprovalPolicyChanged { policy } => {
            assert_eq!(*policy, ApprovalPolicyValue::Auto);
        }
        other => panic!("expected a typed ApprovalPolicyChanged, got {other:?}"),
    }
    // Audit only: never a transcript record.
    let records = session.journal_records();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(session.history().unwrap(), vec![Message::user("one")]);
}
