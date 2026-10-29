//! `radian_host::config::migrate_claude`: the Claude Code → Radian draft.
//!
//! Every fixture is a temporary directory this test creates. Nothing here reads the user's
//! real `~/.claude` (there is no default directory at all).

use std::path::{Path, PathBuf};

use radian_host::config::migrate_claude::{
    ClaudeMigrateError, ClaudeTargets, import_claude, write_claude_draft,
};

/// A temporary directory, removed on drop.
struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        // A process-wide counter keeps every call unique, independent of clock
        // resolution: macOS `SystemTime` has microsecond resolution, so two parallel
        // tests that use the same name can otherwise get the same directory.
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("claude-migrate-{name}-{n}-{}", std::process::id(),));
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

fn write(path: &Path, rel: &str, text: &str) {
    let full = path.join(rel);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&full, text).unwrap();
}

const SECRET: &str = "sk-live-abcdef1234567890abcdef";

/// The user-level fixture: commands (one with frontmatter + `$ARGUMENTS`, one bare), a
/// `skills/` dir, a `.claude.json` with `mcpServers` (one carrying a plaintext secret), a
/// `settings.json` with portable and dropped hooks, a `CLAUDE.md`, dropped items, and an
/// unreadable credential file.
fn user_home(dir: &Path) {
    write(
        dir,
        "commands/review.md",
        "---\ndescription: Review a PR diff.\n---\n\nReview $ARGUMENTS now.\n",
    );
    write(dir, "commands/plan.md", "Plan $ARGUMENTS please.\n");
    write(
        dir,
        "skills/extra/SKILL.md",
        "---\nname: extra\ndescription: Extra skill.\n---\n\nBody.\n",
    );
    write(
        dir,
        ".claude.json",
        &format!(
            r#"{{
  "mcpServers": {{
    "fs": {{ "command": "npx", "args": ["-y", "server"], "env": {{ "WITH_DEFAULT": "x" }} }},
    "secret-srv": {{ "command": "node", "env": {{ "API_KEY": "{SECRET}" }} }}
  }}
}}"#
        ),
    );
    write(
        dir,
        "settings.json",
        r#"{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo pre", "timeout": 10 }] },
      { "matcher": "Edit|Write", "hooks": [{ "type": "command", "command": "echo edit" }] },
      { "matcher": "Read", "hooks": [{ "type": "command", "command": "echo read" }] },
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo async-inner", "async": true }] },
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo late" }], "async": true },
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo cond", "if": "true" }] },
      { "matcher": "Bash", "hooks": [{ "type": "http", "url": "https://example.com/hook" }] },
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo fork", "context": "fork" }] }
    ],
    "PostToolUse": [
      { "matcher": "Edit", "hooks": [{ "type": "command", "command": "make lint" }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "make check" }] }
    ],
    "SessionStart": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo hi" }] }
    ]
  }
}"#,
    );
    write(dir, "CLAUDE.md", "# My Claude\n\nBe nice.\n");
    write(dir, "agents/old.md", "old agent\n");
    write(dir, "plugins/p.json", "{}\n");
    write(dir, "CLAUDE.local.md", "local\n");
    write(dir, "MEMORY.md", "memory\n");
    // Credential files the import must never touch (mode 0000, like `import-pi`'s test).
    write(dir, ".credentials.json", r#"{"secret":"VALUE"}"#);
    write(dir, "auth.json", r#"{"secret":"VALUE"}"#);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for name in [".credentials.json", "auth.json"] {
            std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o000))
                .unwrap();
        }
    }
}

/// The project-level fixture: a command, a skill, a `.mcp.json` (never translated), project
/// hooks (reported, never translated), and dropped items.
fn project(dir: &Path) {
    write(
        dir,
        ".claude/commands/deploy.md",
        "---\ndescription: Deploy the app.\n---\n\nDeploy $ARGUMENTS.\n",
    );
    write(
        dir,
        ".claude/skills/team/SKILL.md",
        "---\nname: team\ndescription: Team skill.\n---\n\nTeam body.\n",
    );
    write(
        dir,
        ".mcp.json",
        r#"{"mcpServers": {"db": {"command": "db-server"}}}"#,
    );
    write(
        dir,
        ".claude/settings.json",
        r#"{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo proj" }] }
    ]
  }
}"#,
    );
    write(dir, ".claude/rules/style.md", "style\n");
    write(dir, ".claude/agents/helper.md", "helper\n");
    write(dir, "CLAUDE.local.md", "local\n");
}

#[cfg(unix)]
fn readable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    for name in [".credentials.json", "auth.json"] {
        let _ = std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o600));
    }
}

#[cfg(not(unix))]
fn readable(_dir: &Path) {}

/// The typical fixtures map commands, skills, `CLAUDE.md` and hooks; the config draft is
/// accepted by the real config reader (including `[hooks]`), and every generated skill is
/// found by the real skill discovery.
#[test]
fn the_typical_fixtures_map_commands_hooks_and_agents_md() {
    let home = Temp::new("typical-home");
    let proj = Temp::new("typical-proj");
    user_home(home.path());
    project(proj.path());

    let draft = import_claude(Some(home.path()), Some(proj.path())).unwrap();

    // The config draft parses and carries the mapped keys.
    let config = radian_host::parse_config(Path::new("/tmp/draft.toml"), &draft.config_toml)
        .expect("the draft must be a valid Radian configuration");
    assert!(
        config.skipped.is_empty(),
        "the draft must not contain known-but-unwired keys: {:?}",
        config.skipped
    );
    assert_eq!(config.skills.extra_paths.len(), 1);
    assert!(
        config.skills.extra_paths[0].ends_with("skills"),
        "{:?}",
        config.skills.extra_paths
    );
    // Tool names are translated once, here: Bash→shell, Edit|Write→patch|write, Read→read.
    let matchers: Vec<Option<String>> = config
        .hooks
        .pre_tool_use
        .iter()
        .map(|entry| entry.matcher.clone())
        .collect();
    assert!(matchers.contains(&Some("shell".to_owned())), "{matchers:?}");
    assert!(
        matchers.contains(&Some("patch|write".to_owned())),
        "{matchers:?}"
    );
    assert!(matchers.contains(&Some("read".to_owned())), "{matchers:?}");
    assert_eq!(config.hooks.post_tool_use.len(), 1);
    assert_eq!(
        config.hooks.post_tool_use[0].matcher.as_deref(),
        Some("patch")
    );
    assert_eq!(config.hooks.stop.len(), 1);
    assert!(
        draft
            .notes
            .iter()
            .any(|note| note.contains("file_path") && note.contains(".path")),
        "the field-rename hint must be reported: {:?}",
        draft.notes
    );

    // Commands keep `$ARGUMENTS` verbatim and gain a `name`.
    assert_eq!(draft.user_skills.len(), 2);
    let review = draft
        .user_skills
        .iter()
        .find(|skill| skill.name == "review")
        .expect("review skill");
    assert!(
        review.content.contains("Review $ARGUMENTS now."),
        "{}",
        review.content
    );
    assert!(
        review.content.contains("description: Review a PR diff."),
        "{}",
        review.content
    );
    assert_eq!(draft.project_skills.len(), 2);

    // `CLAUDE.md` is carried verbatim.
    assert_eq!(
        draft.user_agents_md.as_deref(),
        Some("# My Claude\n\nBe nice.\n")
    );

    // Every generated skill is found by the real skill discovery.
    let scratch = Temp::new("typical-write");
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: Some(scratch.path().join("project-skills")),
        project_hooks_path: Some(scratch.path().join("project-hooks.json")),
    };
    let report = write_claude_draft(&draft, &targets).unwrap();
    assert!(!report.written.is_empty());
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    let user_found = radian_host::discover(&targets.user_skills_dir);
    for skill in &draft.user_skills {
        assert!(
            user_found
                .skills
                .iter()
                .any(|found| found.name == skill.name),
            "user skill `{}` must be discovered: {:?}",
            skill.name,
            user_found.skills
        );
    }
    let proj_found = radian_host::discover(targets.project_skills_dir.as_ref().unwrap());
    for skill in &draft.project_skills {
        assert!(
            proj_found
                .skills
                .iter()
                .any(|found| found.name == skill.name),
            "project skill `{}` must be discovered: {:?}",
            skill.name,
            proj_found.skills
        );
    }
    readable(home.path());
}

/// The report names every non-portable item: dropped hook keys, unknown events, agents,
/// plugins, LSP, rules, local files, memory/sessions, the never-translated `.mcp.json`
/// notice — alongside the new import lines for `[mcp]` servers and project hooks.
#[test]
fn the_drop_report_names_every_non_portable_item() {
    let home = Temp::new("drops-home");
    let proj = Temp::new("drops-proj");
    user_home(home.path());
    project(proj.path());

    let draft = import_claude(Some(home.path()), Some(proj.path())).unwrap();
    let report = draft.notes.join("\n");
    for needle in [
        "`async`",
        "`if`",
        "http",
        "fork",
        "SessionStart",
        "agents",
        "plugins",
        "LSP",
        "rules",
        "CLAUDE.local.md",
        "memory",
        "sessions",
        "imported 2 stdio server(s) into `[mcp]`",
        "imported 1 hook entry as project hooks",
        "not translated; the runtime reads it",
        "`.mcp.json`",
    ] {
        assert!(
            report.contains(needle),
            "the report must name `{needle}`:\n{report}"
        );
    }
    readable(home.path());
}

/// Credential files are never read: unreadable `.credentials.json`/`auth.json` (mode
/// `0000`) neither fail the import nor appear in the read log. A mutation that opens
/// either file fails this test.
#[test]
#[cfg(unix)]
fn credential_files_are_never_read() {
    let home = Temp::new("creds-home");
    user_home(home.path());

    let draft = import_claude(Some(home.path()), None)
        .expect("unreadable credential files must not matter");
    for path in &draft.files_read {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        assert!(
            name != ".credentials.json" && name != "auth.json",
            "the read log must never mention credential files: {}",
            path.display()
        );
    }
    for note in &draft.notes {
        assert!(
            !note.contains(".credentials.json\"") && !note.contains("VALUE"),
            "no credential value may leak into the report: {note}"
        );
    }
    assert!(
        draft.notes.iter().any(|note| note.contains("never read")),
        "the report must say credentials are not migrated: {:?}",
        draft.notes
    );
    assert!(!draft.config_toml.contains("VALUE"));
    readable(home.path());
}

/// A plaintext secret under a sensitive name is never copied: the draft carries no
/// value, only an `inherit_env` entry (no string may stand under such a name), and
/// the report names the variable to set in the shell. A mutation that copies the
/// literal fails this test.
#[test]
fn a_literal_secret_under_a_sensitive_name_is_never_shown_and_rides_inherit_env() {
    let home = Temp::new("secret-home");
    user_home(home.path());

    let draft = import_claude(Some(home.path()), None).unwrap();
    assert!(
        !draft.config_toml.contains(SECRET),
        "the literal must not be in the draft: {}",
        draft.config_toml
    );
    for note in &draft.notes {
        assert!(
            !note.contains(SECRET),
            "the literal value must not be in a note: {note}"
        );
    }
    assert!(
        draft.notes.iter().any(|note| note.contains("inherit_env")
            && note.contains("`API_KEY`")
            && note.contains("value is never shown")),
        "the note must name inherit_env and the variable: {:?}",
        draft.notes
    );
    // The written entry carries the name only, never a value.
    assert!(
        draft.config_toml.contains("API_KEY"),
        "the name rides inherit_env: {}",
        draft.config_toml
    );
    let server = draft
        .mcp_servers
        .iter()
        .find(|server| server.id == "secret-srv")
        .expect("secret-srv must be imported");
    assert!(
        server.env.is_empty(),
        "no value may land in env: {server:?}"
    );
    assert!(
        server.inherit_env.contains(&"API_KEY".to_owned()),
        "{server:?}"
    );
    readable(home.path());
}

/// A plaintext secret under an ordinary name becomes a `${VAR}` expansion with a
/// suggested name: the literal appears neither in the draft nor in any note. A
/// mutation that writes the literal value instead of the reference fails this test.
#[test]
fn a_literal_secret_under_an_ordinary_name_becomes_a_reference() {
    let home = Temp::new("secret-ref-home");
    write(
        home.path(),
        ".claude.json",
        &format!(
            r#"{{"mcpServers": {{"svc": {{"command": "serve", "env": {{"MY_KEY": "{SECRET}"}}}}}}}}"#
        ),
    );

    let draft = import_claude(Some(home.path()), None).unwrap();
    assert!(
        !draft.config_toml.contains(SECRET),
        "the literal must not be in the draft: {}",
        draft.config_toml
    );
    for note in &draft.notes {
        assert!(
            !note.contains(SECRET),
            "the literal value must not be in a note: {note}"
        );
    }
    assert!(
        draft.notes.iter().any(|note| note.contains("SVC_MY_KEY")
            && note.contains("${SVC_MY_KEY}")
            && note.contains("value is never shown")),
        "the note must suggest an env reference: {:?}",
        draft.notes
    );
    assert!(
        draft.config_toml.contains("${SVC_MY_KEY}"),
        "the draft carries the reference: {}",
        draft.config_toml
    );
    readable(home.path());
}

/// The merge promise: an existing `config.toml` survives byte-for-byte
/// (comment, unrelated keys, existing hook entries) while the imported `extra_paths` entry
/// and hook entries are appended. A mutation that writes the draft whole fails this test.
#[test]
fn existing_config_is_merged_preserving_everything() {
    let home = Temp::new("merge-home");
    let proj = Temp::new("merge-proj");
    user_home(home.path());
    project(proj.path());
    let draft = import_claude(Some(home.path()), Some(proj.path())).unwrap();

    let scratch = Temp::new("merge-scratch");
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: Some(scratch.path().join("project-skills")),
        project_hooks_path: Some(scratch.path().join("project-hooks.json")),
    };
    let original = "# my configuration\n\
        schema_version = 1\n\
        \n\
        [model]\n\
        model = \"m-1\"\n\
        \n\
        [skills]\n\
        extra_paths = [\"keep\"]\n\
        \n\
        [[hooks.PreToolUse]]\n\
        matcher = \"read\"\n\
        [[hooks.PreToolUse.hooks]]\n\
        type = \"command\"\n\
        command = \"existing\"\n";
    std::fs::write(&targets.config_path, original).unwrap();

    let report = write_claude_draft(&draft, &targets).unwrap();
    assert!(report.skipped.is_empty(), "{report:?}");
    let after = std::fs::read_to_string(&targets.config_path).unwrap();
    // Everything that existed is byte-identical: the comment, the unrelated key, the kept
    // path entry and the existing hook entry.
    assert!(after.contains("# my configuration"), "{after}");
    assert!(after.contains("model = \"m-1\""), "{after}");
    assert!(after.contains("\"keep\""), "{after}");
    assert!(after.contains("command = \"existing\""), "{after}");
    // The import landed alongside it.
    let imported_path = home.path().join("skills").display().to_string();
    assert!(after.contains(&imported_path), "{after}");
    assert!(after.contains("command = \"echo pre\""), "{after}");
    let config = radian_host::parse_config(&targets.config_path, &after).unwrap();
    assert!(config.skipped.is_empty(), "{config:?}");
    assert!(config.skills.extra_paths.contains(&"keep".to_owned()));
    assert!(config.skills.extra_paths.contains(&imported_path));
    assert!(
        config
            .hooks
            .pre_tool_use
            .iter()
            .any(|entry| entry.hooks.iter().any(|hook| hook.command == "existing"))
    );
    assert!(
        config
            .hooks
            .pre_tool_use
            .iter()
            .any(|entry| entry.hooks.iter().any(|hook| hook.command == "echo pre"))
    );
    readable(home.path());
}

/// Importing twice adds nothing: no duplicate `extra_paths`, no duplicate hook entries. A
/// mutation that drops the dedupe fails this test.
#[test]
fn a_second_import_adds_nothing() {
    let home = Temp::new("twice-home");
    let proj = Temp::new("twice-proj");
    user_home(home.path());
    project(proj.path());
    let draft = import_claude(Some(home.path()), Some(proj.path())).unwrap();

    let scratch = Temp::new("twice-scratch");
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: Some(scratch.path().join("project-skills")),
        project_hooks_path: Some(scratch.path().join("project-hooks.json")),
    };
    let first = write_claude_draft(&draft, &targets).unwrap();
    assert!(!first.written.is_empty());
    let before = std::fs::read(&targets.config_path).unwrap();

    let second = write_claude_draft(&draft, &targets).unwrap();
    assert!(second.written.is_empty(), "{second:?}");
    assert_eq!(
        std::fs::read(&targets.config_path).unwrap(),
        before,
        "a second import changes nothing"
    );
    let after = std::fs::read_to_string(&targets.config_path).unwrap();
    assert_eq!(after.matches("\"keep\"").count(), 0, "{after}");
    assert_eq!(after.matches("echo pre").count(), 1, "{after}");
    let imported_path = home.path().join("skills").display().to_string();
    assert_eq!(after.matches(&imported_path).count(), 1, "{after}");
    readable(home.path());
}

/// An inline `skills = { extra_paths = [...] }` table merges: the comment and the existing
/// entry stay byte-identical, the new path is appended inline, and a second run adds
/// nothing. Against the old table-only code this fails with an internal error.
#[test]
fn an_inline_skills_table_merges() {
    let home = Temp::new("inline-skills-home");
    user_home(home.path());
    let draft = import_claude(Some(home.path()), None).unwrap();

    let scratch = Temp::new("inline-skills-scratch");
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: None,
        project_hooks_path: None,
    };
    let original = "schema_version = 1\n# keep me\nskills = { extra_paths = [\"/a\"] }\n";
    std::fs::write(&targets.config_path, original).unwrap();

    let report = write_claude_draft(&draft, &targets).unwrap();
    assert!(report.skipped.is_empty(), "{report:?}");
    let after = std::fs::read_to_string(&targets.config_path).unwrap();
    assert!(after.contains("# keep me"), "{after}");
    assert!(
        after.contains("skills = {"),
        "the inline form stays inline: {after}"
    );
    let imported = home.path().join("skills").display().to_string();
    assert!(after.contains(&imported), "{after}");
    let config = radian_host::parse_config(&targets.config_path, &after).unwrap();
    assert!(config.skipped.is_empty(), "{config:?}");
    assert_eq!(config.skills.extra_paths.len(), 2);
    assert!(config.skills.extra_paths.contains(&"/a".to_owned()));
    assert!(config.skills.extra_paths.contains(&imported));

    // A second run appends nothing.
    let second = write_claude_draft(&draft, &targets).unwrap();
    assert!(second.written.is_empty(), "{second:?}");
    let again = std::fs::read_to_string(&targets.config_path).unwrap();
    assert_eq!(again.matches(&imported).count(), 1, "{again}");
    assert_eq!(again.matches("\"/a\"").count(), 1, "{again}");
    readable(home.path());
}

/// An inline hooks event list (`PreToolUse = [{ ... }]` inside `[hooks]`) merges: the
/// existing entry stays, the imported one is appended in the same inline form, and a
/// second run adds nothing. Against the old table-only code this fails with an internal
/// error.
#[test]
fn an_inline_hooks_list_merges() {
    let home = Temp::new("inline-hooks-home");
    user_home(home.path());
    let draft = import_claude(Some(home.path()), None).unwrap();

    let scratch = Temp::new("inline-hooks-scratch");
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: None,
        project_hooks_path: None,
    };
    let original = "schema_version = 1\n[hooks]\n\
        PreToolUse = [{ matcher = \"shell\", hooks = [{ type = \"command\", \
        command = \"existing\" }] }]\n";
    std::fs::write(&targets.config_path, original).unwrap();

    let report = write_claude_draft(&draft, &targets).unwrap();
    assert!(report.skipped.is_empty(), "{report:?}");
    let after = std::fs::read_to_string(&targets.config_path).unwrap();
    assert!(after.contains("command = \"existing\""), "{after}");
    assert!(after.contains("command = \"echo pre\""), "{after}");
    let config = radian_host::parse_config(&targets.config_path, &after).unwrap();
    assert!(
        config
            .hooks
            .pre_tool_use
            .iter()
            .any(|entry| entry.hooks.iter().any(|hook| hook.command == "existing"))
    );
    assert!(
        config
            .hooks
            .pre_tool_use
            .iter()
            .any(|entry| entry.hooks.iter().any(|hook| hook.command == "echo pre"))
    );

    let second = write_claude_draft(&draft, &targets).unwrap();
    assert!(second.written.is_empty(), "{second:?}");
    let again = std::fs::read_to_string(&targets.config_path).unwrap();
    assert_eq!(again.matches("echo pre").count(), 1, "{again}");
    readable(home.path());
}

/// A top-level inline `hooks = { ... }` table merges like a `[hooks]` table (the reader
/// loads both the same way): the existing entries stay, the imported ones are appended
/// inline, and a second run adds nothing.
#[test]
fn a_top_level_inline_hooks_table_merges() {
    let home = Temp::new("inline-top-home");
    user_home(home.path());
    let draft = import_claude(Some(home.path()), None).unwrap();

    let scratch = Temp::new("inline-top-scratch");
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: None,
        project_hooks_path: None,
    };
    let original = "schema_version = 1\nhooks = { PreToolUse = [] }\n";
    std::fs::write(&targets.config_path, original).unwrap();

    let report = write_claude_draft(&draft, &targets).unwrap();
    assert!(report.skipped.is_empty(), "{report:?}");
    let after = std::fs::read_to_string(&targets.config_path).unwrap();
    assert!(
        after.contains("hooks = {"),
        "the inline form stays inline: {after}"
    );
    assert!(after.contains("command = \"echo pre\""), "{after}");
    let config = radian_host::parse_config(&targets.config_path, &after).unwrap();
    assert!(
        config
            .hooks
            .pre_tool_use
            .iter()
            .any(|entry| entry.hooks.iter().any(|hook| hook.command == "echo pre"))
    );
    assert!(
        config
            .hooks
            .stop
            .iter()
            .any(|entry| entry.hooks.iter().any(|hook| hook.command == "make check"))
    );

    let second = write_claude_draft(&draft, &targets).unwrap();
    assert!(second.written.is_empty(), "{second:?}");
    let again = std::fs::read_to_string(&targets.config_path).unwrap();
    assert_eq!(again.matches("echo pre").count(), 1, "{again}");
    readable(home.path());
}

/// A non-table `hooks` value is not a shape the merge can extend, so it is left alone
/// with one conflict line naming the key — never an internal error — while the rest of
/// the import still lands.
#[test]
fn a_non_table_hooks_value_is_a_conflict_not_an_error() {
    let home = Temp::new("inline-top-home");
    user_home(home.path());
    let draft = import_claude(Some(home.path()), None).unwrap();

    let scratch = Temp::new("inline-top-scratch");
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: None,
        project_hooks_path: None,
    };
    let original = "schema_version = 1\nhooks = \"not-a-table\"\n";
    std::fs::write(&targets.config_path, original).unwrap();

    let report = write_claude_draft(&draft, &targets).unwrap();
    assert!(
        report.skipped.iter().any(|line| line.contains("`hooks`")),
        "{report:?}"
    );
    // The rest still landed: the extra_paths entry is there and skills were written.
    let after = std::fs::read_to_string(&targets.config_path).unwrap();
    let imported = home.path().join("skills").display().to_string();
    assert!(after.contains(&imported), "{after}");
    assert!(targets.user_agents_path.exists());
    readable(home.path());
}

/// Existing `AGENTS.md`/`SKILL.md` targets are skipped with one report line each and keep
/// their bytes, while the other targets are still written. A mutation that overwrites
/// fails this test.
#[test]
fn existing_files_are_skipped_and_other_targets_still_written() {
    let home = Temp::new("skip-home");
    let proj = Temp::new("skip-proj");
    user_home(home.path());
    project(proj.path());
    let draft = import_claude(Some(home.path()), Some(proj.path())).unwrap();

    let scratch = Temp::new("skip-scratch");
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: Some(scratch.path().join("project-skills")),
        project_hooks_path: Some(scratch.path().join("project-hooks.json")),
    };
    let agents_path = targets.user_agents_path.clone();
    let review_path = targets.user_skills_dir.join("review").join("SKILL.md");
    std::fs::create_dir_all(review_path.parent().unwrap()).unwrap();
    std::fs::write(&agents_path, "MINE\n").unwrap();
    std::fs::write(&review_path, "MINE SKILL\n").unwrap();

    let report = write_claude_draft(&draft, &targets).unwrap();
    assert_eq!(std::fs::read_to_string(&agents_path).unwrap(), "MINE\n");
    assert_eq!(
        std::fs::read_to_string(&review_path).unwrap(),
        "MINE SKILL\n"
    );
    for path in [&agents_path, &review_path] {
        assert!(
            report
                .skipped
                .iter()
                .any(|line| line.contains(&path.display().to_string())
                    && line.contains("exists, not written; the draft is in the dry-run output")),
            "{report:?}"
        );
    }
    // The other targets still landed.
    assert!(targets.config_path.exists());
    let plan = targets.user_skills_dir.join("plan").join("SKILL.md");
    assert!(
        std::fs::read_to_string(&plan)
            .unwrap()
            .contains("$ARGUMENTS")
    );
    assert!(
        targets
            .project_skills_dir
            .as_ref()
            .unwrap()
            .join("team")
            .join("SKILL.md")
            .exists()
    );
    readable(home.path());
}

/// A read-only configuration (a newer `schema_version`) stops the whole write with an
/// error and nothing is written: no config change, no skill files, no `AGENTS.md`.
#[test]
fn a_read_only_config_writes_nothing() {
    let home = Temp::new("readonly-home");
    let proj = Temp::new("readonly-proj");
    user_home(home.path());
    project(proj.path());
    let draft = import_claude(Some(home.path()), Some(proj.path())).unwrap();

    let scratch = Temp::new("readonly-scratch");
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: Some(scratch.path().join("project-skills")),
        project_hooks_path: Some(scratch.path().join("project-hooks.json")),
    };
    let original = "schema_version = 99\n";
    std::fs::write(&targets.config_path, original).unwrap();

    let error = write_claude_draft(&draft, &targets).unwrap_err();
    assert!(
        matches!(error, ClaudeMigrateError::ReadOnly { .. }),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(&targets.config_path).unwrap(),
        original
    );
    assert!(!targets.user_agents_path.exists());
    assert!(!targets.user_skills_dir.exists());
    assert!(!targets.project_skills_dir.as_ref().unwrap().exists());
    readable(home.path());
}

/// At least one source is required.
#[test]
fn no_source_is_refused() {
    assert!(matches!(
        import_claude(None, None).unwrap_err(),
        ClaudeMigrateError::MissingSource
    ));
}

/// A non-directory source is refused.
#[test]
fn a_missing_directory_is_refused() {
    let dir = Temp::new("no-dir");
    let missing = dir.path().join("nope");
    assert!(matches!(
        import_claude(Some(&missing), None).unwrap_err(),
        ClaudeMigrateError::NotADirectory { .. } | ClaudeMigrateError::Read { .. }
    ));
}

/// An invalid draft never reaches the disk.
#[test]
fn an_invalid_draft_is_never_written() {
    let scratch = Temp::new("invalid-draft");
    let mut draft = import_claude(None, None).unwrap_or_else(|_| ClaudeDraftForTest::empty());
    draft.config_toml = "schema_version = 0\n".to_owned();
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: None,
        project_hooks_path: None,
    };
    let error = write_claude_draft(&draft, &targets).unwrap_err();
    assert!(
        matches!(error, ClaudeMigrateError::InvalidDraft { .. }),
        "{error}"
    );
    assert!(!targets.config_path.exists());
}

struct ClaudeDraftForTest;

impl ClaudeDraftForTest {
    fn empty() -> radian_host::ClaudeDraft {
        radian_host::ClaudeDraft {
            config_toml: String::new(),
            project_hooks_json: None,
            user_agents_md: None,
            user_skills: Vec::new(),
            project_skills: Vec::new(),
            mcp_servers: Vec::new(),
            notes: Vec::new(),
            files_read: Vec::new(),
        }
    }
}

/// A user `.claude.json` with one stdio server, one `sse` server, one server carrying
/// foreign keys and one server with a literal secret in `env`: the merged `config.toml`
/// parses, the MCP loader returns exactly the stdio servers with the right
/// command/args/env, the literal appears neither in the written file nor in the draft
/// nor in any note, and the report names each dropped item. A mutation that writes the
/// literal secret value instead of the reference fails this test.
#[test]
fn user_mcp_servers_translate_with_drops_reported_and_secrets_referenced() {
    unsafe {
        std::env::set_var("SVC_SVC_KEY", "from-test-env");
    }
    let home = Temp::new("mcp-mixed-home");
    write(
        home.path(),
        ".claude.json",
        &format!(
            r#"{{
  "mcpServers": {{
    "files": {{ "command": "npx", "args": ["-y", "server"], "env": {{ "WITH_DEFAULT": "x" }} }},
    "web": {{ "type": "sse", "url": "https://example.com/sse" }},
    "legacy": {{ "command": "old", "disabled": true, "autoApprove": ["x"], "timeout": 30 }},
    "svc": {{ "command": "serve", "env": {{ "SVC_KEY": "{SECRET}" }} }}
  }}
}}"#
        ),
    );

    let draft = import_claude(Some(home.path()), None).unwrap();
    let report = draft.notes.join("\n");
    for needle in [
        "server `web`",
        "type `sse`",
        "`disabled`",
        "`autoApprove`",
        "`timeout`",
        "SVC_KEY",
        "SVC_SVC_KEY",
        "value is never shown",
    ] {
        assert!(
            report.contains(needle),
            "the report must name `{needle}`:\n{report}"
        );
    }
    assert!(!draft.config_toml.contains(SECRET), "{}", draft.config_toml);
    for note in &draft.notes {
        assert!(!note.contains(SECRET), "{note}");
    }

    let scratch = Temp::new("mcp-mixed-scratch");
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: None,
        project_hooks_path: None,
    };
    let write_report = write_claude_draft(&draft, &targets).unwrap();
    assert!(write_report.skipped.is_empty(), "{write_report:?}");
    let after = std::fs::read_to_string(&targets.config_path).unwrap();
    assert!(!after.contains(SECRET), "{after}");
    let config = radian_host::parse_config(&targets.config_path, &after).unwrap();
    assert!(config.skipped.is_empty(), "{config:?}");
    let ids: Vec<&str> = config
        .mcp
        .servers
        .iter()
        .map(|server| server.id.as_str())
        .collect();
    assert_eq!(ids, vec!["files", "legacy", "svc"], "{ids:?}");
    let files = &config.mcp.servers[0];
    assert_eq!(files.command, std::path::PathBuf::from("npx"));
    assert_eq!(
        files.args,
        vec![
            std::ffi::OsString::from("-y"),
            std::ffi::OsString::from("server"),
        ]
    );
    let env: std::collections::BTreeMap<String, String> = files
        .env
        .iter()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
    assert_eq!(env.get("WITH_DEFAULT").map(String::as_str), Some("x"));
    let legacy = &config.mcp.servers[1];
    assert_eq!(legacy.command, std::path::PathBuf::from("old"));
    // The secret reference expands at load: the loader sees the shell value.
    let svc = &config.mcp.servers[2];
    let env: std::collections::BTreeMap<String, String> = svc
        .env
        .iter()
        .map(|(key, value)| {
            (
                key.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
    assert_eq!(
        env.get("SVC_KEY").map(String::as_str),
        Some("from-test-env"),
        "{env:?}"
    );
    unsafe {
        std::env::remove_var("SVC_SVC_KEY");
    }
    readable(home.path());
}

/// An existing `config.toml` that already uses `[[mcp.servers]]` gains new servers in
/// that same form, and the file still loads: no both-forms file is produced. A mutation
/// that always writes `mcpServers` regardless of the existing form fails this test.
#[test]
fn new_servers_follow_the_existing_servers_form() {
    let home = Temp::new("mcp-form-home");
    write(
        home.path(),
        ".claude.json",
        r#"{"mcpServers": {"fresh": {"command": "fresh-cmd", "args": ["a"]}}}"#,
    );
    let draft = import_claude(Some(home.path()), None).unwrap();
    assert_eq!(draft.mcp_servers.len(), 1);

    let scratch = Temp::new("mcp-form-scratch");
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: None,
        project_hooks_path: None,
    };
    let original = "schema_version = 1\n[[mcp.servers]]\nid = \"keep\"\ncommand = \"keep-cmd\"\n";
    std::fs::write(&targets.config_path, original).unwrap();

    let report = write_claude_draft(&draft, &targets).unwrap();
    assert!(report.skipped.is_empty(), "{report:?}");
    let after = std::fs::read_to_string(&targets.config_path).unwrap();
    assert!(after.contains("id = \"keep\""), "{after}");
    assert!(after.contains("id = \"fresh\""), "{after}");
    assert!(
        !after.contains("mcpServers"),
        "no both-forms file may be produced: {after}"
    );
    let config = radian_host::parse_config(&targets.config_path, &after).unwrap();
    assert!(config.skipped.is_empty(), "{config:?}");
    let ids: Vec<&str> = config
        .mcp
        .servers
        .iter()
        .map(|server| server.id.as_str())
        .collect();
    assert_eq!(ids, vec!["keep", "fresh"], "{ids:?}");
    readable(home.path());
}

/// An existing server id is skipped and reported, and the existing entry stays
/// byte-for-byte unchanged while a new server still lands.
#[test]
fn an_existing_server_id_is_skipped_and_reported() {
    let home = Temp::new("mcp-dup-home");
    write(
        home.path(),
        ".claude.json",
        r#"{"mcpServers": {
            "dup": {"command": "other"},
            "fresh": {"command": "fresh-cmd"}}}"#,
    );
    let draft = import_claude(Some(home.path()), None).unwrap();

    let scratch = Temp::new("mcp-dup-scratch");
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: None,
        project_hooks_path: None,
    };
    let original = "schema_version = 1\n[[mcp.servers]]\nid = \"dup\"\ncommand = \"mine\"\n";
    std::fs::write(&targets.config_path, original).unwrap();

    let report = write_claude_draft(&draft, &targets).unwrap();
    assert!(
        report
            .skipped
            .iter()
            .any(|line| line.contains("`dup`") && line.contains("already present")),
        "{report:?}"
    );
    let after = std::fs::read_to_string(&targets.config_path).unwrap();
    assert!(
        after.contains("id = \"dup\"\ncommand = \"mine\""),
        "{after}"
    );
    assert!(!after.contains("other"), "{after}");
    assert!(after.contains("id = \"fresh\""), "{after}");
    let config = radian_host::parse_config(&targets.config_path, &after).unwrap();
    let dup = config
        .mcp
        .servers
        .iter()
        .find(|server| server.id == "dup")
        .expect("the existing server");
    assert_eq!(dup.command, std::path::PathBuf::from("mine"));
    readable(home.path());
}

/// A project `.claude/settings.json` with hooks in and out of the subset: the written
/// `.radian/hooks.json` loads through the project hooks loader, `Bash` matchers became
/// `shell`, and the off-subset entries are reported. A mutation that skips the
/// tool-name translation for project hooks fails this test.
#[test]
fn project_hooks_translate_with_drops_reported() {
    let proj = Temp::new("proj-hooks-proj");
    write(
        proj.path(),
        ".claude/settings.json",
        r#"{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo proj-pre" }] },
      { "matcher": "Edit|Write", "hooks": [{ "type": "command", "command": "echo edit" }] },
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo async", "async": true }] }
    ],
    "SessionStart": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo hi" }] }
    ]
  }
}"#,
    );

    let draft = import_claude(None, Some(proj.path())).unwrap();
    let report = draft.notes.join("\n");
    assert!(report.contains("imported 2 hook entries"), "{report}");
    assert!(report.contains("`async`"), "{report}");
    assert!(report.contains("SessionStart"), "{report}");
    let json = draft
        .project_hooks_json
        .as_deref()
        .expect("importable project hooks render a file");
    assert!(!json.contains("echo async"), "{json}");
    assert!(!json.contains("echo hi"), "{json}");

    let hooks_path = proj.path().join(".radian/hooks.json");
    let targets = ClaudeTargets {
        config_path: proj.path().join("config.toml"),
        user_skills_dir: proj.path().join("user-skills"),
        user_agents_path: proj.path().join("AGENTS.md"),
        project_skills_dir: None,
        project_hooks_path: Some(hooks_path.clone()),
    };
    let write_report = write_claude_draft(&draft, &targets).unwrap();
    assert!(write_report.skipped.is_empty(), "{write_report:?}");
    assert!(hooks_path.is_file());
    let (loaded, warnings) =
        radian_host::load_project_hooks_file(&hooks_path).expect("the written file loads");
    assert!(warnings.is_empty(), "{warnings:?}");
    let matchers: Vec<Option<String>> = loaded
        .pre_tool_use
        .iter()
        .map(|entry| entry.matcher.clone())
        .collect();
    assert!(matchers.contains(&Some("shell".to_owned())), "{matchers:?}");
    assert!(
        matchers.contains(&Some("patch|write".to_owned())),
        "{matchers:?}"
    );
    let commands: Vec<&str> = loaded
        .pre_tool_use
        .iter()
        .flat_map(|entry| entry.hooks.iter())
        .map(|hook| hook.command.as_str())
        .collect();
    assert!(commands.contains(&"echo proj-pre"), "{commands:?}");
    assert!(commands.contains(&"echo edit"), "{commands:?}");
}

/// An existing `.radian/hooks.json` gains only missing entries (nothing it holds is
/// replaced or dropped); a fully-imported file is left byte-identical with one report
/// line; an invalid one is left untouched with one report line. A mutation that
/// overwrites an existing `.radian/hooks.json` fails this test.
#[test]
fn an_existing_project_hooks_file_only_gains_missing_entries() {
    let proj = Temp::new("proj-hooks-merge");
    write(
        proj.path(),
        ".claude/settings.json",
        r#"{"hooks": {
            "PreToolUse": [
              { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo new" }] }
            ]}}"#,
    );
    let draft = import_claude(None, Some(proj.path())).unwrap();
    assert!(draft.project_hooks_json.is_some());

    let hooks_path = proj.path().join(".radian/hooks.json");
    std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
    let keep_only = "{\n  \"PreToolUse\": [\n    {\n      \"matcher\": \"read\",\n      \
        \"hooks\": [\n        {\n          \"type\": \"command\",\n          \
        \"command\": \"echo keep\"\n        }\n      ]\n    }\n  ]\n}\n";
    std::fs::write(&hooks_path, keep_only).unwrap();
    let targets = ClaudeTargets {
        config_path: proj.path().join("config.toml"),
        user_skills_dir: proj.path().join("user-skills"),
        user_agents_path: proj.path().join("AGENTS.md"),
        project_skills_dir: None,
        project_hooks_path: Some(hooks_path.clone()),
    };

    // The missing entry is added alongside what is there.
    let report = write_claude_draft(&draft, &targets).unwrap();
    assert!(report.skipped.is_empty(), "{report:?}");
    assert!(
        report.written.contains(&hooks_path),
        "the merged file counts as written: {report:?}"
    );
    let after = std::fs::read_to_string(&hooks_path).unwrap();
    assert!(after.contains("echo keep"), "{after}");
    assert!(after.contains("echo new"), "{after}");
    let (loaded, _) = radian_host::load_project_hooks_file(&hooks_path).unwrap();
    assert_eq!(loaded.pre_tool_use.len(), 2);

    // A second run adds nothing: the file is byte-identical, with one report line.
    let before = std::fs::read(&hooks_path).unwrap();
    let report = write_claude_draft(&draft, &targets).unwrap();
    assert!(
        report
            .skipped
            .iter()
            .any(|line| line.contains("already present")),
        "{report:?}"
    );
    assert_eq!(std::fs::read(&hooks_path).unwrap(), before);
    assert_eq!(
        std::fs::read_to_string(&hooks_path)
            .unwrap()
            .matches("echo new")
            .count(),
        1
    );

    // An invalid file is left untouched.
    std::fs::write(&hooks_path, "{not json").unwrap();
    let report = write_claude_draft(&draft, &targets).unwrap();
    assert!(
        report
            .skipped
            .iter()
            .any(|line| line.contains("not valid JSON")),
        "{report:?}"
    );
    assert_eq!(std::fs::read_to_string(&hooks_path).unwrap(), "{not json");
}

/// Edge case 8: a Claude hook's `args` array is carried over unchanged, in the user
/// `[hooks]` draft and in the project `.radian/hooks.json` draft, and the written files
/// keep it (ADR 0045 §4). Dropping the array (the old behavior) fails every assertion.
#[test]
fn a_hook_args_array_is_carried_over_unchanged() {
    let home = Temp::new("args-home");
    let proj = Temp::new("args-proj");
    let args = r#"["a b", "$(touch /tmp/x)", ";", "*"]"#;
    write(
        home.path(),
        "settings.json",
        &format!(
            r#"{{"hooks": {{"PreToolUse": [{{"matcher": "Bash", "hooks": [{{"type": "command", "command": "/hooks/check", "args": {args}}}]}}]}}}}"#
        ),
    );
    write(
        proj.path(),
        ".claude/settings.json",
        &format!(
            r#"{{"hooks": {{"Stop": [{{"hooks": [{{"type": "command", "command": "/hooks/stop", "args": {args}}}]}}]}}}}"#
        ),
    );

    let draft = import_claude(Some(home.path()), Some(proj.path())).unwrap();
    let expected = serde_json::json!(["a b", "$(touch /tmp/x)", ";", "*"]);
    let config = radian_host::parse_config(Path::new("/tmp/draft.toml"), &draft.config_toml)
        .expect("the draft is a valid Radian configuration");
    assert_eq!(
        config.hooks.pre_tool_use[0].hooks[0].args,
        Some(vec![
            "a b".to_owned(),
            "$(touch /tmp/x)".to_owned(),
            ";".to_owned(),
            "*".to_owned(),
        ]),
        "the user draft keeps the array"
    );
    let project_hooks: serde_json::Value =
        serde_json::from_str(draft.project_hooks_json.as_deref().expect("project hooks")).unwrap();
    assert_eq!(
        project_hooks["Stop"][0]["hooks"][0]["args"], expected,
        "the project draft keeps the array"
    );

    // The written files keep it too.
    let scratch = Temp::new("args-write");
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: Some(scratch.path().join("project-skills")),
        project_hooks_path: Some(scratch.path().join("project-hooks.json")),
    };
    write_claude_draft(&draft, &targets).unwrap();
    let written = std::fs::read_to_string(&targets.config_path).unwrap();
    let written_config = radian_host::parse_config(&targets.config_path, &written).unwrap();
    assert_eq!(
        written_config.hooks.pre_tool_use[0].hooks[0].args,
        Some(vec![
            "a b".to_owned(),
            "$(touch /tmp/x)".to_owned(),
            ";".to_owned(),
            "*".to_owned(),
        ]),
        "the written config keeps the array: {written}"
    );
    let written_hooks = std::fs::read_to_string(scratch.path().join("project-hooks.json")).unwrap();
    let written_json: serde_json::Value = serde_json::from_str(&written_hooks).unwrap();
    assert_eq!(
        written_json["Stop"][0]["hooks"][0]["args"], expected,
        "the written hooks file keeps the array: {written_hooks}"
    );
    readable(home.path());
}

/// ADR 0045 §5 / edge case 5's import half: `async` and `asyncRewake` are carried into the
/// user draft and the project hooks draft on `PostToolUse`/`Stop`, while an async
/// `PreToolUse` hook is still dropped with one report line. Dropping every async entry
/// (the old behavior) fails these assertions.
#[test]
fn async_hooks_are_imported_on_post_and_stop_but_not_on_pre() {
    let home = Temp::new("async-home");
    let proj = Temp::new("async-proj");
    write(
        home.path(),
        "settings.json",
        r#"{"hooks": {
            "PostToolUse": [{"matcher": "Edit", "hooks": [
                {"type": "command", "command": "/hooks/lint", "async": true},
                {"type": "command", "command": "/hooks/rewake", "asyncRewake": true}
            ]}],
            "PreToolUse": [{"matcher": "Bash", "hooks": [
                {"type": "command", "command": "/hooks/too-early", "async": true},
                {"type": "command", "command": "/hooks/sync"}
            ]}]
        }}"#,
    );
    write(
        proj.path(),
        ".claude/settings.json",
        r#"{"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "/hooks/verify", "async": true, "asyncRewake": true}
        ]}]}}"#,
    );

    let draft = import_claude(Some(home.path()), Some(proj.path())).unwrap();
    let report = draft.notes.join("\n");
    assert!(
        report.contains("`async`/`asyncRewake` is only supported on `PostToolUse` and `Stop`"),
        "the PreToolUse async hook is dropped with a report line: {report}"
    );
    let config = radian_host::parse_config(Path::new("/tmp/draft.toml"), &draft.config_toml)
        .expect("the draft is a valid Radian configuration");
    assert_eq!(config.hooks.post_tool_use[0].hooks.len(), 2);
    assert!(config.hooks.post_tool_use[0].hooks[0].async_hook);
    assert!(!config.hooks.post_tool_use[0].hooks[0].async_rewake);
    assert!(config.hooks.post_tool_use[0].hooks[1].async_rewake);
    assert!(
        config.hooks.post_tool_use[0].hooks[1].is_async(),
        "asyncRewake implies async"
    );
    assert_eq!(
        config.hooks.pre_tool_use[0].hooks.len(),
        1,
        "only the sync PreToolUse hook imports"
    );
    assert_eq!(config.hooks.pre_tool_use[0].hooks[0].command, "/hooks/sync");
    assert!(
        config.hooks.pre_tool_use[0]
            .hooks
            .iter()
            .all(|hook| !hook.is_async()),
        "no async hook on PreToolUse"
    );

    let project_hooks: serde_json::Value =
        serde_json::from_str(draft.project_hooks_json.as_deref().expect("project hooks")).unwrap();
    assert_eq!(project_hooks["Stop"][0]["hooks"][0]["async"], true);
    assert_eq!(project_hooks["Stop"][0]["hooks"][0]["asyncRewake"], true);

    // The written files carry the flags too.
    let scratch = Temp::new("async-write");
    let targets = ClaudeTargets {
        config_path: scratch.path().join("config.toml"),
        user_skills_dir: scratch.path().join("user-skills"),
        user_agents_path: scratch.path().join("AGENTS.md"),
        project_skills_dir: None,
        project_hooks_path: Some(scratch.path().join("project-hooks.json")),
    };
    write_claude_draft(&draft, &targets).unwrap();
    let written = std::fs::read_to_string(&targets.config_path).unwrap();
    let written_config = radian_host::parse_config(&targets.config_path, &written).unwrap();
    assert!(written_config.hooks.post_tool_use[0].hooks[0].async_hook);
    assert!(written_config.hooks.post_tool_use[0].hooks[1].async_rewake);
    let written_hooks = std::fs::read_to_string(scratch.path().join("project-hooks.json")).unwrap();
    let written_json: serde_json::Value = serde_json::from_str(&written_hooks).unwrap();
    let loaded =
        radian_host::project_hooks::parse_project_value(&written_json, "written", &mut Vec::new());
    assert!(loaded.stop[0].hooks[0].async_rewake);
}

/// ADR 0045 §5: the async fields belong to `PostToolUse` and `Stop`. A pre-call hook that
/// writes either one explicitly as `false` is dropped and reported like an async one —
/// presence, not value — so the import never emits a hook the config parser rejects.
#[test]
fn an_explicitly_false_async_field_on_pre_is_not_imported() {
    let home = Temp::new("async-false-home");
    write(
        home.path(),
        "settings.json",
        r#"{"hooks": {
            "PreToolUse": [{"matcher": "Bash", "hooks": [
                {"type": "command", "command": "/hooks/async-off", "async": false},
                {"type": "command", "command": "/hooks/rewake-off", "asyncRewake": false},
                {"type": "command", "command": "/hooks/sync"}
            ]}],
            "PermissionRequest": [{"hooks": [
                {"type": "command", "command": "/hooks/ask", "async": false}
            ]}]
        }}"#,
    );

    let draft = import_claude(Some(home.path()), None).unwrap();
    let report = draft.notes.join("\n");
    assert_eq!(
        report
            .matches("`async`/`asyncRewake` is only supported on `PostToolUse` and `Stop`")
            .count(),
        3,
        "one report line per dropped hook: {report}"
    );
    let config = radian_host::parse_config(Path::new("/tmp/draft.toml"), &draft.config_toml)
        .expect("the draft is a valid Radian configuration");
    assert_eq!(
        config.hooks.pre_tool_use[0].hooks.len(),
        1,
        "the explicitly false fields still drop their hooks"
    );
    assert_eq!(config.hooks.pre_tool_use[0].hooks[0].command, "/hooks/sync");
    assert!(
        config.hooks.permission_request.is_empty(),
        "PermissionRequest drops it too"
    );
}

/// ADR 0045 §5: importing a Claude async hook with `timeout: 0` must keep zero (or omit it),
/// not freeze the synchronous 60-second default into the draft. Zero means "the default" in
/// Radian too, and an async hook's default is 600 s, so `HookCommand::timeout()` must still
/// select the async default after the import.
#[test]
fn an_imported_zero_timeout_keeps_the_async_default() {
    let home = Temp::new("async-zero-home");
    write(
        home.path(),
        "settings.json",
        r#"{"hooks": {"PostToolUse": [{"matcher": "Edit", "hooks": [
            {"type": "command", "command": "/hooks/lint", "async": true, "timeout": 0},
            {"type": "command", "command": "/hooks/format", "timeout": 0}
        ]}]}}"#,
    );

    let draft = import_claude(Some(home.path()), None).unwrap();
    assert!(
        draft.config_toml.contains("timeout = 0"),
        "zero stays zero in the draft: {}",
        draft.config_toml
    );
    let config = radian_host::parse_config(Path::new("/tmp/draft.toml"), &draft.config_toml)
        .expect("the draft is a valid Radian configuration");
    assert_eq!(config.hooks.post_tool_use[0].hooks[0].timeout_secs, Some(0));
    assert_eq!(
        config.hooks.post_tool_use[0].hooks[0].timeout().as_secs(),
        radian_host::config::DEFAULT_ASYNC_HOOK_TIMEOUT_SECS,
        "an imported async hook with `timeout: 0` uses the 600 s async default"
    );
    assert_eq!(
        config.hooks.post_tool_use[0].hooks[1].timeout().as_secs(),
        radian_host::config::DEFAULT_HOOK_TIMEOUT_SECS,
        "a synchronous hook with `timeout: 0` still defaults to 60 s"
    );

    // The project draft renders the same zero, and the project reader picks the async default.
    let proj = Temp::new("async-zero-proj");
    write(
        proj.path(),
        ".claude/settings.json",
        r#"{"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "/hooks/verify", "async": true, "timeout": 0}
        ]}]}}"#,
    );
    let project_draft = import_claude(None, Some(proj.path())).unwrap();
    let project_hooks: serde_json::Value = serde_json::from_str(
        project_draft
            .project_hooks_json
            .as_deref()
            .expect("project hooks"),
    )
    .unwrap();
    assert_eq!(project_hooks["Stop"][0]["hooks"][0]["timeout"], 0);
    let mut warnings = Vec::new();
    let loaded =
        radian_host::project_hooks::parse_project_value(&project_hooks, "test", &mut warnings);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        loaded.stop[0].hooks[0].timeout().as_secs(),
        radian_host::config::DEFAULT_ASYNC_HOOK_TIMEOUT_SECS,
        "the project reader also keeps the 600 s async default"
    );
}

/// ADR 0046: any `args` that is not an array of strings drops that hook with one report
/// line. `null` counts — in user and in project settings, which share `hook_command` — so
/// a `null` hook never imports as shell form; its good sibling still imports.
#[test]
fn a_hook_args_that_is_null_is_dropped_with_one_report_line() {
    let home = Temp::new("args-null-home");
    let proj = Temp::new("args-null-proj");
    write(
        home.path(),
        "settings.json",
        r#"{"hooks": {"PreToolUse": [{"matcher": "Bash", "hooks": [
            {"type": "command", "command": "/hooks/good"},
            {"type": "command", "command": "/hooks/null", "args": null}
        ]}]}}"#,
    );
    write(
        proj.path(),
        ".claude/settings.json",
        r#"{"hooks": {"Stop": [{"hooks": [
            {"type": "command", "command": "/hooks/stop-good"},
            {"type": "command", "command": "/hooks/stop-null", "args": null}
        ]}]}}"#,
    );

    let draft = import_claude(Some(home.path()), Some(proj.path())).unwrap();
    let config = radian_host::parse_config(Path::new("/tmp/draft.toml"), &draft.config_toml)
        .expect("the draft is a valid Radian configuration");
    let commands: Vec<&str> = config.hooks.pre_tool_use[0]
        .hooks
        .iter()
        .map(|hook| hook.command.as_str())
        .collect();
    assert_eq!(
        commands,
        vec!["/hooks/good"],
        "the null hook never imports: {commands:?}"
    );
    assert_eq!(
        config.hooks.pre_tool_use[0].hooks[0].args, None,
        "the imported sibling stays shell form"
    );

    let project: serde_json::Value =
        serde_json::from_str(draft.project_hooks_json.as_deref().expect("project hooks")).unwrap();
    let project_hooks = project["Stop"][0]["hooks"].as_array().unwrap();
    assert_eq!(project_hooks.len(), 1, "{project}");
    assert_eq!(project_hooks[0]["command"], "/hooks/stop-good");

    let drops: Vec<&String> = draft
        .notes
        .iter()
        .filter(|line| line.contains("a hook was not imported"))
        .collect();
    assert_eq!(drops.len(), 2, "one line per null hook: {:?}", draft.notes);
    assert!(
        drops.iter().all(|line| line.contains("args")),
        "each line names the bad `args`: {drops:?}"
    );
    assert!(
        drops.iter().any(|line| line.contains("`settings.json`"))
            && drops
                .iter()
                .any(|line| line.contains("`.claude/settings.json`")),
        "one for the user file, one for the project file: {drops:?}"
    );
    readable(home.path());
}

/// ADR 0045 §4 / ADR 0046: a Claude `PermissionRequest` entry is imported like the other
/// events, with its matcher translated once (`Bash`→`shell`, `Edit`→`patch`). The project
/// file it writes loads back through the runtime reader.
#[test]
fn a_claude_permission_request_hook_is_imported() {
    let home = Temp::new("permission-request-home");
    write(
        home.path(),
        "settings.json",
        r#"{
  "hooks": {
    "PermissionRequest": [
      { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo answer" }] }
    ]
  }
}"#,
    );
    let draft = import_claude(Some(home.path()), None).unwrap();
    let config =
        radian_host::parse_config(Path::new("/tmp/draft.toml"), &draft.config_toml).unwrap();
    assert_eq!(config.hooks.permission_request.len(), 1);
    assert_eq!(
        config.hooks.permission_request[0].matcher.as_deref(),
        Some("shell")
    );
    assert_eq!(
        config.hooks.permission_request[0].hooks[0].command,
        "echo answer"
    );

    let proj = Temp::new("permission-request-proj");
    write(
        proj.path(),
        ".claude/settings.json",
        r#"{
  "hooks": {
    "PermissionRequest": [
      { "matcher": "Edit", "hooks": [{ "type": "command", "command": "echo ask" }] }
    ]
  }
}"#,
    );
    let draft = import_claude(None, Some(proj.path())).unwrap();
    let json = draft
        .project_hooks_json
        .as_deref()
        .expect("the project hooks file is rendered");
    let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
    let mut warnings = Vec::new();
    let loaded = radian_host::project_hooks::parse_project_value(&parsed, "test", &mut warnings);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(loaded.permission_request.len(), 1);
    assert_eq!(
        loaded.permission_request[0].matcher.as_deref(),
        Some("patch")
    );
}
