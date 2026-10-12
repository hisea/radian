//! ADR 0006 §15-1: the envelope, sequential `seq`, LF line endings, and byte-exact
//! `OpaqueJson` round trips through the file.

mod common;

use agent_core::{
    ContentPart, JournalRecord, Message, MessageMeta, ModelUsage, OpaqueJson, ToolCall, ToolResult,
    TranscriptJournal,
};
use common::TempDir;
use radian_session::{Session, SessionCreatedData, SessionJournal};
use serde_json::json;

fn created() -> SessionCreatedData {
    SessionCreatedData::new(
        "/work/example",
        json!({"model": {"provider": "example", "model": "m-1"}}),
    )
}

#[tokio::test]
async fn a_session_file_has_the_v1_envelope_and_sequential_seqs() {
    let dir = TempDir::new("envelope");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid-1", created()).unwrap();

    journal
        .append(JournalRecord::Message {
            index: 0,
            message: Message::user("hello"),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::ToolExecutionStarted {
            call_id: "c1".into(),
            name: "echo".into(),
            arguments: json!({"a": 1}),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::ModelUsage {
            usage: ModelUsage::default(),
        })
        .await
        .unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.ends_with('\n'), "the file must end with LF");
    let lines: Vec<&str> = raw.lines().collect();
    assert_eq!(lines.len(), 4);

    // Fixed field order (ADR 0006 §3) and the first line.
    assert!(
        lines[0].starts_with(r#"{"schema_version":1,"session_id":"sid-1","seq":0,"ts":"#),
        "{}",
        lines[0]
    );
    for (i, line) in lines.iter().enumerate() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(v["session_id"], "sid-1");
        assert_eq!(v["seq"], i);
        assert!(v["ts"].as_str().unwrap().ends_with('Z'), "{}", v["ts"]);
        assert!(v["critical"].is_boolean());
        assert!(v["data"].is_object());
    }
    let events: Vec<String> = lines
        .iter()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["event"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        events,
        [
            "SessionCreated",
            "MessageAppended",
            "ToolExecutionStarted",
            "ModelUsageRecorded"
        ]
    );
}

#[tokio::test]
async fn opaque_json_round_trips_byte_for_byte_through_the_file() {
    let dir = TempDir::new("opaque");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid-opaque", created()).unwrap();

    // Deliberately unconventional whitespace and key order: only `OpaqueJson` fields may
    // survive this untouched.
    let payload_raw = r#"{ "think":  {"sig":"abc"} ,"k":  1}"#;
    let details_raw = r#"{"reasoning_tokens": 3,  "audio": null}"#;
    let assistant = Message::Assistant {
        content: vec![ContentPart::text("hi")],
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "echo".into(),
            arguments: json!({"z": 1, "a": 2}),
        }],
        provider_payload: Some(OpaqueJson::parse(payload_raw).unwrap()),
        interrupted: false,
        meta: MessageMeta::default(),
    };
    journal
        .append(JournalRecord::Message {
            index: 0,
            message: assistant.clone(),
        })
        .await
        .unwrap();
    journal
        .append(JournalRecord::ModelUsage {
            usage: ModelUsage {
                input: Some(10),
                output: None,
                total: Some(12),
                cache_read: None,
                cache_write: None,
                details: Some(OpaqueJson::parse(details_raw).unwrap()),
            },
        })
        .await
        .unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(
        raw.contains(payload_raw),
        "provider_payload must be written byte-for-byte:\n{raw}"
    );
    assert!(
        raw.contains(details_raw),
        "usage.details must be written byte-for-byte:\n{raw}"
    );

    // And the reader gets the same bytes back, so equality holds on the parsed message.
    let session = Session::open(&path).unwrap();
    match &session.events()[1].data {
        radian_session::EventData::MessageAppended { message, .. } => {
            assert_eq!(message, &assistant);
            assert_eq!(
                message.provider_payload().map(|p| p.get()),
                Some(payload_raw)
            );
        }
        other => panic!("expected a MessageAppended, got {other:?}"),
    }
}

trait ProviderPayload {
    fn provider_payload(&self) -> Option<&OpaqueJson>;
}

impl ProviderPayload for Message {
    fn provider_payload(&self) -> Option<&OpaqueJson> {
        match self {
            Message::Assistant {
                provider_payload, ..
            } => provider_payload.as_ref(),
            _ => None,
        }
    }
}

#[tokio::test]
async fn resuming_continues_the_sequence_and_keeps_the_session_id() {
    let dir = TempDir::new("resume");
    let path = dir.file("s.jsonl");
    {
        let journal = SessionJournal::create(&path, "sid-r", created()).unwrap();
        journal
            .append(JournalRecord::Message {
                index: 0,
                message: Message::user("one"),
            })
            .await
            .unwrap();
        assert_eq!(journal.next_seq(), 2);
    }
    let journal = SessionJournal::resume(&path, "sid-r").unwrap();
    assert_eq!(journal.next_seq(), 2);
    journal
        .append(JournalRecord::Message {
            index: 1,
            message: Message::assistant("two"),
        })
        .await
        .unwrap();
    let session = Session::open(&path).unwrap();
    let seqs: Vec<u64> = session.events().iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [0, 1, 2]);
}

#[tokio::test]
async fn a_tool_result_settles_with_its_full_shape() {
    let dir = TempDir::new("settled");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, "sid-t", created()).unwrap();
    journal
        .append(JournalRecord::ToolExecutionSettled {
            call_id: "c9".into(),
            result: ToolResult::success("c9", vec![ContentPart::text("done")]),
        })
        .await
        .unwrap();
    let session = Session::open(&path).unwrap();
    match &session.events()[1].data {
        radian_session::EventData::ToolExecutionSettled { call_id, result } => {
            assert_eq!(call_id, "c9");
            assert_eq!(result.text(), "done");
        }
        other => panic!("expected ToolExecutionSettled, got {other:?}"),
    }
}
