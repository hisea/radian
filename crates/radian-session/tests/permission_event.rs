//! ADR 0006 §5.9: `PermissionResolved` is written and read back typed, is `critical =
//! false`, and never enters `journal_records`/`replay`.

mod common;

use agent_core::{JournalRecord, Message, TranscriptJournal, recover, replay};
use common::{TempDir, created};
use radian_session::{
    EventData, PermissionApprover, PermissionDecision, RuntimeEvent, Session, SessionJournal,
};

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
        } => {
            assert_eq!(request_id, "req-1");
            assert_eq!(call_id, "call-1");
            assert_eq!(*decision, PermissionDecision::Deny);
            assert_eq!(*approver, PermissionApprover::Policy);
            assert_eq!(reason.as_deref(), Some("no"));
        }
        other => panic!("expected a typed PermissionResolved, got {other:?}"),
    }

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
