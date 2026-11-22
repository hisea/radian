//! A journal written while the `HostNotice` event existed still opens and resumes. The event was
//! removed (T-0007); it was `critical: false`, so the reader skips it as an **unknown
//! non-critical** event (ADR 0006 §7(d)), reports it in `Session::skipped`, and the transcript
//! fold is unchanged. The line is kept here verbatim as the pinned fixture.

mod common;

use common::TempDir;
use radian_core::Message;
use radian_session::Session;

const OLD_JOURNAL: &str = concat!(
    r#"{"schema_version":1,"session_id":"17111111-1111-4111-8111-111111111111","seq":0,"ts":"2026-11-04T09:00:00.000000Z","event":"SessionCreated","critical":true,"data":{"workspace":"/work/example","config_snapshot":{"model":{"provider":"example","model":"m-1"}}}}"#,
    "\n",
    r#"{"schema_version":1,"session_id":"17111111-1111-4111-8111-111111111111","seq":1,"ts":"2026-11-04T09:00:01.000000Z","event":"MessageAppended","critical":true,"data":{"index":0,"message":{"user":{"content":[{"text":{"text":"hello"}}]}}}}"#,
    "\n",
    r#"{"schema_version":1,"session_id":"17111111-1111-4111-8111-111111111111","seq":2,"ts":"2026-11-04T09:00:02.000000Z","event":"HostNotice","critical":false,"data":{"text":"session closed remotely"}}"#,
    "\n",
);

#[test]
fn a_journal_with_a_host_notice_line_resumes_with_the_line_skipped() {
    let dir = TempDir::new("old-host-notice");
    let path = dir.file("s.jsonl");
    std::fs::write(&path, OLD_JOURNAL).unwrap();

    let session = Session::open(&path).expect("the old journal opens without error");
    assert_eq!(session.skipped(), &[(2, "HostNotice".to_string())]);
    assert_eq!(session.events().len(), 2, "2 of the 3 lines are known");
    assert_eq!(session.history().unwrap(), [Message::user("hello")]);
}
