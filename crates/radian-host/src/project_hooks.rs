//! Project-level external command hooks (ADR 0045 slice B): `<workspace>/.radian/hooks.json`.
//!
//! The same Claude/Codex command-hook subset as the user-level `[hooks]` section, spelled in
//! JSON: event names (`PreToolUse`, `PostToolUse`, `Stop`, `PermissionRequest`) map to lists of
//! `{ matcher, hooks }`
//! entries, each hook a `{ type = "command", command, args, timeout, async, asyncRewake }`
//! object. `args` is optional: with it `command` is the program and the hook starts without a shell; without it
//! `command` runs through `/bin/sh -c` (ADR 0045 §4). `matcher` is Claude's
//! case-sensitive pattern over **Radian** tool names, exactly like slice A; `Stop` ignores it.
//! Anything outside the subset — an unknown event name, a malformed entry, a hook whose `type`
//! is not `"command"`, or an entry carrying one of the non-command handler keys (`if`,
//! `http`, `prompt`, `agent`) — is skipped with one warning line, never refusing the file.
//! `async`/`asyncRewake` are fields of their own, accepted on `PostToolUse` and `Stop` only;
//! on `PreToolUse` and `PermissionRequest` that hook is dropped with one warning line and the
//! entry's other hooks still load.
//!
//! The file only loads when the directory is trusted (ADR 0031 T1). Like the project
//! `.mcp.json`: an untrusted directory never reads it; `--trust` overrides for this process;
//! a change made during a session takes effect at the next start.

use std::path::{Path, PathBuf};

use crate::config::{
    HookCommand, HookMatcher, HooksConfig, NOTIFICATION, POST_TOOL_USE, STOP, STOP_FAILURE,
};

/// The file name inside `<workspace>/.radian/`.
pub const FILE_NAME: &str = "hooks.json";

/// `<workspace>/.radian/hooks.json`.
pub fn project_path(workspace: &Path) -> PathBuf {
    workspace.join(".radian").join(FILE_NAME)
}

/// Handler keys this slice does not implement (ADR 0045): an entry carrying any of them is
/// skipped, never run. The same table as the user-level `[hooks]` reader; `async` is no
/// longer one of them.
const NON_COMMAND_HANDLER_KEYS: [&str; 4] = ["if", "http", "prompt", "agent"];

/// The events the project file loads. The same table as the user-level reader: both
/// come from one constant so the two readers cannot drift apart.
const KNOWN_EVENTS: &[&str] = crate::config::HOOK_EVENTS;

/// Parses the project hooks file's JSON value into a [`HooksConfig`], collecting one warning
/// line per skipped entry. Never refuses: outside-the-subset content is skipped and named, so
/// one startup line says it once.
///
/// Both shapes are accepted: the bare events object (`{"PreToolUse": [...]}`) and the wrapped
/// Claude settings shape (`{"hooks": {"PreToolUse": [...]}}`). When both are present the
/// wrapped `hooks` object wins.
pub fn parse_project_value(
    parsed: &serde_json::Value,
    origin: &str,
    warnings: &mut Vec<String>,
) -> HooksConfig {
    let mut config = HooksConfig::default();
    let Some(top) = parsed.as_object() else {
        warnings.push(format!(
            "{origin}: expected an object at the top level; skipped"
        ));
        return config;
    };
    let events = match top.get("hooks") {
        Some(wrapped) if wrapped.is_object() => wrapped.as_object().expect("checked as an object"),
        _ => top,
    };
    for (event, entries) in events {
        // The wrapper key itself is not an event when the top level is the events object.
        if event == "hooks" && top.get("hooks").is_some_and(|value| value.is_object()) {
            continue;
        }
        if !KNOWN_EVENTS.contains(&event.as_str()) {
            warnings.push(format!("{origin}: skipped hooks.{event} (unknown event)"));
            continue;
        }
        let Some(items) = entries.as_array() else {
            warnings.push(format!("{origin}: skipped hooks.{event} (not a list)"));
            continue;
        };
        let accepts_async = matches!(
            event.as_str(),
            POST_TOOL_USE | STOP | STOP_FAILURE | NOTIFICATION
        );
        for (index, item) in items.iter().enumerate() {
            let parsed = parse_entry(item, accepts_async);
            // A bad `args`, or `async` on an event that does not accept it, drops that hook
            // only: the entry's other hooks still load (ADR 0045 §1/§4). Each dropped hook
            // is one line, named down to the hook.
            for hook_index in parsed.dropped_hooks() {
                warnings.push(format!(
                    "{origin}: skipped hooks.{event}[{index}].hooks[{hook_index}] (a bad `args` \
                     or `async` on this event)"
                ));
            }
            match parsed.entry {
                Some(entry) => match event.as_str() {
                    "PreToolUse" => config.pre_tool_use.push(entry),
                    "PermissionRequest" => config.permission_request.push(entry),
                    "PostToolUse" => config.post_tool_use.push(entry),
                    "Stop" => config.stop.push(entry),
                    "StopFailure" => config.stop_failure.push(entry),
                    "Notification" => config.notification.push(entry),
                    _ => unreachable!("the event list is checked above"),
                },
                None if !parsed.dropped_any() => warnings.push(format!(
                    "{origin}: skipped hooks.{event}[{index}] (outside the command subset)"
                )),
                // Every hook was dropped one by one; the per-hook lines said so.
                None => {}
            }
        }
    }
    config
}

/// One parsed `{ matcher, hooks }` entry, plus the hooks dropped one by one.
struct ParsedEntry {
    /// The entry, `None` when it is skipped as a whole (then `dropped` is empty) or when
    /// every hook was dropped individually (then `dropped` names them).
    entry: Option<HookMatcher>,
    /// Indexes of hooks dropped for a bad `args`, or for `async`/`asyncRewake` on an event
    /// that does not accept them. The rest of the entry still loads; each index is one
    /// warning line.
    dropped: Vec<usize>,
}

impl ParsedEntry {
    /// The whole entry is outside the subset. One line names the entry.
    fn skipped() -> Self {
        Self {
            entry: None,
            dropped: Vec::new(),
        }
    }

    /// The indexes of hooks dropped one by one.
    fn dropped_hooks(&self) -> &[usize] {
        &self.dropped
    }

    /// Whether any hook was dropped one by one (so the entry's own line would repeat it).
    fn dropped_any(&self) -> bool {
        !self.dropped.is_empty()
    }
}

/// Reads one `{ matcher, hooks }` entry. A structural defect skips the whole entry (one
/// line): a hook whose `type` is not `"command"`, a hook carrying a non-command handler key,
/// a missing or empty `command`, or a bad `timeout`. A bad `args`, or `async` on an event
/// that does not accept it, drops only that hook.
fn parse_entry(item: &serde_json::Value, accepts_async: bool) -> ParsedEntry {
    let Some(table) = item.as_object() else {
        return ParsedEntry::skipped();
    };
    let matcher = match table.get("matcher") {
        None => None,
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(matcher)) => Some(matcher.clone()),
        Some(_) => return ParsedEntry::skipped(),
    };
    // An empty-string matcher matches everything (like slice A); keep it as written.
    let Some(hooks) = table.get("hooks").and_then(serde_json::Value::as_array) else {
        return ParsedEntry::skipped();
    };
    if hooks.is_empty() {
        return ParsedEntry::skipped();
    }
    let mut commands = Vec::new();
    let mut dropped = Vec::new();
    for (index, hook) in hooks.iter().enumerate() {
        match parse_command(hook, accepts_async) {
            CommandParse::Ok(command) => commands.push(command),
            CommandParse::DropHook => dropped.push(index),
            CommandParse::Skip => return ParsedEntry::skipped(),
        }
    }
    ParsedEntry {
        entry: (!commands.is_empty()).then_some(HookMatcher {
            matcher,
            hooks: commands,
        }),
        dropped,
    }
}

/// One hook row's parse result.
enum CommandParse {
    Ok(HookCommand),
    /// `args` is present but is not an array of strings, or `async`/`asyncRewake` is set on
    /// an event that does not accept them: drop this hook only.
    DropHook,
    /// Outside the command subset: drop the whole entry.
    Skip,
}

/// Reads one `{ type, command, args, timeout, async, asyncRewake }` hook.
fn parse_command(hook: &serde_json::Value, accepts_async: bool) -> CommandParse {
    let Some(table) = hook.as_object() else {
        return CommandParse::Skip;
    };
    if table.get("type").and_then(serde_json::Value::as_str) != Some("command") {
        return CommandParse::Skip;
    }
    if NON_COMMAND_HANDLER_KEYS
        .iter()
        .any(|key| table.contains_key(*key))
    {
        return CommandParse::Skip;
    }
    let Some(command) = table.get("command").and_then(serde_json::Value::as_str) else {
        return CommandParse::Skip;
    };
    if command.is_empty() {
        return CommandParse::Skip;
    }
    let timeout_secs = match table.get("timeout") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(timeout)) => match timeout.as_u64() {
            Some(secs) => Some(secs),
            None => return CommandParse::Skip,
        },
        Some(_) => return CommandParse::Skip,
    };
    let args = match table.get("args") {
        // A present `args` decides how the command runs (exec form or `/bin/sh -c`), so
        // `null` is not "absent" here, unlike `matcher`/`timeout`: it is not an array of
        // strings and the hook is dropped (ADR 0045 §4). Reading it as absent would quietly
        // switch the hook to the shell form.
        None => None,
        Some(serde_json::Value::Array(items)) => {
            let mut args = Vec::with_capacity(items.len());
            for item in items {
                let Some(arg) = item.as_str() else {
                    return CommandParse::DropHook;
                };
                args.push(arg.to_owned());
            }
            Some(args)
        }
        Some(_) => return CommandParse::DropHook,
    };
    let async_rewake = match table.get("asyncRewake") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(on)) => *on,
        Some(_) => return CommandParse::DropHook,
    };
    let async_hook = match table.get("async") {
        None | Some(serde_json::Value::Null) => async_rewake,
        Some(serde_json::Value::Bool(on)) => *on || async_rewake,
        Some(_) => return CommandParse::DropHook,
    };
    // Presence, not value: a pre-call hook that writes `"async": false` still asked for a
    // field those events do not accept and is dropped like an async one (ADR 0045 §5).
    // JSON `null` is absence, like `matcher` and `timeout`.
    let declares_async = ["async", "asyncRewake"]
        .iter()
        .any(|key| table.get(*key).is_some_and(|value| !value.is_null()));
    if !accepts_async && declares_async {
        return CommandParse::DropHook;
    }
    CommandParse::Ok(HookCommand {
        command: command.to_owned(),
        args,
        timeout_secs,
        async_hook,
        async_rewake,
    })
}

/// Loads the project hooks file at `path`: an unreadable file or invalid JSON is an error (the
/// caller turns it into one warning line, so a project file never breaks startup).
pub fn load_project_file(path: &Path) -> Result<(HooksConfig, Vec<String>), String> {
    let origin = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|e| format!("{origin}: {e}"))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{origin}: not valid hooks JSON: {e}"))?;
    let mut warnings = Vec::new();
    let config = parse_project_value(&parsed, &origin, &mut warnings);
    Ok((config, warnings))
}

/// Counts every hook command in the config, for the loaded line.
fn hook_count(config: &HooksConfig) -> usize {
    config
        .pre_tool_use
        .iter()
        .chain(config.permission_request.iter())
        .chain(config.post_tool_use.iter())
        .chain(config.stop.iter())
        .chain(config.stop_failure.iter())
        .chain(config.notification.iter())
        .map(|entry| entry.hooks.len())
        .sum()
}

/// Loads `<workspace>/.radian/hooks.json` for a session (ADR 0031 T1: only when the directory
/// is trusted), and reports what the host prints as `[hooks] …`.
///
/// - No file: nothing loads, no lines.
/// - Untrusted: nothing loads; one line says so, in the shape of ADR 0031 T3's
///   non-interactive line.
/// - Trusted but unreadable: nothing loads; one warning line (a project file must not break
///   startup, like `<workspace>/.radian/config.toml`).
/// - Trusted and readable: the hooks plus one loaded line and the skipped-entry warnings.
pub fn load_project(workspace: &Path, trusted: bool) -> (HooksConfig, Vec<String>) {
    let path = project_path(workspace);
    if !path.is_file() {
        return (HooksConfig::default(), Vec::new());
    }
    let origin = path.display().to_string();
    if !trusted {
        return (
            HooksConfig::default(),
            vec![format!(
                "project .radian/hooks.json not loaded (untrusted): pass --trust to load {origin}, \
                 or run radian interactively"
            )],
        );
    }
    match load_project_file(&path) {
        Ok((config, warnings)) => {
            let mut lines = vec![format!(
                "project .radian/hooks.json: loaded {} hook{} from {origin}",
                hook_count(&config),
                if hook_count(&config) == 1 { "" } else { "s" },
            )];
            lines.extend(warnings);
            (config, lines)
        }
        Err(error) => (
            HooksConfig::default(),
            vec![format!(
                "project .radian/hooks.json cannot be read ({error}); skipped"
            )],
        ),
    }
}

/// One project hook entry as written, for the trust question (ADR 0031 T3): the event, the
/// matcher as written (`*` when missing/empty, like the inspector shows), and the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectHooksPreviewEntry {
    pub event: String,
    pub matcher: String,
    pub command: String,
}

/// The trust question's view of `<workspace>/.radian/hooks.json`. `None` when there is no
/// file; `error` when the file exists but cannot be read or parsed (the loader's message,
/// which names the problem, never a value).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectHooksPreview {
    pub entries: Vec<ProjectHooksPreviewEntry>,
    pub error: Option<String>,
}

/// Renders a [`ProjectHooksPreview`] as trust-question lines: one line per hook (the event,
/// the matcher as written, the command), and the read/parse error when the file cannot be
/// used. Commands are as written; they only run once trusted.
pub fn preview_lines(preview: &ProjectHooksPreview) -> Vec<String> {
    if let Some(error) = &preview.error {
        return vec![format!("project .radian/hooks.json: {error}")];
    }
    if preview.entries.is_empty() {
        return vec!["project .radian/hooks.json: no hooks to load".to_owned()];
    }
    let mut lines = vec![format!(
        "project .radian/hooks.json: {} hook{} to load once trusted:",
        preview.entries.len(),
        if preview.entries.len() == 1 { "" } else { "s" },
    )];
    for entry in &preview.entries {
        lines.push(format!(
            "hooks: {} {} `{}`",
            entry.event, entry.matcher, entry.command
        ));
    }
    lines
}

/// Reads the raw entries of `<workspace>/.radian/hooks.json` for the trust question. The
/// loader runs first purely as the structural check, so the preview lists exactly what a
/// trusted load would start (plus the skipped entries, which the loader drops with a warning).
pub fn preview_project(workspace: &Path) -> Option<ProjectHooksPreview> {
    let path = project_path(workspace);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            return Some(ProjectHooksPreview {
                entries: Vec::new(),
                error: Some(format!("{}: {error}", path.display())),
            });
        }
    };
    let origin = path.display().to_string();
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(parsed) => parsed,
        Err(error) => {
            return Some(ProjectHooksPreview {
                entries: Vec::new(),
                error: Some(format!("{origin}: not valid hooks JSON: {error}")),
            });
        }
    };
    let mut warnings = Vec::new();
    let config = parse_project_value(&parsed, &origin, &mut warnings);
    // The parse is the authority; a top-level non-object still previews as its warning.
    if !parsed.is_object() {
        return Some(ProjectHooksPreview {
            entries: Vec::new(),
            error: warnings.into_iter().next(),
        });
    }
    let mut entries = Vec::new();
    let mut push = |event: &str, matchers: &[HookMatcher]| {
        for matcher in matchers {
            let display = match matcher.matcher.as_deref() {
                None | Some("") => "*".to_owned(),
                Some(matcher) => matcher.to_owned(),
            };
            for hook in &matcher.hooks {
                entries.push(ProjectHooksPreviewEntry {
                    event: event.to_owned(),
                    matcher: display.clone(),
                    command: hook.command.clone(),
                });
            }
        }
    };
    push("PreToolUse", &config.pre_tool_use);
    push("PermissionRequest", &config.permission_request);
    push("PostToolUse", &config.post_tool_use);
    push("Stop", &config.stop);
    push("StopFailure", &config.stop_failure);
    push("Notification", &config.notification);
    Some(ProjectHooksPreview {
        entries,
        error: None,
    })
}

#[cfg(test)]
mod args_tests {
    use super::*;

    /// `args` is the exec-form list (ADR 0045 §4), carried verbatim.
    #[test]
    fn args_parses_as_the_exec_form_argument_list() {
        let parsed: serde_json::Value = serde_json::from_str(
            r#"{
                "PreToolUse": [{"matcher": "shell", "hooks": [
                    {"type": "command", "command": "/hooks/check", "args": ["a b", "$(touch /tmp/x)", ";", "*"]},
                    {"type": "command", "command": "/hooks/empty", "args": []}
                ]}]
            }"#,
        )
        .unwrap();
        let mut warnings = Vec::new();
        let config = parse_project_value(&parsed, "test", &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        let hooks = &config.pre_tool_use[0].hooks;
        assert_eq!(
            hooks[0].args,
            Some(vec![
                "a b".to_owned(),
                "$(touch /tmp/x)".to_owned(),
                ";".to_owned(),
                "*".to_owned(),
            ])
        );
        assert_eq!(hooks[1].args, Some(Vec::new()));
    }

    /// Edge case: `args: "x"`, `args: [1]` and `args: null` drop that hook only, one warning
    /// line each,
    /// and the entry's other hooks still load (ADR 0045 §4). Dropping the whole entry, or
    /// silently accepting the value, fails this.
    #[test]
    fn a_bad_args_skips_only_that_hook() {
        // `null` is not an array of strings: it must skip the hook, never read as "absent"
        // and fall back to `/bin/sh -c` (the shell form that can run the command).
        for bad in ["\"x\"", "[1]", "null"] {
            let source = format!(
                r#"{{"PreToolUse": [{{"matcher": "shell", "hooks": [
                    {{"type": "command", "command": "/hooks/good"}},
                    {{"type": "command", "command": "/hooks/bad", "args": {bad}}}
                ]}}]}}"#
            );
            let parsed: serde_json::Value = serde_json::from_str(&source).unwrap();
            let mut warnings = Vec::new();
            let config = parse_project_value(&parsed, "test", &mut warnings);
            assert_eq!(config.pre_tool_use.len(), 1, "{config:?}");
            assert_eq!(
                config.pre_tool_use[0].hooks.len(),
                1,
                "only the bad hook drops: {:?}",
                config.pre_tool_use[0]
            );
            assert_eq!(config.pre_tool_use[0].hooks[0].command, "/hooks/good");
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(
                warnings[0].contains("hooks.PreToolUse[0].hooks[1]")
                    && warnings[0].contains("args"),
                "one line names the dropped hook: {warnings:?}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, name: &str, text: &str) -> std::path::PathBuf {
        std::fs::create_dir_all(dir.join(".radian")).unwrap();
        let path = dir.join(".radian").join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn temp_dir(tag: &str) -> crate::test_support::TempDir {
        crate::test_support::TempDir::with_label("radian-host-project-hooks", tag)
    }

    /// The subset parses: the four deciding events, matchers kept raw, commands with
    /// timeouts carried.
    #[test]
    fn the_four_deciding_events_parse_with_matchers_and_timeouts() {
        let parsed: serde_json::Value = serde_json::from_str(
            r#"{
                "PreToolUse": [{"matcher": "shell|write", "hooks": [{"type": "command", "command": "echo hi", "timeout": 10}]}],
                "PermissionRequest": [{"matcher": "shell", "hooks": [{"type": "command", "command": "answer-ask"}]}],
                "PostToolUse": [{"matcher": "patch", "hooks": [{"type": "command", "command": "make lint"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "make check", "timeout": 30}]}]
            }"#,
        )
        .unwrap();
        let mut warnings = Vec::new();
        let config = parse_project_value(&parsed, "test", &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.pre_tool_use.len(), 1);
        assert_eq!(
            config.pre_tool_use[0].matcher.as_deref(),
            Some("shell|write")
        );
        assert_eq!(config.pre_tool_use[0].hooks.len(), 1);
        assert_eq!(
            config.pre_tool_use[0].hooks[0].timeout(),
            std::time::Duration::from_secs(10)
        );
        assert_eq!(config.permission_request.len(), 1);
        assert_eq!(config.permission_request[0].hooks[0].command, "answer-ask");
        assert_eq!(config.post_tool_use.len(), 1);
        assert_eq!(
            config.post_tool_use[0].hooks[0].timeout(),
            std::time::Duration::from_secs(crate::config::DEFAULT_HOOK_TIMEOUT_SECS)
        );
        assert_eq!(config.stop.len(), 1);
        assert_eq!(
            config.stop[0].hooks[0].timeout(),
            std::time::Duration::from_secs(30)
        );
    }

    /// The wrapped Claude settings shape parses too.
    #[test]
    fn the_wrapped_hooks_object_parses() {
        let parsed: serde_json::Value = serde_json::from_str(
            r#"{"hooks": {"PreToolUse": [{"matcher": "shell", "hooks": [{"type": "command", "command": "echo hi"}]}]}}"#,
        )
        .unwrap();
        let mut warnings = Vec::new();
        let config = parse_project_value(&parsed, "test", &mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.pre_tool_use.len(), 1);
    }

    /// Outside the subset never refuses: an unknown event, a non-command type, the
    /// non-command handler keys, a missing command and a bad timeout are each skipped
    /// with one line.
    #[test]
    fn outside_the_subset_is_skipped_never_refused() {
        let parsed: serde_json::Value = serde_json::from_str(
            r#"{
                "PreToolUse": [{"matcher": "read", "hooks": [{"type": "http", "command": "https://example.com/hook"}]}],
                "PostToolUse": [{"matcher": "read", "hooks": [{"type": "command", "command": "echo hi", "if": "true"}]}],
                "SessionStart": [{"matcher": "read", "hooks": [{"type": "command", "command": "echo hi"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": ""}]}]
            }"#,
        )
        .unwrap();
        let mut warnings = Vec::new();
        let config = parse_project_value(&parsed, "test", &mut warnings);
        assert!(config.is_empty(), "{config:?}");
        assert_eq!(warnings.len(), 4, "{warnings:?}");
        for key in [
            "hooks.PostToolUse[0]",
            "hooks.PreToolUse[0]",
            "hooks.SessionStart",
            "hooks.Stop[0]",
        ] {
            assert!(
                warnings.iter().any(|warning| warning.contains(key)),
                "{key}: {warnings:?}"
            );
        }
    }

    /// The async fields (ADR 0045 §5): `async` and `asyncRewake` parse on `PostToolUse` and
    /// `Stop`, `asyncRewake` implies `async`, and the async default timeout is 600 s. On
    /// `PreToolUse` that hook is dropped with one line while its sibling still loads.
    #[test]
    fn async_hooks_parse_on_post_and_stop_and_drop_on_pre() {
        let parsed: serde_json::Value = serde_json::from_str(
            r#"{
                "PostToolUse": [{"matcher": "patch", "hooks": [{"type": "command", "command": "make lint", "async": true}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "make verify", "asyncRewake": true}]}],
                "PreToolUse": [{"matcher": "shell", "hooks": [
                    {"type": "command", "command": "echo sync"},
                    {"type": "command", "command": "echo async", "async": true}
                ]}]
            }"#,
        )
        .unwrap();
        let mut warnings = Vec::new();
        let config = parse_project_value(&parsed, "test", &mut warnings);
        assert!(config.post_tool_use[0].hooks[0].async_hook);
        assert!(!config.post_tool_use[0].hooks[0].async_rewake);
        assert_eq!(
            config.post_tool_use[0].hooks[0].timeout().as_secs(),
            crate::config::DEFAULT_ASYNC_HOOK_TIMEOUT_SECS,
            "an async hook without `timeout` defaults to 600 s, not 60"
        );
        assert!(config.stop[0].hooks[0].async_rewake);
        assert!(config.stop[0].hooks[0].is_async());
        assert_eq!(
            config.pre_tool_use[0].hooks.len(),
            1,
            "the async PreToolUse hook drops, its sibling still loads"
        );
        assert_eq!(config.pre_tool_use[0].hooks[0].command, "echo sync");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("hooks.PreToolUse[0].hooks[1]"),
            "one line names the dropped async hook: {warnings:?}"
        );
    }

    /// A pre-call hook that writes `"async": false` or `"asyncRewake": false` still carries
    /// a field those events do not accept: presence, not the effective value, drops it with
    /// one line, and its siblings still load (ADR 0045 §5).
    #[test]
    fn an_explicitly_false_async_field_on_pre_is_still_dropped() {
        let parsed: serde_json::Value = serde_json::from_str(
            r#"{
                "PreToolUse": [{"matcher": "shell", "hooks": [
                    {"type": "command", "command": "echo async-off", "async": false},
                    {"type": "command", "command": "echo rewake-off", "asyncRewake": false},
                    {"type": "command", "command": "echo keep"}
                ]}],
                "PermissionRequest": [{"hooks": [
                    {"type": "command", "command": "echo ask", "async": false}
                ]}]
            }"#,
        )
        .unwrap();
        let mut warnings = Vec::new();
        let config = parse_project_value(&parsed, "test", &mut warnings);
        assert_eq!(
            config.pre_tool_use[0].hooks.len(),
            1,
            "the false-valued fields still drop their hooks: {config:?}"
        );
        assert_eq!(config.pre_tool_use[0].hooks[0].command, "echo keep");
        assert!(
            config.permission_request.is_empty(),
            "PermissionRequest drops it too: {config:?}"
        );
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        for key in [
            "hooks.PreToolUse[0].hooks[0]",
            "hooks.PreToolUse[0].hooks[1]",
            "hooks.PermissionRequest[0].hooks[0]",
        ] {
            assert!(
                warnings.iter().any(|warning| warning.contains(key)),
                "{key}: {warnings:?}"
            );
        }
    }

    /// A non-object top level warns once and loads nothing.
    #[test]
    fn a_non_object_top_level_warns_and_loads_nothing() {
        let parsed = serde_json::Value::Array(vec![]);
        let mut warnings = Vec::new();
        let config = parse_project_value(&parsed, "test", &mut warnings);
        assert!(config.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    /// `load_project` gates on trust. Untrusted: no hooks, one line in ADR 0031 T3's shape.
    /// Trusted: the hooks plus the loaded line. A malformed file warns and loads nothing
    /// instead of breaking startup.
    #[test]
    fn load_project_gates_on_trust_and_never_breaks_startup() {
        let dir = temp_dir("project-gate");
        // No file: nothing loads, no lines.
        let (config, lines) = load_project(&dir, false);
        assert!(config.is_empty());
        assert!(lines.is_empty(), "{lines:?}");

        write(
            &dir,
            FILE_NAME,
            r#"{"PreToolUse": [{"matcher": "shell", "hooks": [{"type": "command", "command": "echo hi"}]}]}"#,
        );
        // Untrusted: nothing loads; the line says so and names `--trust`.
        let (config, lines) = load_project(&dir, false);
        assert!(config.is_empty(), "untrusted must load nothing");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("not loaded (untrusted)") && lines[0].contains("--trust"),
            "{lines:?}"
        );
        // Trusted: the hook plus the loaded line.
        let (config, lines) = load_project(&dir, true);
        assert_eq!(config.pre_tool_use.len(), 1);
        assert!(
            lines.iter().any(|line| line.contains("loaded 1 hook")),
            "{lines:?}"
        );
        // Malformed while trusted: a warning, no hooks, no error.
        write(&dir, FILE_NAME, "{not json");
        let (config, lines) = load_project(&dir, true);
        assert!(config.is_empty());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("skipped"), "{lines:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The preview carries events, matchers and commands as written.
    #[test]
    fn the_preview_lists_raw_entries() {
        let dir = temp_dir("preview");
        assert_eq!(preview_project(&dir), None, "no file means no preview");
        write(
            &dir,
            FILE_NAME,
            r#"{"PreToolUse": [{"matcher": "shell", "hooks": [{"type": "command", "command": "make check"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "make verify"}]}]}"#,
        );
        let preview = preview_project(&dir).expect("the file exists");
        assert!(preview.error.is_none(), "{:?}", preview.error);
        assert_eq!(preview.entries.len(), 2);
        let lines = preview_lines(&preview);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("2 hooks to load once trusted")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("PreToolUse")
                && line.contains("shell")
                && line.contains("make check")),
            "event, matcher, command: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("Stop") && line.contains("make verify")),
            "{lines:?}"
        );
        // A broken file previews as an error, not entries.
        write(&dir, FILE_NAME, "{not json");
        let preview = preview_project(&dir).expect("the file exists");
        assert!(preview.entries.is_empty());
        assert!(preview.error.is_some(), "{preview:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
