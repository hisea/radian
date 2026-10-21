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
/// plugins, LSP, rules, local files, memory/sessions, and the two not-imported notices.
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
        "reads MCP servers from `--mcp-config` only",
        "does not load project hooks",
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

/// A plaintext secret is never copied: the draft and the report carry only an ADR 0008 §3
/// environment reference with a suggested name. A mutation that copies the literal fails
/// this test.
#[test]
fn a_literal_secret_is_never_shown_and_becomes_a_reference() {
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
        draft
            .notes
            .iter()
            .any(|note| note.contains("SECRET_SRV_API_KEY") && note.contains("from = \"env\"")),
        "the note must suggest an env reference: {:?}",
        draft.notes
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
            user_agents_md: None,
            user_skills: Vec::new(),
            project_skills: Vec::new(),
            notes: Vec::new(),
            files_read: Vec::new(),
        }
    }
}
