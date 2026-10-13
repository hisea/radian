//! Export (task reqs 8/9): JSON and Markdown, provider payloads opt-in, and redaction that
//! cannot be switched off (ADR 0013 T4 / decision 3).

mod common;

use common::TempDir;
use radian_session::{
    ExportOptions, Session, export_json, export_json_to, export_markdown, export_markdown_to,
};
use serde_json::json;

const FAKE_KEY: &str = "sk-test-secret-1234567890";
const CUSTOM_HEADER: &str = "hdr-secret-value";
const PAYLOAD_MARKER: &str = "payload-marker-xyz";
const VISIBLE_MARKER: &str = "visible-marker-abc";
const REASONING_MARKER: &str = "reasoning-marker-def";

/// A session whose assistant message carries a provider payload and a fake key in its
/// visible text, and whose `SessionCreated` config snapshot carries a custom header.
fn write_export_session(path: &std::path::Path) {
    let sid = "sid-export";
    let lines = [
        json!({"schema_version":1,"session_id":sid,"seq":0,
        "ts":"2026-10-12T08:00:00.000000Z","event":"SessionCreated","critical":true,
        "data":{"workspace":"/work/export","config_snapshot":{
            "headers":[["X-Api-Key", CUSTOM_HEADER]],
            "extra_body":{"authorization":format!("Bearer {FAKE_KEY}")}
        }}}),
        json!({"schema_version":1,"session_id":sid,"seq":1,
            "ts":"2026-10-12T08:00:01.000000Z","event":"MessageAppended","critical":true,
            "data":{"index":0,"message":{"user":{"content":[{"text":{"text":"hello"}}]}}}}),
        json!({"schema_version":1,"session_id":sid,"seq":2,
        "ts":"2026-10-12T08:00:02.000000Z","event":"MessageAppended","critical":true,
        "data":{"index":1,"message":{"assistant":{
            "content":[{"text":{"text":format!("answer with {VISIBLE_MARKER} and key {FAKE_KEY}")}}],
            "provider_payload":{"marker":PAYLOAD_MARKER,"api_key":FAKE_KEY},
            "reasoning":[{"text":{"text":format!("thinking {REASONING_MARKER} about {FAKE_KEY}")}}]
        }}}}),
    ];
    let mut out = String::new();
    for line in lines {
        out.push_str(&line.to_string());
        out.push('\n');
    }
    std::fs::write(path, out).unwrap();
}

fn demo(dir: &TempDir) -> (std::path::PathBuf, Session) {
    let path = dir.file("s.jsonl");
    write_export_session(&path);
    let session = Session::open(&path).unwrap();
    (path, session)
}

#[test]
fn json_export_omits_provider_payload_by_default_and_includes_it_when_opted_in() {
    let dir = TempDir::new("export-json");
    let (_path, session) = demo(&dir);

    let default = export_json(&session, &ExportOptions::new()).unwrap();
    assert!(
        !default.contains(PAYLOAD_MARKER),
        "the raw provider payload must not appear by default"
    );
    assert!(
        default.contains(VISIBLE_MARKER),
        "the message text is exported"
    );

    let with_payload = export_json(
        &session,
        &ExportOptions::new().include_provider_payload(true),
    )
    .unwrap();
    assert!(
        with_payload.contains(PAYLOAD_MARKER),
        "the payload is included when the user opts in"
    );
}

#[test]
fn markdown_export_follows_the_same_payload_default() {
    let dir = TempDir::new("export-md");
    let (_path, session) = demo(&dir);

    let default = export_markdown(&session, &ExportOptions::new()).unwrap();
    assert!(!default.contains(PAYLOAD_MARKER));
    assert!(default.contains(VISIBLE_MARKER));

    let with_payload = export_markdown(
        &session,
        &ExportOptions::new().include_provider_payload(true),
    )
    .unwrap();
    assert!(with_payload.contains(PAYLOAD_MARKER));
}

/// The fake key and the custom header value must be absent from every export, with and
/// without the payload, in both formats. There is no switch that turns this off.
#[test]
fn secrets_never_appear_in_any_export() {
    let dir = TempDir::new("export-redact");
    let (_path, session) = demo(&dir);
    let payload = ExportOptions::new().include_provider_payload(true);
    let no_payload = ExportOptions::new();

    let json_out = dir.file("out.json");
    let md_out = dir.file("out.md");
    export_json_to(&session, &no_payload, &json_out).unwrap();
    export_markdown_to(&session, &no_payload, &md_out).unwrap();
    let json_with = export_json(&session, &payload).unwrap();
    let md_with = export_markdown(&session, &payload).unwrap();

    let files = [
        std::fs::read_to_string(&json_out).unwrap(),
        std::fs::read_to_string(&md_out).unwrap(),
        json_with,
        md_with,
    ];
    for text in &files {
        assert!(!text.contains(FAKE_KEY), "fake key leaked: {text}");
        assert!(
            !text.contains(CUSTOM_HEADER),
            "custom header value leaked: {text}"
        );
        // A canary: the export is not empty and the payload marker is present when asked.
        assert!(text.contains(VISIBLE_MARKER));
    }
    // The key was in the payload, so with the payload the redactor is what removed it.
    assert!(files[2].contains(PAYLOAD_MARKER));
}

/// A secret the pattern scanner cannot recognize is still removed when the host names it.
#[test]
fn a_named_secret_is_redacted_even_without_a_known_pattern() {
    let dir = TempDir::new("export-named");
    let (path, _session) = demo(&dir);
    // Put the opaque token in a plain message so no payload stripping can hide it.
    let hidden = "opaque-token-9f3a";
    let mut body = String::new();
    for line in std::fs::read_to_string(&path).unwrap().lines() {
        body.push_str(&line.replace("hello", hidden));
        body.push('\n');
    }
    std::fs::write(&path, &body).unwrap();
    let session = Session::open(&path).unwrap();

    let without = export_json(&session, &ExportOptions::new()).unwrap();
    assert!(without.contains(hidden), "unrecognized tokens pass through");
    let with = export_json(&session, &ExportOptions::new().with_secret(hidden)).unwrap();
    assert!(!with.contains(hidden), "a named secret is always redacted");
}

#[test]
fn export_never_modifies_the_session_file() {
    let dir = TempDir::new("export-unchanged");
    let (path, session) = demo(&dir);
    let before = std::fs::read(&path).unwrap();
    export_json_to(&session, &ExportOptions::new(), dir.file("a.json")).unwrap();
    export_markdown_to(
        &session,
        &ExportOptions::new().include_provider_payload(true),
        dir.file("b.md"),
    )
    .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

/// ADR 0014, decision 2026-10-12: reasoning is opt-in per export, like the raw payload,
/// and **both formats agree**. Before this, JSON carried reasoning while Markdown silently
/// dropped it -- neither by decision.
#[test]
fn reasoning_is_omitted_by_default_in_both_formats_and_included_when_opted_in() {
    let dir = TempDir::new("export-reasoning");
    let (_, session) = demo(&dir);

    let default = ExportOptions::new();
    let json_default = export_json(&session, &default).unwrap();
    let md_default = export_markdown(&session, &default).unwrap();
    assert!(!json_default.contains(REASONING_MARKER), "{json_default}");
    assert!(!md_default.contains(REASONING_MARKER), "{md_default}");
    // The answer itself is still there: only the reasoning was dropped.
    assert!(json_default.contains(VISIBLE_MARKER));
    assert!(md_default.contains(VISIBLE_MARKER));

    let opted = ExportOptions::new().include_reasoning(true);
    let json_opted = export_json(&session, &opted).unwrap();
    let md_opted = export_markdown(&session, &opted).unwrap();
    assert!(json_opted.contains(REASONING_MARKER), "{json_opted}");
    assert!(md_opted.contains(REASONING_MARKER), "{md_opted}");
    // Kept apart from the answer and labelled, never folded into it.
    let reasoning_at = md_opted.find(REASONING_MARKER).unwrap();
    assert!(md_opted[..reasoning_at].contains("reasoning"), "{md_opted}");

    // Opting in to reasoning does not opt in to the payload, and turns nothing else off.
    assert!(!json_opted.contains(PAYLOAD_MARKER), "{json_opted}");
    // Redaction still applies inside the reasoning that was included.
    for out in [&json_opted, &md_opted] {
        assert!(!out.contains(FAKE_KEY), "{out}");
    }
}
