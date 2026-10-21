//! The `[hooks]` section (ADR 0045 slice A): the Claude/Codex command-hook subset in
//! `config.toml`.
//!
//! Only three events exist — `PreToolUse`, `PostToolUse`, `Stop` — each a list of
//! `{ matcher, hooks }` entries, each hook a `{ type = "command", command, timeout }`
//! table. `matcher` is Claude's case-sensitive pattern over **Radian** tool names
//! (`shell`, `write`, `patch`, `read`, …): `*` or a missing/empty matcher matches every
//! tool, otherwise the pattern is a regular expression anchored to the whole tool name
//! (`^(?:<matcher>)$`). There is no runtime aliasing of Claude's tool names: an import
//! renames them once, never the reader.
//!
//! Anything outside the subset never refuses the file. An unknown event name, a malformed
//! entry, a hook whose `type` is not `"command"`, or an entry carrying one of the
//! non-command handler keys (`async`, `if`, `http`, `prompt`, `agent`) is skipped with one
//! line in [`Config::skipped`], so the startup line says it once.

use toml::Value;

use super::Config;

/// The hook events this slice implements. The single source for the match arms below,
/// the project hooks reader, and the manual's drift test: adding an event means adding
/// it here first.
pub const HOOK_EVENTS: &[&str] = &[PRE_TOOL_USE, POST_TOOL_USE, STOP];
/// The tool-call hook that runs before the tool runs.
pub const PRE_TOOL_USE: &str = "PreToolUse";
/// The tool-call hook that runs after the tool runs.
pub const POST_TOOL_USE: &str = "PostToolUse";
/// The hook that runs when the model stops.
pub const STOP: &str = "Stop";

/// The default per-hook timeout, in seconds (Claude's default): how long one hook command
/// may run before it counts as a hook error (fail closed).
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 60;

/// One `{ type = "command", command, timeout }` hook.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookCommand {
    /// The shell command to run (`/bin/sh -c`), inside the session sandbox when enforced.
    pub command: String,
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
        self.pre_tool_use.is_empty() && self.post_tool_use.is_empty() && self.stop.is_empty()
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
            match parse_entry(item) {
                Some(entry) => list.push(entry),
                // One entry, one line: a bad hook drops its whole entry, never a sibling
                // entry, and never the file.
                None => config.skipped.push(format!("hooks.{event}[{index}]")),
            }
        }
    }
    Ok(())
}

/// Reads one `{ matcher, hooks }` entry. `None` means the whole entry is skipped (one
/// line): a hook whose `type` is not `"command"`, a hook carrying a non-command handler
/// key, a missing or empty `command`, or a bad `timeout`.
fn parse_entry(item: &Value) -> Option<HookMatcher> {
    let table = item.as_table()?;
    let matcher = match table.get("matcher") {
        None => None,
        Some(Value::String(matcher)) => Some(matcher.clone()),
        Some(_) => return None,
    };
    let hooks = table.get("hooks")?.as_array()?;
    if hooks.is_empty() {
        return None;
    }
    let mut commands = Vec::new();
    for hook in hooks {
        commands.push(parse_command(hook)?);
    }
    Some(HookMatcher {
        matcher,
        hooks: commands,
    })
}

/// Reads one `{ type, command, timeout }` hook. `None` means the entry is skipped.
fn parse_command(hook: &Value) -> Option<HookCommand> {
    let table = hook.as_table()?;
    if table.get("type").and_then(Value::as_str) != Some("command") {
        return None;
    }
    if NON_COMMAND_HANDLER_KEYS
        .iter()
        .any(|key| table.contains_key(*key))
    {
        return None;
    }
    let command = table.get("command").and_then(Value::as_str)?;
    if command.is_empty() {
        return None;
    }
    let timeout_secs = match table.get("timeout") {
        None => None,
        Some(Value::Integer(secs)) if *secs >= 0 => Some(*secs as u64),
        Some(_) => return None,
    };
    Some(HookCommand {
        command: command.to_owned(),
        timeout_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse_str(text: &str) -> Config {
        super::super::parse(Path::new("/tmp/test/config.toml"), text).unwrap()
    }

    /// The subset parses: three events, matchers kept raw, commands with timeouts carried.
    #[test]
    fn the_three_events_parse_with_matchers_and_timeouts() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[[hooks.PreToolUse]]\n",
            "matcher = \"shell|write\"\n",
            "[[hooks.PreToolUse.hooks]]\n",
            "type = \"command\"\n",
            "command = \"echo hi\"\n",
            "timeout = 10\n",
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
