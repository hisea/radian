//! ADR 0021 D3/D4: the file store. One Markdown record per file, one `MEMORY.md` index per
//! scope, atomic writes, a closed `type`/`source` set, and a `last_used` rewrite that touches
//! only its own line.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use common::TempDir;
use radian_memory::{
    FileProvenance, FileStore, IndexLine, ListEntry, MemoryError, NewRecord, RECORD_SOURCES,
    RECORD_TYPES, Scope, index_lines,
};

fn global() -> Scope {
    Scope::Global
}

fn new_record(name: &str) -> NewRecord {
    NewRecord {
        name: name.to_owned(),
        description: "a short description".to_owned(),
        record_type: "user".to_owned(),
        source: "model".to_owned(),
        provenance: None,
        body: "body\nmore\n".to_owned(),
    }
}

#[test]
fn write_then_read_round_trips_every_field_and_the_body_bytes() {
    let dir = TempDir::new("files-roundtrip");
    let store = FileStore::open(dir.path());
    let body = "first line\n---\nafter a line that spells the frontmatter fence\n\n";
    let record = NewRecord {
        name: "style".to_owned(),
        description: "Tabs, not spaces: everywhere".to_owned(),
        record_type: "feedback".to_owned(),
        source: "import".to_owned(),
        provenance: Some(FileProvenance {
            session_id: Some("session-1".to_owned()),
            turn_id: Some("turn-2".to_owned()),
        }),
        body: body.to_owned(),
    };
    let written = store.write(&global(), record).unwrap();
    assert_eq!(written.frontmatter.name, "style");
    assert_eq!(
        written.frontmatter.description,
        "Tabs, not spaces: everywhere"
    );
    assert_eq!(written.frontmatter.record_type, "feedback");
    assert_eq!(written.frontmatter.source, "import");
    assert!(written.frontmatter.created.ends_with('Z'));
    assert!(written.frontmatter.modified.ends_with('Z'));
    assert!(written.frontmatter.last_used.is_none());
    assert_eq!(
        written.frontmatter.provenance,
        Some(FileProvenance {
            session_id: Some("session-1".to_owned()),
            turn_id: Some("turn-2".to_owned()),
        })
    );

    let read = store.read(&global(), "style").unwrap();
    assert_eq!(read.frontmatter, written.frontmatter);
    assert_eq!(read.body, body, "the body keeps its exact bytes");

    // `last_used` has no input on `write`; a touch sets it and every other field stays.
    let at = "2026-10-31T12:34:56.000000Z";
    store.touch_last_used(&global(), "style", at).unwrap();
    let read = store.read(&global(), "style").unwrap();
    assert_eq!(read.frontmatter.last_used.as_deref(), Some(at));
    assert_eq!(read.frontmatter.name, written.frontmatter.name);
    assert_eq!(
        read.frontmatter.description,
        written.frontmatter.description
    );
    assert_eq!(
        read.frontmatter.record_type,
        written.frontmatter.record_type
    );
    assert_eq!(read.frontmatter.source, written.frontmatter.source);
    assert_eq!(read.frontmatter.created, written.frontmatter.created);
    assert_eq!(read.frontmatter.modified, written.frontmatter.modified);
    assert_eq!(read.frontmatter.provenance, written.frontmatter.provenance);
    assert_eq!(read.body, body);
}

#[test]
fn a_name_outside_the_rule_is_refused_before_any_file_is_touched() {
    let dir = TempDir::new("files-bad-name");
    let store = FileStore::open(dir.path());
    for name in [
        "Upper".to_owned(),
        "a/b".to_owned(),
        "a".repeat(radian_memory::NAME_MAX + 1),
        String::new(),
    ] {
        let error = store.write(&global(), new_record(&name)).unwrap_err();
        assert!(
            matches!(&error, MemoryError::InvalidName { name: refused, .. } if *refused == name),
            "{error}"
        );
    }
    assert!(
        !dir.path().join("global").exists(),
        "a refused write must not create the scope directory"
    );
}

#[test]
fn a_type_outside_the_closed_set_is_unreadable_and_reports_its_line() {
    let dir = TempDir::new("files-bad-type");
    let store = FileStore::open(dir.path());
    store.write(&global(), new_record("good")).unwrap();

    let timestamp = "2026-10-31T12:00:00.000000Z";
    std::fs::write(
        store.record_path(&global(), "hand"),
        format!(
            "---\nname: hand\ndescription: an old kind\ntype: decision\nsource: user\ncreated: {timestamp}\nmodified: {timestamp}\n---\nbody\n"
        ),
    )
    .unwrap();

    let entries = store.list(&global()).unwrap();
    let unreadable: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            ListEntry::Unreadable { path, reason } => Some((path, reason)),
            ListEntry::Record(_) => None,
        })
        .collect();
    assert_eq!(unreadable.len(), 1, "{entries:?}");
    assert!(unreadable[0].0.ends_with("hand.md"), "{entries:?}");
    assert!(
        unreadable[0].1.contains("line 4"),
        "the reason names the type line: {}",
        unreadable[0].1
    );
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry, ListEntry::Record(front) if front.name == "good")),
        "the other record still lists: {entries:?}"
    );
    let error = store.read(&global(), "hand").unwrap_err();
    assert!(
        matches!(&error, MemoryError::Frontmatter { line: 4, .. }),
        "{error}"
    );
}

fn without_last_used_line(text: &str) -> String {
    text.split_inclusive('\n')
        .filter(|line| !line.starts_with("last_used:"))
        .collect()
}

#[test]
fn touch_last_used_changes_only_the_last_used_line() {
    let dir = TempDir::new("files-touch");
    let store = FileStore::open(dir.path());
    let written = store.write(&global(), new_record("style")).unwrap();
    let path = store.record_path(&global(), "style");
    let before = std::fs::read_to_string(&path).unwrap();

    let at = "2026-10-31T12:34:56.000000Z";
    store.touch_last_used(&global(), "style", at).unwrap();
    let touched = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        without_last_used_line(&touched),
        before,
        "every byte other than the new line is unchanged"
    );
    let read = store.read(&global(), "style").unwrap();
    assert_eq!(read.frontmatter.last_used.as_deref(), Some(at));
    assert_eq!(read.frontmatter.modified, written.frontmatter.modified);
    assert_eq!(read.body, written.body);

    let next = "2026-11-01T00:00:00.000000Z";
    store.touch_last_used(&global(), "style", next).unwrap();
    let touched_again = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        without_last_used_line(&touched_again),
        without_last_used_line(&touched),
        "a second touch replaces its own line and nothing else"
    );
    let read = store.read(&global(), "style").unwrap();
    assert_eq!(read.frontmatter.last_used.as_deref(), Some(next));
    assert_eq!(read.frontmatter.modified, written.frontmatter.modified);
    assert_eq!(read.body, written.body);
}

#[cfg(unix)]
#[test]
fn a_write_creates_the_scope_directory_0700_and_the_file_0600() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new("files-modes");
    let store = FileStore::open(dir.path());
    store.write(&global(), new_record("style")).unwrap();

    let scope_mode = std::fs::metadata(store.scope_dir(&global()))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(scope_mode, 0o700);
    let file_mode = std::fs::metadata(store.record_path(&global(), "style"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(file_mode, 0o600);
    let index_mode = store.index_path(&global());
    store
        .write_index(&global(), "- [style](style.md) — a short description\n")
        .unwrap();
    let index_mode = std::fs::metadata(index_mode).unwrap().permissions().mode() & 0o777;
    assert_eq!(index_mode, 0o600);
}

fn spawn_writer(name: &str, root: &Path, out: &Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_worker", "--nocapture"])
        .env("MEM_FILES_ROOT", root)
        .env("MEM_FILES_NAME", name)
        .env("MEM_FILES_OUT", out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn two_processes_writing_different_names_in_one_scope_keep_both_files() {
    let dir = TempDir::new("files-multiprocess");
    let out_a = dir.path().join("a.out");
    let out_b = dir.path().join("b.out");
    let mut a = spawn_writer("alpha", dir.path(), &out_a);
    let mut b = spawn_writer("beta", dir.path(), &out_b);
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());
    assert_eq!(std::fs::read_to_string(&out_a).unwrap().trim(), "ok");
    assert_eq!(std::fs::read_to_string(&out_b).unwrap().trim(), "ok");

    let store = FileStore::open(dir.path());
    let names: Vec<String> = store
        .list(&global())
        .unwrap()
        .into_iter()
        .filter_map(|entry| match entry {
            ListEntry::Record(frontmatter) => Some(frontmatter.name),
            ListEntry::Unreadable { .. } => None,
        })
        .collect();
    assert_eq!(names, ["alpha", "beta"]);
    assert!(store.read(&global(), "alpha").is_ok());
    assert!(store.read(&global(), "beta").is_ok());
}

/// Child process for the multi-writer test. No-op unless spawned with `MEM_FILES_ROOT`.
#[test]
fn child_worker() {
    let Some(root) = std::env::var_os("MEM_FILES_ROOT") else {
        return;
    };
    let name = std::env::var("MEM_FILES_NAME").unwrap();
    let out = PathBuf::from(std::env::var("MEM_FILES_OUT").unwrap());
    let store = FileStore::open(root);
    let result = match store.write(&global(), new_record(&name)) {
        Ok(_) => "ok".to_owned(),
        Err(error) => format!("error {error}"),
    };
    std::fs::write(out, result).unwrap();
}

#[test]
fn index_lines_parses_the_form_and_reports_a_line_that_is_not() {
    let text = "- [Style](style.md) — tabs, not spaces\nnot an index line\n- [Deploy](deploy.md) — the office vpn\n";
    let lines = index_lines(text);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(
        lines[0],
        IndexLine::Entry {
            line: 1,
            title: "Style".to_owned(),
            file: "style.md".to_owned(),
            description: "tabs, not spaces".to_owned(),
        }
    );
    assert_eq!(lines[1], IndexLine::Invalid { line: 2 });
    assert_eq!(
        lines[2],
        IndexLine::Entry {
            line: 3,
            title: "Deploy".to_owned(),
            file: "deploy.md".to_owned(),
            description: "the office vpn".to_owned(),
        }
    );
}

#[test]
fn a_quoted_yaml_scalar_reads_as_its_value() {
    let dir = TempDir::new("files-yaml-scalars");
    let store = FileStore::open(dir.path());
    std::fs::create_dir_all(store.scope_dir(&global())).unwrap();
    let timestamp = "2026-10-31T12:00:00.000000Z";
    std::fs::write(
        store.record_path(&global(), "hand"),
        format!(
            "---\nname: \"hand\"\ndescription: 'a quoted: value'\ntype: \"user\"\nsource: 'model'\ncreated: \"{timestamp}\"\nmodified: '{timestamp}'\nprovenance:\n  session_id: \"session-1\"\n  turn_id: 'turn-2'\n---\nbody\n"
        ),
    )
    .unwrap();

    let record = store.read(&global(), "hand").unwrap();
    assert_eq!(record.frontmatter.name, "hand");
    assert_eq!(record.frontmatter.description, "a quoted: value");
    assert_eq!(record.frontmatter.record_type, "user");
    assert_eq!(record.frontmatter.source, "model");
    assert_eq!(record.frontmatter.created, timestamp);
    assert_eq!(record.frontmatter.modified, timestamp);
    assert_eq!(
        record.frontmatter.provenance,
        Some(FileProvenance {
            session_id: Some("session-1".to_owned()),
            turn_id: Some("turn-2".to_owned()),
        })
    );
    assert_eq!(record.body, "body\n");

    let entries = store.list(&global()).unwrap();
    assert!(
        entries
            .iter()
            .all(|entry| matches!(entry, ListEntry::Record(_))),
        "a quoted closed-set value is readable: {entries:?}"
    );
}

#[test]
fn a_write_keeps_the_source_of_an_existing_record() {
    let dir = TempDir::new("files-source-fixed");
    let store = FileStore::open(dir.path());
    let mut first = new_record("style");
    first.source = "user".to_owned();
    store.write(&global(), first).unwrap();

    let mut second = new_record("style");
    second.source = "model".to_owned();
    second.body = "new body\n".to_owned();
    let written = store.write(&global(), second).unwrap();
    assert_eq!(
        written.frontmatter.source, "user",
        "the returned record keeps the source fixed at the first write"
    );

    let read = store.read(&global(), "style").unwrap();
    assert_eq!(read.frontmatter.source, "user");
    assert_eq!(read.body, "new body\n");
}

#[test]
fn rename_rewrites_only_the_name_line_and_keeps_the_rest() {
    let dir = TempDir::new("files-rename");
    let store = FileStore::open(dir.path());
    let written = store.write(&global(), new_record("style")).unwrap();
    let before = store.read(&global(), "style").unwrap();
    store.rename(&global(), "style", "style-2").unwrap();
    assert!(!store.record_path(&global(), "style").exists());
    let after = store.read(&global(), "style-2").unwrap();
    assert_eq!(after.frontmatter.name, "style-2");
    assert_eq!(
        after.frontmatter.description,
        before.frontmatter.description
    );
    assert_eq!(
        after.frontmatter.record_type,
        written.frontmatter.record_type
    );
    assert_eq!(after.frontmatter.source, written.frontmatter.source);
    assert_eq!(after.frontmatter.created, written.frontmatter.created);
    assert_eq!(after.frontmatter.modified, written.frontmatter.modified);
    assert_eq!(after.body, "body\nmore\n");
    assert!(
        store.rename(&global(), "style-2", "style-3").is_ok(),
        "a free target name is accepted"
    );
    store
        .write(&global(), new_record("style"))
        .expect("occupy the old name");
    let error = store.rename(&global(), "style-3", "style").unwrap_err();
    assert!(error.to_string().contains("already exists"), "{error}");
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn backticked(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else {
            break;
        };
        values.push(after[..close].to_owned());
        rest = &after[close + 1..];
    }
    values
}

/// The comma-separated backticked values that directly follow `marker`.
fn documented_values_after(text: &str, marker: &str) -> Vec<String> {
    let start = text
        .find(marker)
        .unwrap_or_else(|| panic!("the docs do not mention {marker}"));
    let mut rest = &text[start + marker.len()..];
    let mut values = Vec::new();
    while let Some(open) = rest.find('`') {
        if !rest[..open]
            .chars()
            .all(|ch| ch.is_whitespace() || ch == ':' || ch == ',')
        {
            break;
        }
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else {
            break;
        };
        values.push(after[..close].to_owned());
        rest = &after[close + 1..];
        let trimmed = rest.trim_start();
        let Some(after_comma) = trimmed.strip_prefix(',') else {
            break;
        };
        rest = after_comma;
    }
    assert!(!values.is_empty(), "no values follow {marker}");
    values
}

/// A D4 table row's closed-set cell: `| \`type\` | closed set | \`a\`, \`b\`. |`.
fn adr_closed_set(adr: &str, field: &str) -> Vec<String> {
    let prefix = format!("| `{field}` | closed set |");
    let row = adr
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("ADR 0021 D4 has no closed-set row for {field}"));
    let cell = row.split('|').nth(3).unwrap_or("");
    backticked(cell)
}

#[test]
fn the_documented_closed_sets_are_the_constants() {
    let root = repo_root();
    let readme = std::fs::read_to_string(root.join("crates/radian-memory/README.md")).unwrap();
    let adr = std::fs::read_to_string(root.join("docs/adr/0021-memory.md")).unwrap();

    let documented_types = documented_values_after(&readme, "`RECORD_TYPES`:");
    assert_eq!(
        documented_types
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        RECORD_TYPES,
        "the README lists a different `type` set than RECORD_TYPES"
    );
    let documented_sources = documented_values_after(&readme, "`RECORD_SOURCES`:");
    assert_eq!(
        documented_sources
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        RECORD_SOURCES,
        "the README lists a different `source` set than RECORD_SOURCES"
    );
    assert_eq!(
        adr_closed_set(&adr, "type")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        RECORD_TYPES,
        "ADR 0021 D4 lists a different `type` set than RECORD_TYPES"
    );
    assert_eq!(
        adr_closed_set(&adr, "source")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        RECORD_SOURCES,
        "ADR 0021 D4 lists a different `source` set than RECORD_SOURCES"
    );
}
