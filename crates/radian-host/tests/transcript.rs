//! The shared folded transcript (ADR 0023 U1): both folds, the one entry type they produce and
//! its JSON shape.
//!
//! Each test names the mutation it catches where the ADR's test list requires one.

use std::io::Write;
use std::path::{Path, PathBuf};

use radian_core::Message;
use radian_host::transcript::{AttachFold, Card, Entry, EntryKind};

/// A scratch directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "radian-host-transcript-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One journal envelope (ADR 0006 §1).
fn envelope(seq: u64, event: &str, critical: bool, data: serde_json::Value) -> String {
    format!(
        r#"{{"schema_version":1,"session_id":"shared-fold","seq":{seq},"ts":"2026-11-02T00:00:00.000000Z","event":"{event}","critical":{critical},"data":{data}}}"#
    )
}

/// A `MessageAppended` body for the message at transcript `index`.
fn appended(index: u64, message: Message) -> serde_json::Value {
    serde_json::json!({"index": index, "message": message})
}

/// Writes `lines` as `<id>.jsonl` and opens the fold over it.
fn open_fold(label: &str, id: &str, lines: &[String]) -> (Scratch, PathBuf, AttachFold) {
    let scratch = Scratch::new(label);
    let path = scratch.file(&format!("{id}.jsonl"));
    std::fs::write(&path, format!("{}\n", lines.join("\n"))).unwrap();
    let fold = AttachFold::open(&path, None).expect("the fold opens");
    (scratch, path, fold)
}

/// Appends `lines` to the journal a fold is following.
fn append(path: &Path, lines: &[String]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(format!("{}\n", lines.join("\n")).as_bytes())
        .unwrap();
    file.flush().unwrap();
}

/// **Edge case 3.** Every entry kind has one stable JSON shape: a snake_case object whose first
/// key is the `kind` tag, with every field present in declaration order. An image's base64
/// payload is never serialized (the placeholder line in `text` stands in for it).
///
/// Mutation: add, remove, rename or reorder a field of [`Entry`] or a variant of [`EntryKind`],
/// or serialize an image's payload, and this FAILS.
#[test]
fn the_json_shape_of_every_entry_kind_is_stable() {
    let golden = [
        (EntryKind::User, r#"{"kind":"user""#),
        (EntryKind::Assistant, r#"{"kind":"assistant""#),
        (EntryKind::Reasoning, r#"{"kind":"reasoning""#),
        (EntryKind::ToolCall, r#"{"kind":"tool_call""#),
        (EntryKind::ToolResult, r#"{"kind":"tool_result""#),
        (EntryKind::Notice, r#"{"kind":"notice""#),
        (EntryKind::Banner, r#"{"kind":"banner""#),
        (EntryKind::Startup, r#"{"kind":"startup""#),
        (EntryKind::TrustHint, r#"{"kind":"trust_hint""#),
        (EntryKind::Warning, r#"{"kind":"warning""#),
        (EntryKind::Tip, r#"{"kind":"tip""#),
        (EntryKind::Compaction, r#"{"kind":"compaction""#),
        (EntryKind::Context, r#"{"kind":"context""#),
        (
            EntryKind::Bash {
                exclude_from_context: true,
            },
            r#"{"kind":"bash","exclude_from_context":true"#,
        ),
    ];
    // The tail every entry carries, unchanged by its kind.
    let tail = r#""text":"x","is_error":false,"result":null,"error_code":null,"card":"none","tool":null,"compaction":null,"interrupted":false,"images":[]}"#;
    for (kind, head) in golden {
        let entry = Entry::new(kind, "x");
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(json, format!("{head},{tail}"), "{kind:?}");
        // Round-trips through JSON: the serialized value parses to the same value.
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, serde_json::to_value(&entry).unwrap(), "{kind:?}");
    }

    // A tool entry's card data is a snake_case object too, and an image keeps its facts but not
    // its payload.
    let mut entry = Entry::new(EntryKind::ToolCall, "shell $ ls");
    entry.tool = Some(Box::new(radian_host::transcript::ToolCard::new(
        "c1",
        "shell",
        serde_json::json!({"command": "ls"}),
    )));
    entry.card = Card::Solo;
    entry.result = Some(false);
    entry.images = vec![radian_host::transcript::ImagePart {
        data: std::sync::Arc::from("QUJD"),
        mime_type: "image/png".to_owned(),
        width: 2,
        height: 3,
        placeholder: "[image: shot.png, 2×3, 1 KB]".to_owned(),
    }];
    let json = serde_json::to_value(&entry).unwrap();
    assert_eq!(json["kind"], "tool_call");
    assert_eq!(json["card"], "solo");
    assert_eq!(json["result"], false);
    assert_eq!(json["tool"]["call_id"], "c1");
    assert_eq!(json["tool"]["name"], "shell");
    assert_eq!(json["tool"]["arguments"]["command"], "ls");
    assert_eq!(json["images"][0]["mime_type"], "image/png");
    assert_eq!(
        json["images"][0]["placeholder"],
        "[image: shot.png, 2×3, 1 KB]"
    );
    assert!(
        json["images"][0].get("data").is_none(),
        "an image payload must not be serialized: {json}"
    );
}

/// The journal the two late-result tests fold: a user turn, an assistant message that calls
/// `read`, and (once committed) the result.
fn journal_with_a_call() -> Vec<String> {
    let call = radian_core::ToolCall {
        id: "c1".to_owned(),
        name: "read".to_owned(),
        arguments: serde_json::json!({"path": "a.rs"}),
    };
    vec![
        envelope(
            0,
            "SessionCreated",
            true,
            serde_json::json!({"workspace": "/w", "config_snapshot": {}}),
        ),
        envelope(
            1,
            "TurnStarted",
            true,
            serde_json::json!({"turn_id": "t-1"}),
        ),
        envelope(
            2,
            "MessageAppended",
            true,
            appended(0, Message::user("read it")),
        ),
        envelope(
            3,
            "MessageAppended",
            true,
            appended(
                1,
                Message::Assistant {
                    content: Vec::new(),
                    tool_calls: vec![call],
                    reasoning: Vec::new(),
                    interrupted: false,
                    meta: radian_core::MessageMeta::default(),
                    provider_payload: None,
                },
            ),
        ),
    ]
}

/// **Edge case 4 (the journal fold).** A tool call whose result arrives later updates the entry
/// the call already has, and the call stays one entry (ADR 0002/0028 R4).
///
/// Mutation: make the pairing push a new tool call entry instead of rewriting the matched one
/// and this FAILS (two `ToolCall` entries, the first with no result).
#[test]
fn a_late_journal_result_updates_the_call_entry_it_folded_earlier() {
    let (_scratch, path, mut fold) = open_fold("journal", "late", &journal_with_a_call());
    // Not live: the open call is shown with the fold's recovery wording (ADR 0002) until the
    // result commits.
    let before: Vec<usize> = fold
        .entries()
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.kind == EntryKind::ToolCall)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(before.len(), 1, "one call, one entry");
    let call_index = before[0];
    assert_eq!(
        fold.entries()[call_index].result,
        Some(true),
        "the repaired result is the crash's error"
    );
    assert_eq!(
        fold.entries()[call_index + 1].kind,
        EntryKind::ToolResult,
        "the recovery wording is a result entry"
    );

    append(
        &path,
        &[
            envelope(
                4,
                "MessageAppended",
                true,
                appended(
                    2,
                    Message::Tool {
                        result: radian_core::ToolResult {
                            call_id: "c1".to_owned(),
                            content: vec![radian_core::ContentPart::text("7: fn a() {}")],
                            structured: None,
                            is_error: false,
                            error_code: None,
                        },
                        meta: radian_core::MessageMeta::default(),
                    },
                ),
            ),
            envelope(
                5,
                "TurnFinished",
                true,
                serde_json::json!({"turn_id": "t-1", "outcome": "completed"}),
            ),
        ],
    );
    assert!(fold.poll().unwrap(), "the poll sees the committed result");

    let calls: Vec<usize> = fold
        .entries()
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.kind == EntryKind::ToolCall)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        calls,
        vec![call_index],
        "the same call entry, not a new one"
    );
    let call = &fold.entries()[call_index];
    assert_eq!(call.result, Some(false));
    assert_eq!(call.card, Card::Head);
    assert_eq!(call.text, "read a.rs");
    assert_eq!(
        call.tool.as_ref().and_then(|card| card.result_lines),
        Some(1)
    );
    assert_eq!(fold.entries()[call_index + 1].kind, EntryKind::ToolResult);
    assert_eq!(fold.entries()[call_index + 1].text, "7: fn a() {}");
    assert_eq!(fold.entries()[call_index + 1].card, Card::Tail);
}
