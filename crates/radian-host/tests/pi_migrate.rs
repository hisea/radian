//! `radian_host::config::migrate_pi` (Phase 5): the Pi → Radian draft.
//!
//! Every fixture is a temporary directory this test creates. Nothing here reads the user's
//! real `~/.pi` (there is no default directory at all) or `~/.radian`.

use std::path::{Path, PathBuf};

use radian_host::config::migrate_pi::{PiMigrateError, import_pi, write_draft};

/// A temporary directory, removed on drop.
struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "pi-migrate-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("[scratch] kept for debugging: {}", self.0.display());
            return;
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, name: &str, text: &str) {
    std::fs::write(path.join(name), text).unwrap();
}

/// The typical fixture: an endpoint, a model, `${OPENAI_API_KEY}`, one plain header, one
/// interpolated header, a theme, a compaction preference and two keybindings.
fn typical(dir: &Path) {
    write(
        dir,
        "settings.json",
        r#"{
  "defaultProvider": "openai",
  "defaultModel": "gpt-5",
  "theme": "dark",
  "compaction": { "enabled": false, "reserveTokens": 16384 },
  "defaultThinkingLevel": "high",
  "defaultTools": ["read", "bash"]
}"#,
    );
    write(
        dir,
        "models.json",
        r#"{
  "providers": {
    "openai": {
      "baseUrl": "https://api.openai.com/v1",
      "api": "openai-responses",
      "apiKey": "${OPENAI_API_KEY}",
      "headers": { "x-tenant": "team", "x-trace": "Bearer ${TRACE_TOKEN}" },
      "models": [{ "id": "gpt-5" }, { "id": "gpt-5-mini" }]
    }
  },
  "modelOverrides": { "gpt-5": { "reasoning": true } }
}"#,
    );
    write(
        dir,
        "keybindings.json",
        r#"{
  "tui.input.submit": "ctrl+s",
  "tui.altScreen.pageUp": ["ctrl+u"],
  "app.session.new": "ctrl+shift+n"
}"#,
    );
}

/// The draft must be accepted by the very reader that will load it, and it must contain only
/// keys that reader implements (nothing lands in `skipped`).
#[test]
fn the_typical_fixture_maps_the_endpoint_model_key_and_preferences() {
    let dir = Temp::new("typical");
    typical(dir.path());
    let draft = import_pi(dir.path()).unwrap();

    let config = radian_host::parse_config(Path::new("/tmp/draft.toml"), &draft.toml)
        .expect("the draft must be a valid Radian configuration");
    assert_eq!(
        config.model.base_url.as_deref(),
        Some("https://api.openai.com/v1")
    );
    assert_eq!(config.model.model.as_deref(), Some("gpt-5"));
    assert_eq!(config.model.provider.as_deref(), Some("openai-compatible"));
    assert_eq!(config.model.api_key_env.as_deref(), Some("OPENAI_API_KEY"));
    assert_eq!(
        config.model.headers,
        vec![("x-tenant".to_string(), "team".to_string())]
    );
    assert_eq!(config.tui.theme.as_deref(), Some("dark"));
    assert_eq!(config.compaction.auto, Some(false));
    assert_eq!(
        config.tui.keybindings,
        vec![("send".to_string(), vec!["ctrl-s".to_string()])]
    );
    assert!(
        config.skipped.is_empty(),
        "the draft must not contain known-but-unwired keys: {:?}",
        config.skipped
    );
    // The interpolated header cannot be expressed and the Pi-only settings are reported.
    assert!(draft.notes.iter().any(|note| note.contains("x-trace")));
    assert!(draft.notes.iter().any(|note| note.contains("defaultTools")));
    assert!(
        draft
            .notes
            .iter()
            .any(|note| note.contains("modelOverrides"))
    );
}

/// A literal key is never copied: the draft references a suggested environment variable and
/// neither the draft nor the report contains the value.
#[test]
fn a_literal_key_becomes_an_environment_reference_and_is_never_shown() {
    let dir = Temp::new("literal");
    write(
        dir.path(),
        "settings.json",
        r#"{ "defaultProvider": "ollama" }"#,
    );
    write(
        dir.path(),
        "models.json",
        r#"{ "providers": { "ollama": {
             "baseUrl": "http://localhost:11434/v1",
             "api": "openai-completions",
             "apiKey": "sk-live-abcdef1234567890",
             "models": [{ "id": "qwen2.5-coder:7b" }] } } }"#,
    );
    let draft = import_pi(dir.path()).unwrap();
    let config = radian_host::parse_config(Path::new("/tmp/draft.toml"), &draft.toml).unwrap();
    assert_eq!(config.model.api_key_env.as_deref(), Some("OLLAMA_API_KEY"));
    assert!(
        !draft.toml.contains("sk-live"),
        "the literal value must not be in the draft"
    );
    for note in &draft.notes {
        assert!(
            !note.contains("sk-live"),
            "the literal value must not be in a note: {note}"
        );
    }
    assert!(
        draft.notes.iter().any(|note| note.contains("literal")),
        "{:?}",
        draft.notes
    );
}

/// `!command` is listed, never executed.
#[test]
fn a_command_key_is_listed_and_never_executed() {
    let dir = Temp::new("command");
    let marker = dir.path().join("PWNED");
    write(dir.path(), "settings.json", "{}");
    write(
        dir.path(),
        "models.json",
        &format!(
            r#"{{ "providers": {{ "acme": {{
                 "baseUrl": "https://acme.example/v1",
                 "api": "openai-completions",
                 "apiKey": "!touch {}",
                 "models": [{{ "id": "m" }}] }} }} }}"#,
            marker.display()
        ),
    );
    let draft = import_pi(dir.path()).unwrap();
    assert!(!marker.exists(), "the command must not have run");
    assert!(
        draft.notes.iter().any(|note| note.contains("!command")),
        "{:?}",
        draft.notes
    );
    let config = radian_host::parse_config(Path::new("/tmp/draft.toml"), &draft.toml).unwrap();
    assert_eq!(config.model.api_key_env, None);
}

/// `auth.json` is never opened — not even `stat`ed — so a credential file that cannot be read
/// (mode `0000`) neither fails the migration nor leaks a byte.
#[cfg(unix)]
#[test]
fn auth_json_is_never_read() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = Temp::new("auth");
    typical(dir.path());
    let auth = dir.path().join("auth.json");
    std::fs::write(&auth, r#"{"openai":{"type":"api_key","key":"sk-SECRET"}}"#).unwrap();
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o000)).unwrap();

    let draft = import_pi(dir.path()).expect("an unreadable auth.json must not matter");
    assert!(!draft.toml.contains("SECRET"));
    for note in &draft.notes {
        assert!(!note.contains("SECRET"), "{note}");
    }
    assert!(
        draft.notes.iter().any(|note| note.contains("auth.json")),
        "the report must say credentials are not migrated"
    );
    // The fixture is only readable again so the temporary directory can be removed.
    std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// A base URL that carries credentials is a secret: it is not migrated.
#[test]
fn a_base_url_with_credentials_is_not_migrated() {
    let dir = Temp::new("url-credentials");
    write(
        dir.path(),
        "settings.json",
        r#"{ "defaultProvider": "acme" }"#,
    );
    write(
        dir.path(),
        "models.json",
        r#"{ "providers": { "acme": {
             "baseUrl": "https://user:sk-SECRET@acme.example/v1",
             "api": "openai-completions",
             "models": [{ "id": "m" }] } } }"#,
    );
    let draft = import_pi(dir.path()).unwrap();
    assert!(!draft.toml.contains("SECRET"), "{}", draft.toml);
    for note in &draft.notes {
        assert!(!note.contains("SECRET"), "{note}");
    }
    let config = radian_host::parse_config(Path::new("/tmp/draft.toml"), &draft.toml).unwrap();
    assert_eq!(config.model.base_url, None);
}

/// A missing file is skipped; a malformed one names the file and the position.
#[test]
fn a_missing_file_is_skipped_and_a_malformed_one_names_the_file() {
    let dir = Temp::new("malformed");
    write(
        dir.path(),
        "settings.json",
        r#"{ "defaultProvider": "openai" }"#,
    );
    let draft = import_pi(dir.path()).unwrap();
    assert!(draft.notes.iter().any(|note| note.contains("models.json")));

    let broken = Temp::new("broken");
    write(broken.path(), "settings.json", "not json {");
    let error = import_pi(broken.path()).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("settings.json"), "{text}");
    assert!(text.contains("line 1"), "{text}");
    assert!(matches!(error, PiMigrateError::Parse { .. }));
}

/// A Pi binding whose key is already another Radian action's default is dropped, because the
/// TUI refuses to start on that conflict.
#[test]
fn a_keybinding_that_collides_with_a_radian_default_is_skipped() {
    let dir = Temp::new("keybinding-collision");
    write(dir.path(), "settings.json", "{}");
    write(
        dir.path(),
        "keybindings.json",
        r#"{ "tui.altScreen.pageUp": "ctrl+u", "tui.altScreen.searchNext": "enter",
             "tui.input.submit": "ctrl+s", "app.suspend": "ctrl+z" }"#,
    );
    let draft = import_pi(dir.path()).unwrap();
    let config = radian_host::parse_config(Path::new("/tmp/draft.toml"), &draft.toml).unwrap();
    assert_eq!(
        config.tui.keybindings,
        vec![("send".to_string(), vec!["ctrl-s".to_string()])],
        "only the non-colliding binding is emitted"
    );
    assert!(draft.notes.iter().any(|note| note.contains("ctrl-u")));
    assert!(draft.notes.iter().any(|note| note.contains("find_prev")));
    assert!(draft.notes.iter().any(|note| note.contains("app.suspend")));
}

/// `--from` must be a directory.
#[test]
fn a_missing_directory_is_refused() {
    let dir = Temp::new("no-dir");
    let missing = dir.path().join("nope");
    assert!(matches!(
        import_pi(&missing).unwrap_err(),
        PiMigrateError::NotADirectory { .. } | PiMigrateError::Read { .. }
    ));
    // A file where a directory is expected is refused as such.
    write(dir.path(), "settings.json", "{}");
    assert!(matches!(
        import_pi(&dir.path().join("settings.json")).unwrap_err(),
        PiMigrateError::NotADirectory { .. }
    ));
}

/// `--write`: the target is created owner-only, reads back through the reader, and a second
/// write refuses and leaves the bytes alone.
#[cfg(unix)]
#[test]
fn the_write_is_owner_only_atomic_and_never_overwrites() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = Temp::new("write");
    typical(dir.path());
    let draft = import_pi(dir.path()).unwrap();
    let target = dir.path().join("config").join("config.toml");
    write_draft(&target, &draft.toml).unwrap();

    let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the file is owner-only");
    let parent = std::fs::metadata(target.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(parent, 0o700, "the directory is owner-only");
    let text = std::fs::read_to_string(&target).unwrap();
    radian_host::parse_config(&target, &text).expect("what landed on disk must read back");
    assert!(
        !target.with_file_name("config.toml.tmp-0").exists(),
        "no temporary file is left behind"
    );

    let before = std::fs::read(&target).unwrap();
    let error = write_draft(&target, "schema_version = 1\n").unwrap_err();
    assert!(matches!(error, PiMigrateError::TargetExists { .. }));
    assert_eq!(std::fs::read(&target).unwrap(), before, "bytes unchanged");
}

/// An invalid draft is refused by the writer before it reaches the disk.
#[test]
fn an_invalid_draft_is_never_written() {
    let dir = Temp::new("invalid-draft");
    let target = dir.path().join("config.toml");
    let error = write_draft(&target, "schema_version = 0\n").unwrap_err();
    assert!(matches!(error, PiMigrateError::InvalidDraft { .. }));
    assert!(!target.exists());
}
