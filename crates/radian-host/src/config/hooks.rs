//! The `[hooks]` section (ADR 0045 slice A): the Claude/Codex command-hook subset in
//! `config.toml`.
//!
//! Only four events exist — `PreToolUse`, `PostToolUse`, `Stop` and `PermissionRequest` — each
//! a list of `{ matcher, hooks }` entries, each hook a `{ type = "command", command, args, timeout }`
//! table. `args` is optional: with it `command` is the program and the hook starts without a
//! shell; without it `command` runs through `/bin/sh -c` (ADR 0045 §4). `matcher` is Claude's case-sensitive pattern over **Radian** tool names
//! (`shell`, `write`, `patch`, `read`, …): `*` or a missing/empty matcher matches every
//! tool, otherwise the pattern is a regular expression anchored to the whole tool name
//! (`^(?:<matcher>)$`). There is no runtime aliasing of Claude's tool names: an import
//! renames them once, never the reader.
//!
//! Anything outside the subset never refuses the file. An unknown event name, a malformed
//! entry, a hook whose `type` is not `"command"`, or an entry carrying one of the
//! non-command handler keys (`async`, `if`, `http`, `prompt`, `agent`) is skipped with one
//! line in [`Config::skipped`], so the startup line says it once. An `args` that is not an
//! array of strings is not a structural defect: it drops that hook only, one line named down
//! to the hook, and the entry's other hooks still load.

use toml::Value;

use super::Config;

/// The hook events this slice implements. The single source for the match arms below,
/// the project hooks reader, and the manual's drift test: adding an event means adding
/// it here first.
pub const HOOK_EVENTS: &[&str] = &[PRE_TOOL_USE, PERMISSION_REQUEST, POST_TOOL_USE, STOP];
/// The tool-call hook that runs before the tool runs.
pub const PRE_TOOL_USE: &str = "PreToolUse";
/// The hook that answers an `ask` before anyone is asked (ADR 0045 §4).
pub const PERMISSION_REQUEST: &str = "PermissionRequest";
/// The tool-call hook that runs after the tool runs.
pub const POST_TOOL_USE: &str = "PostToolUse";
/// The hook that runs when the model stops.
pub const STOP: &str = "Stop";

/// The default per-hook timeout, in seconds (Claude's default): how long one hook command
/// may run before it counts as a hook error (fail closed).
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 60;

/// The `[hooks] intent_stop` default: the built-in validator runs unless turned off.
pub const DEFAULT_INTENT_STOP: bool = true;

/// One `{ type = "command", command, args, timeout }` hook.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookCommand {
    /// The shell command to run (`/bin/sh -c`), or — when `args` is `Some` — the program to
    /// start directly, inside the session sandbox when enforced.
    pub command: String,
    /// The exec-form arguments (ADR 0045 §4). `None` runs `command` through `/bin/sh -c`;
    /// `Some` starts `command` directly with exactly these arguments, never through a shell.
    /// An empty list is a run with no arguments.
    pub args: Option<Vec<String>>,
    /// Seconds before the run counts as a hook error. `None` means the default.
    pub timeout_secs: Option<u64>,
}

impl HookCommand {
    /// The effective timeout: the configured one, or the default when unset or zero (a zero
    /// timeout would fire at once, which is never what a config means).
    pub fn timeout(&self) -> std::time::Duration {
        match self.timeout_secs {
            Some(0) | None => std::time::Duration::from_secs(DEFAULT_HOOK_TIMEOUT_SECS),
            Some(secs) => std::time::Duration::from_secs(secs),
        }
    }
}

/// One `{ matcher, hooks }` entry under an event name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookMatcher {
    /// The tool-name filter (Claude's pattern: `*`/empty matches all, otherwise a regex
    /// anchored to the whole name). `None`/empty/`*` matches every tool; `Stop` ignores it.
    pub matcher: Option<String>,
    pub hooks: Vec<HookCommand>,
}

/// The `[hooks]` section (ADR 0045 slice A): user-level command hooks only. The project
/// level (`<workspace>/.radian/hooks.json`, slice B) is not loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HooksConfig {
    pub pre_tool_use: Vec<HookMatcher>,
    /// The hooks that answer a permission `ask` (ADR 0045 §4). Matched by the same tool-name
    /// pattern as `PreToolUse`.
    pub permission_request: Vec<HookMatcher>,
    pub post_tool_use: Vec<HookMatcher>,
    pub stop: Vec<HookMatcher>,
    /// `[hooks] intent_stop`: the built-in intent-stop validator on the `Stop` path
    /// (backlog `turn-end-continuation.md` §B). `None` means the default, **on**; the
    /// host reads `unwrap_or(true)`. `false` turns the validator off (user `Stop`
    /// hooks still run).
    pub intent_stop: Option<bool>,
}

impl HooksConfig {
    /// Whether any hook is configured at all.
    pub fn is_empty(&self) -> bool {
        self.pre_tool_use.is_empty()
            && self.permission_request.is_empty()
            && self.post_tool_use.is_empty()
            && self.stop.is_empty()
    }
}

/// Whether one matcher runs: `*`/empty/missing always does; otherwise the pattern must
/// compile as `^(?:<matcher>)$` (the runtime's anchoring — `sh` does not match `shell`).
/// The inspector's hooks page reads this, so the flag follows the same compile the
/// runtime uses without a second regex dependency in the TUI.
pub fn matcher_effective(matcher: Option<&str>) -> bool {
    match matcher {
        None | Some("") | Some("*") => true,
        Some(pattern) => regex::Regex::new(&format!("^(?:{pattern})$")).is_ok(),
    }
}

/// Handler keys this slice does not implement (ADR 0045): an entry carrying any of them is
/// skipped, never run.
const NON_COMMAND_HANDLER_KEYS: [&str; 5] = ["async", "if", "http", "prompt", "agent"];

/// Reads `[hooks]` (already looked up as a TOML value). Never refuses: everything outside
/// the subset is skipped and named, so one startup line says what was ignored.
pub fn parse_hooks(value: &Value, config: &mut Config) -> Result<(), super::ConfigError> {
    let Some(table) = value.as_table() else {
        config.skipped.push("hooks".into());
        return Ok(());
    };
    for (event, entries) in table {
        // `[hooks] intent_stop` (backlog `turn-end-continuation.md` §B.6): the
        // built-in validator's kill switch. A boolean is carried; anything else is
        // skipped and named (the `tui.*` rule: a bad limit never refuses startup).
        if event.as_str() == "intent_stop" {
            match entries {
                Value::Boolean(on) => config.hooks.intent_stop = Some(*on),
                _ => config.skipped.push("hooks.intent_stop".into()),
            }
            continue;
        }
        let list = match event.as_str() {
            PRE_TOOL_USE => &mut config.hooks.pre_tool_use,
            PERMISSION_REQUEST => &mut config.hooks.permission_request,
            POST_TOOL_USE => &mut config.hooks.post_tool_use,
            STOP => &mut config.hooks.stop,
            _ => {
                config.skipped.push(format!("hooks.{event}"));
                continue;
            }
        };
        let Some(items) = entries.as_array() else {
            config.skipped.push(format!("hooks.{event}"));
            continue;
        };
        for (index, item) in items.iter().enumerate() {
            let parsed = parse_entry(item);
            // A bad `args` drops that hook only: the entry's other hooks still load (ADR
            // 0045 §4). Each dropped hook is one line, named down to the hook.
            for hook_index in &parsed.bad_args {
                config
                    .skipped
                    .push(format!("hooks.{event}[{index}].hooks[{hook_index}]"));
            }
            match parsed.entry {
                Some(entry) => list.push(entry),
                // One entry, one line: a bad hook drops its whole entry, never a sibling
                // entry, and never the file. When every hook was dropped for a bad `args`,
                // the per-hook lines above already say it.
                None if parsed.bad_args.is_empty() => {
                    config.skipped.push(format!("hooks.{event}[{index}]"));
                }
                None => {}
            }
        }
    }
    Ok(())
}

/// One parsed `{ matcher, hooks }` entry, plus the hooks dropped only for a bad `args`.
struct ParsedEntry {
    /// The entry, `None` when it is skipped as a whole (then `bad_args` is empty) or when
    /// every hook was dropped for a bad `args` (then `bad_args` names them).
    entry: Option<HookMatcher>,
    /// Indexes of hooks dropped because `args` is not an array of strings. The rest of the
    /// entry still loads; each index is one warning line.
    bad_args: Vec<usize>,
}

impl ParsedEntry {
    /// The whole entry is outside the subset. One line names the entry.
    fn skipped() -> Self {
        Self {
            entry: None,
            bad_args: Vec::new(),
        }
    }
}

/// Reads one `{ matcher, hooks }` entry. A structural defect skips the whole entry (one
/// line): a hook whose `type` is not `"command"`, a hook carrying a non-command handler
/// key, a missing or empty `command`, or a bad `timeout`. A bad `args` drops only that hook.
fn parse_entry(item: &Value) -> ParsedEntry {
    let Some(table) = item.as_table() else {
        return ParsedEntry::skipped();
    };
    let matcher = match table.get("matcher") {
        None => None,
        Some(Value::String(matcher)) => Some(matcher.clone()),
        Some(_) => return ParsedEntry::skipped(),
    };
    let Some(hooks) = table.get("hooks").and_then(Value::as_array) else {
        return ParsedEntry::skipped();
    };
    if hooks.is_empty() {
        return ParsedEntry::skipped();
    }
    let mut commands = Vec::new();
    let mut bad_args = Vec::new();
    for (index, hook) in hooks.iter().enumerate() {
        match parse_command(hook) {
            CommandParse::Ok(command) => commands.push(command),
            CommandParse::BadArgs => bad_args.push(index),
            CommandParse::Skip => return ParsedEntry::skipped(),
        }
    }
    ParsedEntry {
        entry: (!commands.is_empty()).then_some(HookMatcher {
            matcher,
            hooks: commands,
        }),
        bad_args,
    }
}

/// One hook row's parse result.
enum CommandParse {
    Ok(HookCommand),
    /// `args` is present but is not an array of strings: drop this hook only.
    BadArgs,
    /// Outside the command subset: drop the whole entry.
    Skip,
}

/// Reads one `{ type, command, args, timeout }` hook.
fn parse_command(hook: &Value) -> CommandParse {
    let Some(table) = hook.as_table() else {
        return CommandParse::Skip;
    };
    if table.get("type").and_then(Value::as_str) != Some("command") {
        return CommandParse::Skip;
    }
    if NON_COMMAND_HANDLER_KEYS
        .iter()
        .any(|key| table.contains_key(*key))
    {
        return CommandParse::Skip;
    }
    let Some(command) = table.get("command").and_then(Value::as_str) else {
        return CommandParse::Skip;
    };
    if command.is_empty() {
        return CommandParse::Skip;
    }
    let timeout_secs = match table.get("timeout") {
        None => None,
        Some(Value::Integer(secs)) if *secs >= 0 => Some(*secs as u64),
        Some(_) => return CommandParse::Skip,
    };
    let args = match table.get("args") {
        None => None,
        Some(Value::Array(items)) => {
            let mut args = Vec::with_capacity(items.len());
            for item in items {
                let Some(arg) = item.as_str() else {
                    return CommandParse::BadArgs;
                };
                args.push(arg.to_owned());
            }
            Some(args)
        }
        Some(_) => return CommandParse::BadArgs,
    };
    CommandParse::Ok(HookCommand {
        command: command.to_owned(),
        args,
        timeout_secs,
    })
}

#[cfg(test)]
mod args_tests {
    use super::*;
    use std::path::Path;

    fn parse_str(text: &str) -> Config {
        super::super::parse(Path::new("/tmp/test/config.toml"), text).unwrap()
    }

    /// `args` is the exec-form list (ADR 0045 §4): strings are carried verbatim, an empty
    /// list means "no arguments" (still no shell), and a missing `args` means `/bin/sh -c`.
    #[test]
    fn args_parses_as_the_exec_form_argument_list() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[[hooks.PreToolUse]]\n",
            "matcher = \"shell\"\n",
            "[[hooks.PreToolUse.hooks]]\n",
            "type = \"command\"\n",
            "command = \"/work/example/hooks/check\"\n",
            "args = [\"a b\", \"$(touch /tmp/x)\", \";\", \"*\"]\n",
            "[[hooks.PostToolUse]]\n",
            "matcher = \"patch\"\n",
            "[[hooks.PostToolUse.hooks]]\n",
            "type = \"command\"\n",
            "command = \"/work/example/hooks/empty\"\n",
            "args = []\n",
            "[[hooks.Stop]]\n",
            "[[hooks.Stop.hooks]]\n",
            "type = \"command\"\n",
            "command = \"make check\"\n",
        ));
        assert!(config.skipped.is_empty(), "{:?}", config.skipped);
        assert_eq!(
            config.hooks.pre_tool_use[0].hooks[0].args,
            Some(vec![
                "a b".to_owned(),
                "$(touch /tmp/x)".to_owned(),
                ";".to_owned(),
                "*".to_owned(),
            ]),
            "arguments stay verbatim, with no shell expansion"
        );
        assert_eq!(
            config.hooks.post_tool_use[0].hooks[0].args,
            Some(Vec::new()),
            "an empty list is exec form with no arguments"
        );
        assert_eq!(config.hooks.stop[0].hooks[0].args, None);
    }

    /// Edge case: `args = "x"` and `args = [1]` drop that hook only, one warning line each,
    /// and the entry's other hooks still load (ADR 0045 §4). Dropping the whole entry, or
    /// silently accepting the value, fails this.
    #[test]
    fn a_bad_args_skips_only_that_hook() {
        for bad in ["args = \"x\"", "args = [1]"] {
            let config = parse_str(&format!(
                concat!(
                    "schema_version = 1\n",
                    "[[hooks.PreToolUse]]\n",
                    "matcher = \"shell\"\n",
                    "[[hooks.PreToolUse.hooks]]\n",
                    "type = \"command\"\n",
                    "command = \"/work/example/hooks/good\"\n",
                    "[[hooks.PreToolUse.hooks]]\n",
                    "type = \"command\"\n",
                    "command = \"/work/example/hooks/bad\"\n",
                    "{}\n",
                ),
                bad
            ));
            assert_eq!(
                config.hooks.pre_tool_use.len(),
                1,
                "the entry still loads: {:?}",
                config.hooks
            );
            assert_eq!(
                config.hooks.pre_tool_use[0].hooks.len(),
                1,
                "only the bad hook drops: {:?}",
                config.hooks.pre_tool_use[0]
            );
            assert_eq!(
                config.hooks.pre_tool_use[0].hooks[0].command,
                "/work/example/hooks/good"
            );
            assert_eq!(
                config.skipped,
                vec!["hooks.PreToolUse[0].hooks[1]".to_owned()],
                "one line names the dropped hook: {bad}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse_str(text: &str) -> Config {
        super::super::parse(Path::new("/tmp/test/config.toml"), text).unwrap()
    }

    /// The subset parses: four events, matchers kept raw, commands with timeouts carried.
    #[test]
    fn the_four_events_parse_with_matchers_and_timeouts() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[[hooks.PreToolUse]]\n",
            "matcher = \"shell|write\"\n",
            "[[hooks.PreToolUse.hooks]]\n",
            "type = \"command\"\n",
            "command = \"echo hi\"\n",
            "timeout = 10\n",
            "[[hooks.PermissionRequest]]\n",
            "matcher = \"shell\"\n",
            "[[hooks.PermissionRequest.hooks]]\n",
            "type = \"command\"\n",
            "command = \"answer-ask\"\n",
            "[[hooks.PostToolUse]]\n",
            "matcher = \"patch\"\n",
            "[[hooks.PostToolUse.hooks]]\n",
            "type = \"command\"\n",
            "command = \"make lint\"\n",
            "[[hooks.Stop]]\n",
            "[[hooks.Stop.hooks]]\n",
            "type = \"command\"\n",
            "command = \"make check\"\n",
            "timeout = 30\n",
        ));
        assert_eq!(config.hooks.pre_tool_use.len(), 1);
        assert_eq!(
            config.hooks.pre_tool_use[0].matcher.as_deref(),
            Some("shell|write")
        );
        assert_eq!(config.hooks.pre_tool_use[0].hooks.len(), 1);
        assert_eq!(
            config.hooks.pre_tool_use[0].hooks[0].timeout(),
            std::time::Duration::from_secs(10)
        );
        assert_eq!(config.hooks.permission_request.len(), 1);
        assert_eq!(
            config.hooks.permission_request[0].hooks[0].command,
            "answer-ask"
        );
        assert_eq!(config.hooks.post_tool_use.len(), 1);
        assert_eq!(
            config.hooks.post_tool_use[0].hooks[0].timeout(),
            std::time::Duration::from_secs(DEFAULT_HOOK_TIMEOUT_SECS)
        );
        assert_eq!(config.hooks.stop.len(), 1);
        assert!(config.skipped.is_empty(), "{:?}", config.skipped);
    }

    /// `[hooks] intent_stop` (backlog `turn-end-continuation.md` §B.6): a boolean is
    /// carried (`None` when absent, the default on); anything else is skipped, never
    /// refused.
    #[test]
    fn the_intent_stop_kill_switch_is_carried_and_a_bad_value_is_skipped() {
        let config = parse_str("schema_version = 1\n[hooks]\nintent_stop = false\n");
        assert_eq!(config.hooks.intent_stop, Some(false));
        assert!(config.skipped.is_empty(), "{:?}", config.skipped);
        let config = parse_str("schema_version = 1\n");
        assert_eq!(config.hooks.intent_stop, None);
        let config = parse_str("schema_version = 1\n[hooks]\nintent_stop = \"off\"\n");
        assert_eq!(config.hooks.intent_stop, None);
        assert!(
            config.skipped.contains(&"hooks.intent_stop".to_string()),
            "{:?}",
            config.skipped
        );
    }

    /// Outside the subset never refuses: an unknown event, a non-command type, and the
    /// non-command handler keys are each skipped with one line.
    #[test]
    fn outside_the_subset_is_skipped_never_refused() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[[hooks.PreToolUse]]\n",
            "matcher = \"read\"\n",
            "[[hooks.PreToolUse.hooks]]\n",
            "type = \"http\"\n",
            "command = \"https://example.com/hook\"\n",
            "[[hooks.PostToolUse]]\n",
            "matcher = \"read\"\n",
            "[[hooks.PostToolUse.hooks]]\n",
            "type = \"command\"\n",
            "command = \"echo hi\"\n",
            "async = true\n",
            "[[hooks.SessionStart]]\n",
            "matcher = \"read\"\n",
            "[[hooks.SessionStart.hooks]]\n",
            "type = \"command\"\n",
            "command = \"echo hi\"\n",
        ));
        assert!(config.hooks.is_empty(), "{:?}", config.hooks);
        for key in [
            "hooks.PostToolUse[0]",
            "hooks.PreToolUse[0]",
            "hooks.SessionStart",
        ] {
            assert!(
                config.skipped.contains(&key.to_string()),
                "{:?}",
                config.skipped
            );
        }
    }
}
