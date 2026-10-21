//! The golden behaviour pin for the config reader (task: move the hand-written TOML walk onto
//! serde). Every fixture in `docs/schema/config/` and a set of hand-made edge cases (an unknown
//! key in each section, a wrong type in each field, a refused key, a removed key, a
//! `schema_version` mismatch) is parsed and the **full result** is recorded: the `Debug` of the
//! accepted `Config` (which carries the whole value tree and the sorted `skipped` list), or the
//! `Display` of the refusal.
//!
//! The refactor must not change one byte of this file. Run with `UPDATE_GOLDEN=1` only to
//! regenerate it deliberately (and then read the diff, never commit a silent change).

use std::path::{Path, PathBuf};

use radian_host::config;

/// The fixed path the golden parses with, so `Config::source` is stable in the snapshot.
const GOLDEN_PATH: &str = "/golden/config.toml";

fn render(name: &str, text: &str) -> String {
    let mut out = String::new();
    out.push_str("### ");
    out.push_str(name);
    out.push('\n');
    match config::parse(Path::new(GOLDEN_PATH), text) {
        Ok(config) => out.push_str(&format!("{config:?}\n")),
        Err(error) => {
            out.push_str("Err(");
            out.push_str(&error.to_string());
            out.push_str(")\n");
        }
    }
    out
}

/// Every fixture under `docs/schema/config/`, sorted by name, so the golden covers the whole
/// `EXPECT` table the reference checker and `config_fixtures.rs` use.
fn fixture_cases() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/schema/config");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".toml"))
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let path = dir.join(&name);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
            (format!("fixture:{name}"), text)
        })
        .collect()
}

/// The hand-made edge cases: one per rule the reader enforces, plus a wrong type for every
/// field whose type is checked.
fn edge_cases() -> Vec<(String, String)> {
    let cases: &[(&str, &str)] = &[
        // ---- schema_version (§5.1/§5.2/§5.3) ----
        ("schema_version.missing", "future_top = 1\n"),
        ("schema_version.string", "schema_version = \"1\"\n"),
        ("schema_version.lower", "schema_version = 0\n"),
        (
            "schema_version.higher",
            "schema_version = 99\n[model]\nmodel = \"m-99\"\n",
        ),
        // ---- unknown keys ----
        ("unknown.top_level", "schema_version = 1\nfuture_top = 1\n"),
        ("unknown.mcp", "schema_version = 1\n[mcp]\nfuture = 1\n"),
        (
            "unknown.model",
            "schema_version = 1\n[model]\nfuture_option = \"alpha\"\n",
        ),
        (
            "unknown.permissions",
            "schema_version = 1\n[permissions]\nfuture = 1\n",
        ),
        (
            "unknown.integrations",
            "schema_version = 1\n[integrations]\nfuture = 1\n",
        ),
        (
            "unknown.control",
            "schema_version = 1\n[control]\nfuture = 1\n",
        ),
        (
            "unknown.sandbox",
            "schema_version = 1\n[sandbox]\nfuture = 1\n",
        ),
        (
            "unknown.compaction",
            "schema_version = 1\n[compaction]\nfuture = 1\n",
        ),
        (
            "unknown.skills",
            "schema_version = 1\n[skills]\nfuture = 1\n",
        ),
        (
            "unknown.memory",
            "schema_version = 1\n[memory]\nfuture = 1\n",
        ),
        (
            "unknown.memory_injection",
            "schema_version = 1\n[memory.injection]\nfuture = 1\n",
        ),
        ("unknown.git", "schema_version = 1\n[git]\nfuture = 1\n"),
        ("unknown.shell", "schema_version = 1\n[shell]\nfuture = 1\n"),
        ("unknown.tui", "schema_version = 1\n[tui]\nfuture = 1\n"),
        (
            "unknown.tui_display",
            "schema_version = 1\n[tui.display]\nfuture = 1\n",
        ),
        (
            "unknown.providers_field",
            "schema_version = 1\n[providers.a]\nkind = \"anthropic\"\nbase_url = \"https://x\"\nfuture = 1\n",
        ),
        // ---- model ----
        (
            "model.section_not_a_table",
            "schema_version = 1\nmodel = 3\n",
        ),
        (
            "model.provider_wrong_type",
            "schema_version = 1\n[model]\nprovider = 3\n",
        ),
        (
            "model.base_url_wrong_type",
            "schema_version = 1\n[model]\nbase_url = 3\n",
        ),
        (
            "model.model_wrong_type",
            "schema_version = 1\n[model]\nmodel = 3\n",
        ),
        (
            "model.headers_wrong_type",
            "schema_version = 1\n[model]\nheaders = 3\n",
        ),
        (
            "model.headers_value_wrong_type",
            "schema_version = 1\n[model.headers]\nx-a = 1\nx-b = \"2\"\n",
        ),
        (
            "model.context_window.zero",
            "schema_version = 1\n[model]\ncontext_window = 0\n",
        ),
        (
            "model.context_window.wrong_type",
            "schema_version = 1\n[model]\ncontext_window = \"x\"\n",
        ),
        (
            "model.retries.negative",
            "schema_version = 1\n[model]\nretries = -1\n",
        ),
        (
            "model.retries.wrong_type",
            "schema_version = 1\n[model]\nretries = \"3\"\n",
        ),
        (
            "model.include_usage",
            "schema_version = 1\n[model]\ninclude_usage = true\n",
        ),
        // `model.api_key` (§4.2 rule 1, then ADR 0026 D2 for the table form).
        (
            "model.api_key.plaintext",
            "schema_version = 1\n[model]\napi_key = \"super-secret\"\n",
        ),
        (
            "model.api_key.secret_ref",
            "schema_version = 1\n[model]\napi_key = { from = \"env\", name = \"K\" }\n",
        ),
        // ---- permissions ----
        (
            "permissions.section_not_a_table",
            "schema_version = 1\npermissions = 3\n",
        ),
        (
            "permissions.profile.tier",
            "schema_version = 1\n[permissions]\nprofile = \"workspace\"\n",
        ),
        (
            "permissions.profile.adr_name",
            "schema_version = 1\n[permissions]\nprofile = \"ask\"\n",
        ),
        (
            "permissions.profile.unknown",
            "schema_version = 1\n[permissions]\nprofile = \"future\"\n",
        ),
        // The removed tier is unknown too: it takes the same refusal branch as any typo.
        (
            "permissions.profile.removed_safe",
            "schema_version = 1\n[permissions]\nprofile = \"safe\"\n",
        ),
        (
            "permissions.profile.unknown_read_only",
            "schema_version = 99\n[permissions]\nprofile = \"future\"\n",
        ),
        (
            "permissions.approval.known",
            "schema_version = 1\n[permissions]\napproval = \"auto\"\n",
        ),
        (
            "permissions.approval.unknown",
            "schema_version = 1\n[permissions]\napproval = \"yolo\"\n",
        ),
        (
            "permissions.approval.unknown_read_only",
            "schema_version = 99\n[permissions]\napproval = \"future\"\n",
        ),
        (
            "permissions.approval.wrong_type",
            "schema_version = 1\n[permissions]\napproval = true\n",
        ),
        // ---- sandbox ----
        (
            "sandbox.section_not_a_table",
            "schema_version = 1\nsandbox = 3\n",
        ),
        (
            "sandbox.allow_unix_sockets.ok",
            "schema_version = 1\n[sandbox]\nallow_unix_sockets = [\"~/.config/herdr/herdr.sock\"]\n",
        ),
        (
            "sandbox.allow_unix_sockets.not_array",
            "schema_version = 1\n[sandbox]\nallow_unix_sockets = \"x\"\n",
        ),
        (
            "sandbox.allow_unix_sockets.bad_element",
            "schema_version = 1\n[sandbox]\nallow_unix_sockets = [\"a\", 3]\n",
        ),
        (
            "sandbox.allow_unix_sockets.empty_element",
            "schema_version = 1\n[sandbox]\nallow_unix_sockets = [\"  \"]\n",
        ),
        // ---- integrations ----
        (
            "integrations.section_not_a_table",
            "schema_version = 1\nintegrations = 3\n",
        ),
        (
            "integrations.herdr.false",
            "schema_version = 1\n[integrations]\nherdr = false\n",
        ),
        (
            "integrations.herdr.wrong_type",
            "schema_version = 1\n[integrations]\nherdr = \"no\"\n",
        ),
        // ---- control ----
        (
            "control.section_not_a_table",
            "schema_version = 1\ncontrol = 3\n",
        ),
        (
            "control.enabled.false",
            "schema_version = 1\n[control]\nenabled = false\n",
        ),
        (
            "control.enabled.wrong_type",
            "schema_version = 1\n[control]\nenabled = \"no\"\n",
        ),
        // ---- compaction ----
        (
            "compaction.section_not_a_table",
            "schema_version = 1\ncompaction = 3\n",
        ),
        (
            "compaction.auto.wrong_type",
            "schema_version = 1\n[compaction]\nauto = \"no\"\n",
        ),
        (
            "compaction.thinking.current",
            "schema_version = 1\n[compaction]\nthinking = \"current\"\n",
        ),
        (
            "compaction.thinking.max",
            "schema_version = 1\n[compaction]\nthinking = \"max\"\n",
        ),
        (
            "compaction.thinking.unknown",
            "schema_version = 1\n[compaction]\nthinking = \"loud\"\n",
        ),
        (
            "compaction.thinking.wrong_type",
            "schema_version = 1\n[compaction]\nthinking = 3\n",
        ),
        // ---- skills ----
        (
            "skills.section_not_a_table",
            "schema_version = 1\nskills = 3\n",
        ),
        (
            "skills.extra_paths.ok",
            "schema_version = 1\n[skills]\nextra_paths = [\"a\", \"b\"]\n",
        ),
        (
            "skills.extra_paths.not_array",
            "schema_version = 1\n[skills]\nextra_paths = \"a\"\n",
        ),
        (
            "skills.extra_paths.bad_element",
            "schema_version = 1\n[skills]\nextra_paths = [\"a\", 3]\n",
        ),
        (
            "skills.extra_paths.empty_element",
            "schema_version = 1\n[skills]\nextra_paths = [\"\"]\n",
        ),
        (
            "skills.builtin.false",
            "schema_version = 1\n[skills]\nbuiltin = false\n",
        ),
        (
            "skills.builtin.wrong_type",
            "schema_version = 1\n[skills]\nbuiltin = 3\n",
        ),
        (
            "skills.disabled.ok",
            "schema_version = 1\n[skills]\ndisabled = [\"diagrams\"]\n",
        ),
        (
            "skills.disabled.bad_element",
            "schema_version = 1\n[skills]\ndisabled = [\"a\", \" \"]\n",
        ),
        (
            "skills.disabled.not_array",
            "schema_version = 1\n[skills]\ndisabled = \"a\"\n",
        ),
        // ---- memory ----
        (
            "memory.section_not_a_table",
            "schema_version = 1\nmemory = 3\n",
        ),
        (
            "memory.injection.not_a_table",
            "schema_version = 1\n[memory]\ninjection = 3\n",
        ),
        (
            "memory.injection.additions.false",
            "schema_version = 1\n[memory.injection]\nadditions = false\n",
        ),
        (
            "memory.injection.additions.wrong_type",
            "schema_version = 1\n[memory.injection]\nadditions = 3\n",
        ),
        (
            "memory.injection.addition_budget.ok",
            "schema_version = 1\n[memory.injection]\naddition_budget = 750\n",
        ),
        (
            "memory.injection.addition_budget.zero",
            "schema_version = 1\n[memory.injection]\naddition_budget = 0\n",
        ),
        (
            "memory.injection.addition_budget.wrong_type",
            "schema_version = 1\n[memory.injection]\naddition_budget = \"x\"\n",
        ),
        // ---- hooks (ADR 0045 slice A) ----
        (
            "hooks.section_not_a_table",
            "schema_version = 1\nhooks = 3\n",
        ),
        (
            "hooks.event_not_a_list",
            "schema_version = 1\n[hooks]\nPreToolUse = 3\n",
        ),
        (
            "hooks.unknown_event",
            "schema_version = 1\n[[hooks.SessionStart]]\nmatcher = \"read\"\n[[hooks.SessionStart.hooks]]\ntype = \"command\"\ncommand = \"echo hi\"\n",
        ),
        (
            "hooks.ok",
            "schema_version = 1\n[[hooks.PreToolUse]]\nmatcher = \"shell\"\n[[hooks.PreToolUse.hooks]]\ntype = \"command\"\ncommand = \"echo hi\"\ntimeout = 5\n",
        ),
        (
            "hooks.non_command_type",
            "schema_version = 1\n[[hooks.PreToolUse]]\nmatcher = \"read\"\n[[hooks.PreToolUse.hooks]]\ntype = \"http\"\ncommand = \"https://example.com\"\n",
        ),
        (
            "hooks.async_handler",
            "schema_version = 1\n[[hooks.PostToolUse]]\nmatcher = \"read\"\n[[hooks.PostToolUse.hooks]]\ntype = \"command\"\ncommand = \"echo hi\"\nasync = true\n",
        ),
        (
            "hooks.bad_timeout",
            "schema_version = 1\n[[hooks.Stop]]\n[[hooks.Stop.hooks]]\ntype = \"command\"\ncommand = \"echo hi\"\ntimeout = \"x\"\n",
        ),
        // ---- shell: the section has no live keys; the removed `auto_background_secs`
        // and any other key warn once and are ignored, a non-table is ignored ----
        (
            "shell.removed_key",
            "schema_version = 1\n[shell]\nauto_background_secs = 30\n",
        ),
        (
            "shell.removed_key_zero",
            "schema_version = 1\n[shell]\nauto_background_secs = 0\n",
        ),
        (
            "shell.removed_key_negative",
            "schema_version = 1\n[shell]\nauto_background_secs = -1\n",
        ),
        (
            "shell.removed_key_wrong_type",
            "schema_version = 1\n[shell]\nauto_background_secs = \"x\"\n",
        ),
        (
            "shell.unknown_key",
            "schema_version = 1\n[shell]\nfuture_key = 1\n",
        ),
        (
            "shell.section_not_a_table",
            "schema_version = 1\nshell = 3\n",
        ),
        // ---- model.smol (ADR 0008 §1, ADR 0043 §1): carried when both keys are set,
        // warned and ignored when half-set; unknown keys and wrong types are skipped ----
        (
            "model.smol.ok",
            "schema_version = 1\n[model.smol]\nprovider = \"cheap\"\nmodel = \"m-c\"\n",
        ),
        (
            "model.smol.half",
            "schema_version = 1\n[model.smol]\nprovider = \"cheap\"\n",
        ),
        (
            "model.smol.wrong_type",
            "schema_version = 1\n[model.smol]\nprovider = 3\nmodel = \"m-c\"\n",
        ),
        (
            "model.smol.not_a_table",
            "schema_version = 1\n[model]\nsmol = 3\n",
        ),
        // `[subagent.explore]` was never released: now an unknown top-level section.
        (
            "unknown.subagent_explore",
            "schema_version = 1\n[subagent.explore]\nprovider = \"cheap\"\nmodel = \"m-c\"\n",
        ),
        // ---- tui ----
        ("tui.section_not_a_table", "schema_version = 1\ntui = 3\n"),
        (
            "tui.theme",
            "schema_version = 1\n[tui]\ntheme = \"kanagawa-wave\"\n",
        ),
        (
            "tui.theme.wrong_type",
            "schema_version = 1\n[tui]\ntheme = 3\n",
        ),
        (
            "tui.theme.nested_table",
            "schema_version = 1\n[tui.theme.hairline]\nframes = false\n",
        ),
        (
            "tui.colors.not_a_table",
            "schema_version = 1\n[tui]\ncolors = 3\n",
        ),
        (
            "tui.colors.entries",
            "schema_version = 1\n[tui.colors]\nuser = \"#0af\"\ntool_call = 39\nbad = { fg = \"#fff\", neon = true }\nmuted = 2.5\n",
        ),
        (
            "tui.keybindings.not_a_table",
            "schema_version = 1\n[tui.keybindings]\n",
        ),
        (
            "tui.keybindings.values",
            "schema_version = 1\n[tui.keybindings]\nsend = [\"ctrl-s\", 3]\nnewline = \"x\"\n",
        ),
        (
            "tui.mouse.wrong_type",
            "schema_version = 1\n[tui]\nmouse = \"yes\"\n",
        ),
        (
            "tui.input_style.unknown",
            "schema_version = 1\n[tui]\ninput_style = \"future\"\n",
        ),
        (
            "tui.footer.not_a_table",
            "schema_version = 1\n[tui]\nfooter = 3\n",
        ),
        (
            "tui.footer.unknown_key",
            "schema_version = 1\n[tui.footer]\nfuture_key = true\n[[tui.footer.rows]]\nleft = [\"model\"]\n",
        ),
        (
            "tui.footer.bad_row_item",
            "schema_version = 1\n[tui.footer]\n[[tui.footer.rows]]\nleft = [{ item = \"model\", format = 3 }]\n",
        ),
        (
            "tui.footer.thresholds",
            "schema_version = 1\n[tui.footer]\n[[tui.footer.rows]]\nleft = [{ item = \"context\", thresholds = [{ at = 70, style = \"warn\" }, { at = 90 }] }]\n",
        ),
        (
            "tui.widgets.not_a_table",
            "schema_version = 1\n[tui]\nwidgets = 3\n",
        ),
        (
            "tui.widgets.unknown_key",
            "schema_version = 1\n[tui.widgets]\nfuture = [\"x\"]\n",
        ),
        (
            "tui.widgets.bad_entry",
            "schema_version = 1\n[tui.widgets]\nabove_input = [\"status\", 3]\n",
        ),
        (
            "tui.usage_period_secs",
            "schema_version = 1\n[tui]\nusage_period_secs = 120\n",
        ),
        (
            "tui.usage_period_secs.wrong_type",
            "schema_version = 1\n[tui]\nusage_period_secs = \"x\"\n",
        ),
        (
            "tui.display.ok",
            "schema_version = 1\n[tui.display]\nreasoning = \"hidden\"\ntool_output = \"errors\"\ntool_preview_lines = 8\nmermaid = false\nhighlight = false\n",
        ),
        (
            "tui.display.unknown",
            "schema_version = 1\n[tui.display]\nreasoning = \"sometimes\"\ntool_preview_lines = 0\nmermaid = \"yes\"\nhighlight = 3\n",
        ),
        (
            "tui.display.not_a_table",
            "schema_version = 1\n[tui]\ndisplay = 3\n",
        ),
        (
            "tui.clipboard.unknown",
            "schema_version = 1\n[tui]\nclipboard = \"future\"\n",
        ),
        (
            "tui.tips.wrong_type",
            "schema_version = 1\n[tui]\ntips = \"yes\"\n",
        ),
        (
            "tui.banner.wrong_type",
            "schema_version = 1\n[tui]\nbanner = \"yes\"\n",
        ),
        ("tui.brand.ok", "schema_version = 1\n[tui]\nbrand = \"ʀ\"\n"),
        (
            "tui.brand.wrong_type",
            "schema_version = 1\n[tui]\nbrand = 3\n",
        ),
        (
            "tui.brand.too_wide",
            "schema_version = 1\n[tui]\nbrand = \"㎭㎭R\"\n",
        ),
        (
            "tui.spinner.ok",
            "schema_version = 1\n[tui]\nspinner = \"quadrant\"\n",
        ),
        (
            "tui.spinner.wrong_type",
            "schema_version = 1\n[tui]\nspinner = 3\n",
        ),
        // ---- providers ----
        (
            "providers.section_not_a_table",
            "schema_version = 1\nproviders = 3\n",
        ),
        (
            "providers.entry_not_a_table",
            "schema_version = 1\n[providers]\na = 3\n",
        ),
        (
            "providers.ok",
            "schema_version = 1\n[providers.corp]\nkind = \"anthropic\"\nbase_url = \"https://x\"\nreasoning_replay = \"reasoning_content\"\n[providers.relay]\nkind = \"openai-compatible\"\nbase_url = \"https://y\"\nreasoning_replay = false\n",
        ),
        (
            "providers.bad_name",
            "schema_version = 1\n[providers.\"has space\"]\nkind = \"anthropic\"\nbase_url = \"https://x\"\n",
        ),
        (
            "providers.shadows_builtin",
            "schema_version = 1\n[providers.anthropic]\nkind = \"anthropic\"\nbase_url = \"https://x\"\n",
        ),
        (
            "providers.unknown_kind",
            "schema_version = 1\n[providers.corp]\nkind = \"grpc\"\nbase_url = \"https://x\"\n",
        ),
        (
            "providers.empty_base_url",
            "schema_version = 1\n[providers.corp]\nkind = \"anthropic\"\nbase_url = \"\"\n",
        ),
        (
            "providers.base_url_wrong_type",
            "schema_version = 1\n[providers.corp]\nkind = \"anthropic\"\nbase_url = 3\n",
        ),
        (
            "providers.reasoning_replay.true",
            "schema_version = 1\n[providers.corp]\nkind = \"anthropic\"\nbase_url = \"https://x\"\nreasoning_replay = true\n",
        ),
        (
            "providers.reasoning_replay.empty",
            "schema_version = 1\n[providers.corp]\nkind = \"anthropic\"\nbase_url = \"https://x\"\nreasoning_replay = \"\"\n",
        ),
        // ---- mcp (ADR 0008 §8) ----
        (
            "mcp.servers_form",
            "schema_version = 1\n[[mcp.servers]]\nid = \"a\"\ncommand = \"true\"\n",
        ),
        (
            "mcp.mcp_servers_form",
            "schema_version = 1\n[mcp.mcpServers.a]\ncommand = \"true\"\n",
        ),
        (
            "mcp.both_forms",
            "schema_version = 1\n[[mcp.servers]]\nid = \"a\"\ncommand = \"true\"\n[mcp.mcpServers]\n",
        ),
        // `unknown.mcp` above already covers the neither case (`[mcp]` with no form).
        (
            "mcp.remote_skipped",
            "schema_version = 1\n[mcp.mcpServers.web]\ntype = \"http\"\nurl = \"https://example.com/mcp\"\n[mcp.mcpServers.files]\ncommand = \"/work/example/mcp-files\"\n",
        ),
        ("mcp.not_a_table", "schema_version = 1\nmcp = 3\n"),
        // ---- secrets (§4.2 / §4.1) ----
        (
            "secret.lookalike",
            "schema_version = 1\n[tui]\nstatus_template = \"sk-live-abcdef123456\"\n",
        ),
        (
            "secret.long_random",
            "schema_version = 1\n[tui]\nstatus_template = \"abcdefghijklmnopqrstuvwxyz0123456789ABCD\"\n",
        ),
        (
            "secret.ref_not_env",
            "schema_version = 1\n[model]\napi_key = { from = \"keychain\", name = \"x\" }\n",
        ),
        (
            "secret.ref_no_name",
            "schema_version = 1\n[model]\napi_key = { from = \"env\" }\n",
        ),
        (
            "secret.ref_unknown_field",
            "schema_version = 1\n[model]\napi_key = { from = \"env\", name = \"x\", extra = 1 }\n",
        ),
    ];
    cases
        .iter()
        .map(|(name, text)| ((*name).to_owned(), (*text).to_owned()))
        .collect()
}

fn snapshot() -> String {
    let mut out = String::new();
    for (name, text) in fixture_cases() {
        out.push_str(&render(&name, &text));
    }
    for (name, text) in edge_cases() {
        out.push_str(&render(&name, &text));
    }
    out
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/config_parse.golden")
}

#[test]
fn the_reader_matches_its_golden_behaviour() {
    let snapshot = snapshot();
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(golden_path(), &snapshot).unwrap();
        return;
    }
    let golden = std::fs::read_to_string(golden_path())
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", golden_path().display()));
    assert_eq!(
        snapshot, golden,
        "the config reader's behaviour changed; if that was deliberate, run with UPDATE_GOLDEN=1 and review the diff"
    );
}
