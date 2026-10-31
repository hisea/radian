//! ADR 0021 D6: `MemoryInjected` has two shapes. The old shape carries `{ id, revision, scope }`
//! records; the new shape carries `{ scope, path, sha256, lines, bytes }` for each injected
//! `MEMORY.md`. Exactly one shape is present, and this build writes only `indexes`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{TempDir, created};
use radian_session::{
    EventData, ExportOptions, MEMORY_INJECTED_INDEX_SCOPES, MemoryInjectedIndex, ReadError,
    Session, SessionJournal, export_json,
};
use serde_json::{Value, json};

const SID: &str = "11111111-1111-4111-8111-111111111111";
const SHA_A: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Writes a two-line session: `SessionCreated` at seq 0 and one raw `MemoryInjected` at seq 1,
/// so a malformed shape can be tested without the writer's validation.
fn path_with_memory_data(dir: &TempDir, data: Value) -> PathBuf {
    let path = dir.file("s.jsonl");
    let lines = [
        json!({"schema_version":1,"session_id":SID,"seq":0,
            "ts":"2026-10-12T13:00:00.000000Z","event":"SessionCreated","critical":true,
            "data":{"workspace":"/work/example","config_snapshot":{}}}),
        json!({"schema_version":1,"session_id":SID,"seq":1,
            "ts":"2026-10-12T13:00:01.000000Z","event":"MemoryInjected","critical":false,
            "data":data}),
    ];
    let mut out = String::new();
    for line in lines {
        out.push_str(&line.to_string());
        out.push('\n');
    }
    std::fs::write(&path, out).unwrap();
    path
}

fn index(scope: &str) -> Value {
    json!({
        "scope": scope,
        "path": "/home/example/.radian/memory/global/MEMORY.md",
        "sha256": SHA_A,
        "lines": 1,
        "bytes": 40,
    })
}

fn one_index(scope: &str) -> MemoryInjectedIndex {
    MemoryInjectedIndex {
        scope: scope.to_string(),
        path: format!("/home/example/.radian/memory/{scope}/MEMORY.md"),
        sha256: SHA_A.to_string(),
        lines: 1,
        bytes: 40,
    }
}

/// The row and the ADR name each scope in a backticked list after `marker`; the list is the
/// constant. A scope added to one side without the other fails here.
fn documented_values_after(text: &str, marker: &str) -> Vec<String> {
    let start = text
        .find(marker)
        .unwrap_or_else(|| panic!("the docs do not mention {marker}"));
    let rest = &text[start + marker.len()..];
    let end = rest
        .find(['。', '.', '\n'])
        .expect("the documented list ends a sentence");
    let mut segment = &rest[..end];
    let mut values = Vec::new();
    while let Some(open) = segment.find('`') {
        let after = &segment[open + 1..];
        let Some(close) = after.find('`') else {
            break;
        };
        values.push(after[..close].to_string());
        segment = &after[close + 1..];
    }
    values
}

#[test]
fn the_documented_index_scopes_are_the_constant() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let readme = std::fs::read_to_string(root.join("docs/schema/fixtures/README.md")).unwrap();
    let adr = std::fs::read_to_string(root.join("docs/adr/0021-memory.md")).unwrap();
    for (name, text, marker) in [
        ("the fixture table", readme, "`scope` 只取 "),
        ("ADR 0021 D6", adr, "`scope` is one of "),
    ] {
        assert_eq!(
            documented_values_after(&text, marker),
            MEMORY_INJECTED_INDEX_SCOPES,
            "{name} lists a different index scope set than MEMORY_INJECTED_INDEX_SCOPES"
        );
    }
    assert_eq!(MEMORY_INJECTED_INDEX_SCOPES, ["project", "global"]);
}

/// Edge 1: an old journal line with `records` only reads as before; the new vector is empty.
#[test]
fn an_old_records_only_line_reads_as_before() {
    let dir = TempDir::new("memory-old-shape");
    let path = path_with_memory_data(
        &dir,
        json!({"records":[{"id":"mem-1","revision":3,"scope":"global"}]}),
    );
    let session = Session::open(&path).unwrap();
    match &session.events()[1].data {
        EventData::MemoryInjected { records, indexes } => {
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].id, "mem-1");
            assert_eq!(records[0].revision, 3);
            assert!(indexes.is_empty(), "an old record carries no indexes");
        }
        other => panic!("expected MemoryInjected, got {other:?}"),
    }
}

/// Edge 2: an `indexes`-only line reads; a `sha256` shorter than 64 hex characters is refused
/// at its line.
#[test]
fn an_indexes_only_line_reads_and_a_short_sha256_is_refused_with_its_line() {
    let dir = TempDir::new("memory-new-shape");
    let path = path_with_memory_data(&dir, json!({"indexes":[index("project")]}));
    let session = Session::open(&path).unwrap();
    match &session.events()[1].data {
        EventData::MemoryInjected { records, indexes } => {
            assert!(records.is_empty(), "a new record carries no records");
            assert_eq!(indexes.len(), 1);
            assert_eq!(indexes[0].scope, "project");
            assert_eq!(indexes[0].sha256, SHA_A);
            assert_eq!(indexes[0].lines, 1);
            assert_eq!(indexes[0].bytes, 40);
        }
        other => panic!("expected MemoryInjected, got {other:?}"),
    }

    let mut short = index("global");
    short["sha256"] = json!("abc");
    let dir = TempDir::new("memory-short-sha");
    let err = Session::open(path_with_memory_data(&dir, json!({"indexes":[short]}))).unwrap_err();
    let ReadError::Corrupt { line, message, .. } = &err else {
        panic!("expected Corrupt, got {err:?}");
    };
    assert_eq!(*line, 2, "the 1-based line must be reported");
    assert!(message.contains("sha256"), "{message}");
}

/// Edge 3: a line carrying both shapes is refused, naming its line.
#[test]
fn a_line_with_both_records_and_indexes_is_refused_naming_its_line() {
    let dir = TempDir::new("memory-both-shapes");
    let data = json!({
        "records": [{"id":"mem-1","revision":3,"scope":"global"}],
        "indexes": [index("global")],
    });
    let err = Session::open(path_with_memory_data(&dir, data)).unwrap_err();
    let ReadError::Corrupt { line, message, .. } = &err else {
        panic!("expected Corrupt, got {err:?}");
    };
    assert_eq!(*line, 2, "the 1-based line must be reported");
    assert!(message.contains("both"), "{message}");
    assert!(
        message.contains("records") && message.contains("indexes"),
        "{message}"
    );
}

/// Edge 4: a line carrying neither shape is refused.
#[test]
fn a_line_with_neither_shape_is_refused_naming_its_line() {
    let dir = TempDir::new("memory-no-shape");
    let err = Session::open(path_with_memory_data(&dir, json!({"other":true}))).unwrap_err();
    let ReadError::Corrupt { line, message, .. } = &err else {
        panic!("expected Corrupt, got {err:?}");
    };
    assert_eq!(*line, 2, "the 1-based line must be reported");
    assert!(message.contains("neither"), "{message}");
}

/// Edge 5: `scope: "session"` is outside the closed set; the message lists both allowed values.
#[test]
fn an_unknown_scope_is_refused_with_the_allowed_values() {
    let dir = TempDir::new("memory-bad-scope");
    let mut bad = index("global");
    bad["scope"] = json!("session");
    let err = Session::open(path_with_memory_data(&dir, json!({"indexes":[bad]}))).unwrap_err();
    let ReadError::Corrupt { line, message, .. } = &err else {
        panic!("expected Corrupt, got {err:?}");
    };
    assert_eq!(*line, 2, "the 1-based line must be reported");
    assert!(message.contains("session"), "{message}");
    assert!(
        message.contains("project") && message.contains("global"),
        "the message lists the closed set: {message}"
    );
}

/// Edge 6: the writer's new-shape record exports through the reader byte-for-byte.
#[tokio::test]
async fn a_new_shape_record_exports_byte_for_byte() {
    let dir = TempDir::new("memory-new-shape-export");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, SID, created()).unwrap();
    journal
        .append_memory_injected_indexes(vec![one_index("project"), one_index("global")])
        .await
        .unwrap();
    drop(journal);

    let raw = std::fs::read_to_string(&path).unwrap();
    let line: Value = serde_json::from_str(raw.lines().nth(1).unwrap()).unwrap();
    assert!(line["data"].get("indexes").is_some(), "{line}");
    assert!(
        line["data"].get("records").is_none(),
        "this build writes only indexes: {line}"
    );
    let original = serde_json::to_string(&line["data"]).unwrap();

    let session = Session::open(&path).unwrap();
    let json = export_json(&session, &ExportOptions::new()).unwrap();
    let doc: Value = serde_json::from_str(&json).unwrap();
    let exported = doc["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["event"] == "MemoryInjected")
        .expect("the export carries the record");
    assert_eq!(exported["data"], line["data"]);
    assert_eq!(
        serde_json::to_string(&exported["data"]).unwrap(),
        original,
        "the export's data is byte-for-byte the record that was written"
    );
}

/// The writer reads the same closed set and hash rule as the reader: it refuses an unknown
/// scope and a short hash before writing a line.
#[tokio::test]
async fn the_index_writer_refuses_an_unknown_scope_or_short_hash() {
    let dir = TempDir::new("memory-writer-validation");
    let path = dir.file("s.jsonl");
    let journal = SessionJournal::create(&path, SID, created()).unwrap();

    let mut bad = one_index("global");
    bad.scope = "session".to_string();
    assert!(
        journal
            .append_memory_injected_indexes(vec![bad])
            .await
            .is_err()
    );

    let mut short = one_index("global");
    short.sha256 = "abc".to_string();
    assert!(
        journal
            .append_memory_injected_indexes(vec![short])
            .await
            .is_err()
    );

    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("MemoryInjected"),
        "a refused record writes no line"
    );
}

/// Edge 7: the checked-in fixture passes the reference checker, and a copy whose new line says
/// `"scope": "x"` fails it.
#[test]
fn the_fixture_passes_the_checker_and_an_unknown_scope_copy_fails_it() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let schema = repo.join("docs/schema");
    let dir = TempDir::new("memory-checker");
    // The checker resolves one sibling path from the repository root
    // (`../../crates/radian-core/src/model.rs`), so the copy keeps the repo layout.
    let copy_schema = dir.file("docs/schema");
    std::fs::create_dir_all(copy_schema.join("fixtures")).unwrap();
    std::fs::copy(
        schema.join("check-fixtures.sh"),
        copy_schema.join("check-fixtures.sh"),
    )
    .unwrap();
    for entry in std::fs::read_dir(schema.join("fixtures")).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(
            entry.path(),
            copy_schema.join("fixtures").join(entry.file_name()),
        )
        .unwrap();
    }
    let core_source = dir.file("crates/radian-core/src");
    std::fs::create_dir_all(&core_source).unwrap();
    std::fs::copy(
        repo.join("crates/radian-core/src/model.rs"),
        core_source.join("model.rs"),
    )
    .unwrap();
    let script = copy_schema.join("check-fixtures.sh");

    let pass = Command::new("bash")
        .arg(&script)
        .output()
        .expect("bash runs");
    let pass_stdout = String::from_utf8_lossy(&pass.stdout);
    assert!(
        pass.status.success(),
        "the fixture must pass:\n{pass_stdout}\n{}",
        String::from_utf8_lossy(&pass.stderr)
    );

    let fixture = copy_schema.join("fixtures/memory-injected.jsonl");
    let text = std::fs::read_to_string(&fixture).unwrap();
    let changed = text.replacen("\"scope\":\"project\"", "\"scope\":\"x\"", 1);
    assert_ne!(changed, text, "the new line names the project scope");
    std::fs::write(&fixture, changed).unwrap();

    let fail = Command::new("bash")
        .arg(&script)
        .output()
        .expect("bash runs");
    let fail_stdout = String::from_utf8_lossy(&fail.stdout);
    assert!(
        !fail.status.success(),
        "an unknown scope must fail the checker:\n{fail_stdout}"
    );
    assert!(
        fail_stdout.contains("FAIL") && fail_stdout.contains("scope"),
        "the checker names the refused line and field:\n{fail_stdout}"
    );
}
