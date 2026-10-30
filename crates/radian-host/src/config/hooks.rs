//! The `[hooks]` section (ADR 0045 slice A): the Claude/Codex command-hook subset in
//! `config.toml`.
//!
//! Only six events exist — `PreToolUse`, `PostToolUse`, `Stop`, `PermissionRequest`,
//! `StopFailure` and `Notification` — each
//! a list of `{ matcher, hooks }` entries, each hook a `{ type = "command", command, args,
//! timeout, async, asyncRewake }`
//! table. `args` is optional: with it `command` is the program and the hook starts without a
//! shell; without it `command` runs through `/bin/sh -c` (ADR 0045 §4). `matcher` is Claude's case-sensitive pattern over **Radian** tool names
//! (`shell`, `write`, `patch`, `read`, …): `*` or a missing/empty matcher matches every
//! tool, otherwise the pattern is a regular expression anchored to the whole tool name
//! (`^(?:<matcher>)$`). There is no runtime aliasing of Claude's tool names: an import
//! renames them once, never the reader.
//!
//! Anything outside the subset never refuses the file. An unknown event name, a malformed
//! entry, a hook whose `type` is not `"command"`, or an entry carrying one of the
//! non-command handler keys (`if`, `http`, `prompt`, `agent`) is skipped with one line in
//! [`Config::skipped`], so the startup line says it once. An `args` that is not an array of
//! strings is not a structural defect: it drops that hook only, one line named down to the
//! hook, and the entry's other hooks still load. The same goes for `async`/`asyncRewake` on
//! an event that does not accept them (`PreToolUse`, `PermissionRequest`).
//!
//! `async` (default `false`) and `asyncRewake` (default `false`; `true` implies `async`)
//! are accepted on `PostToolUse` and `Stop` only (ADR 0045 §1/§5): the hook starts like a
//! synchronous one and the tool result or the `Stop` chain continues at once. On the
//! observe-only `StopFailure` and `Notification` events `async`/`asyncRewake` are accepted
//! and inert: those hooks always run in the background (§6).

use toml::Value;

use super::Config;

/// The hook events this slice implements. The single source for the match arms below,
/// the project hooks reader, and the manual's drift test: adding an event means adding
/// it here first.
pub const HOOK_EVENTS: &[&str] = &[
    PRE_TOOL_USE,
    PERMISSION_REQUEST,
    POST_TOOL_USE,
    STOP,
    STOP_FAILURE,
    NOTIFICATION,
];
/// The tool-call hook that runs before the tool runs.
pub const PRE_TOOL_USE: &str = "PreToolUse";
/// The hook that answers an `ask` before anyone is asked (ADR 0045 §4).
pub const PERMISSION_REQUEST: &str = "PermissionRequest";
/// The tool-call hook that runs after the tool runs.
pub const POST_TOOL_USE: &str = "PostToolUse";
/// The hook that runs when the model stops.
pub const STOP: &str = "Stop";
/// The observe-only hook that runs when a turn ends on a model error (ADR 0045 §6).
pub const STOP_FAILURE: &str = "StopFailure";
/// The observe-only hook that runs when radian is about to ask a human, or a TUI turn left
/// the session idle (ADR 0045 §6).
pub const NOTIFICATION: &str = "Notification";

/// The `StopFailure` matcher's closed set: the [`radian_core::MODEL_ERROR_KINDS`] names, so
/// the parser, the manual and the drift test read one declaration.
pub const STOP_FAILURE_MATCHERS: &[&str] = radian_core::MODEL_ERROR_KINDS;
/// The `Notification` matcher's closed set (ADR 0045 §6).
pub const NOTIFICATION_TYPES: &[&str] = radian_permissions::NOTIFICATION_TYPES;
pub const NOTIFICATION_PERMISSION_PROMPT: &str = radian_permissions::NOTIFICATION_PERMISSION_PROMPT;
pub const NOTIFICATION_IDLE_PROMPT: &str = radian_permissions::NOTIFICATION_IDLE_PROMPT;

/// The `idle_prompt` delay, in seconds (ADR 0045 §6): a fixed constant, no config key.
pub const IDLE_PROMPT_SECS: u64 = 60;

/// The default per-hook timeout, in seconds (Claude's default): how long one hook command
/// may run before it counts as a hook error (fail closed).
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 60;

/// The default timeout of an `async` hook that does not set `timeout`, in seconds (ADR 0045
/// §5): background checks are allowed to be slow.
pub const DEFAULT_ASYNC_HOOK_TIMEOUT_SECS: u64 = 600;

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
    /// `async`: the hook runs in the background (ADR 0045 §5). Accepted on `PostToolUse`
    /// and `Stop` only; the reader drops it elsewhere with one warning.
    pub async_hook: bool,
    /// `asyncRewake`: an `async` hook whose exit 2 wakes an idle session through the wake
    /// gate (ADR 0045 §5). `true` implies [`HookCommand::async_hook`].
    pub async_rewake: bool,
}

impl HookCommand {
    /// Whether this hook runs in the background. `asyncRewake` implies `async`.
    pub fn is_async(&self) -> bool {
        self.async_hook || self.async_rewake
    }

    /// The effective timeout: the configured one, or the default when unset or zero (a zero
    /// timeout would fire at once, which is never what a config means). An `async` hook's
    /// default is [`DEFAULT_ASYNC_HOOK_TIMEOUT_SECS`], not the synchronous one.
    pub fn timeout(&self) -> std::time::Duration {
        match self.timeout_secs {
            Some(0) | None => std::time::Duration::from_secs(if self.is_async() {
                DEFAULT_ASYNC_HOOK_TIMEOUT_SECS
            } else {
                DEFAULT_HOOK_TIMEOUT_SECS
            }),
            Some(secs) => std::time::Duration::from_secs(secs),
        }
    }

    /// The timeout of an observe-only run (`StopFailure`, `Notification`): the configured one,
    /// or the synchronous 60 s default. `async` is inert on those events and never moves this
    /// default (ADR 0045 §6).
    pub fn observe_timeout(&self) -> std::time::Duration {
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
    /// The observe-only `StopFailure` entries (ADR 0045 §6).
    pub stop_failure: Vec<HookMatcher>,
    /// The observe-only `Notification` entries (ADR 0045 §6).
    pub notification: Vec<HookMatcher>,
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
            && self.stop_failure.is_empty()
            && self.notification.is_empty()
    }
}

/// Whether one tool-event matcher runs: `*`/empty/missing always does; otherwise the pattern
/// must compile as `^(?:<matcher>)$` (the runtime's anchoring — `sh` does not match `shell`).
/// The inspector's hooks page reads this, so the flag follows the same compile the
/// runtime uses without a second regex dependency in the TUI.
pub fn matcher_effective(matcher: Option<&str>) -> bool {
    match matcher {
        None | Some("") | Some("*") => true,
        Some(pattern) => regex::Regex::new(&format!("^(?:{pattern})$")).is_ok(),
    }
}

/// The `StopFailure`/`Notification` matcher's effective flag: `*`/empty/missing always
/// matches; otherwise the value must be one of `allowed` (a closed set, never a regex).
pub fn observe_matcher_effective(allowed: &[&str], matcher: Option<&str>) -> bool {
    match matcher {
        None | Some("") | Some("*") => true,
        Some(value) => allowed.contains(&value),
    }
}

/// Handler keys this slice does not implement (ADR 0045): an entry carrying any of them is
/// skipped, never run. `async` is no longer one of them; it is a field of its own.
const NON_COMMAND_HANDLER_KEYS: [&str; 4] = ["if", "http", "prompt", "agent"];

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
            STOP_FAILURE => &mut config.hooks.stop_failure,
            NOTIFICATION => &mut config.hooks.notification,
            _ => {
                config.skipped.push(format!("hooks.{event}"));
                continue;
            }
        };
        let Some(items) = entries.as_array() else {
            config.skipped.push(format!("hooks.{event}"));
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
                config
                    .skipped
                    .push(format!("hooks.{event}[{index}].hooks[{hook_index}]"));
            }
            match parsed.entry {
                Some(entry) => list.push(entry),
                // One entry, one line: a bad hook drops its whole entry, never a sibling
                // entry, and never the file. When every hook was dropped one by one, the
                // per-hook lines above already say it. A structural skip keeps its own
                // line even when a sibling hook was also dropped one by one: the two
                // lines name different defects.
                None if parsed.structural_skip() || !parsed.dropped_any() => {
                    config.skipped.push(format!("hooks.{event}[{index}]"));
                }
                None => {}
            }
        }
    }
    Ok(())
}

/// One parsed `{ matcher, hooks }` entry, plus the hooks dropped one by one.
struct ParsedEntry {
    /// The entry, `None` when it is skipped as a whole or when every hook was dropped
    /// individually (then `dropped` names them).
    entry: Option<HookMatcher>,
    /// Indexes of hooks dropped for a bad `args`, or for `async`/`asyncRewake` on an event
    /// that does not accept them. The rest of the entry still loads; each index is one
    /// warning line. A later structural defect keeps these, so both defects stay named.
    dropped: Vec<usize>,
    /// The whole entry is outside the subset (one line names the entry).
    structural: bool,
}

impl ParsedEntry {
    /// The whole entry is outside the subset. One line names the entry.
    fn skipped() -> Self {
        Self {
            entry: None,
            dropped: Vec::new(),
            structural: true,
        }
    }

    /// The whole entry is outside the subset, but hooks before the structural defect were
    /// already dropped one by one: keep their indexes so each still gets its own line.
    fn skipped_after(dropped: Vec<usize>) -> Self {
        Self {
            entry: None,
            dropped,
            structural: true,
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

    /// Whether the whole entry was skipped for a structural defect (its own line is due
    /// even when a sibling hook was also dropped one by one).
    fn structural_skip(&self) -> bool {
        self.structural
    }
}

/// Reads one `{ matcher, hooks }` entry. A structural defect skips the whole entry (one
/// line): a hook whose `type` is not `"command"`, a hook carrying a non-command handler
/// key, a missing or empty `command`, or a bad `timeout`. A bad `args`, or `async` on an
/// event that does not accept it, drops only that hook.
fn parse_entry(item: &Value, accepts_async: bool) -> ParsedEntry {
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
    let mut dropped = Vec::new();
    for (index, hook) in hooks.iter().enumerate() {
        match parse_command(hook, accepts_async) {
            CommandParse::Ok(command) => commands.push(command),
            CommandParse::DropHook => dropped.push(index),
            CommandParse::Skip => return ParsedEntry::skipped_after(dropped),
        }
    }
    ParsedEntry {
        entry: (!commands.is_empty()).then_some(HookMatcher {
            matcher,
            hooks: commands,
        }),
        dropped,
        structural: false,
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
fn parse_command(hook: &Value, accepts_async: bool) -> CommandParse {
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
                    return CommandParse::DropHook;
                };
                args.push(arg.to_owned());
            }
            Some(args)
        }
        Some(_) => return CommandParse::DropHook,
    };
    let async_rewake = match table.get("asyncRewake") {
        None => false,
        Some(Value::Boolean(on)) => *on,
        Some(_) => return CommandParse::DropHook,
    };
    let async_hook = match table.get("async") {
        None => async_rewake,
        Some(Value::Boolean(on)) => *on || async_rewake,
        Some(_) => return CommandParse::DropHook,
    };
    // Presence, not value: the fields belong to `PostToolUse` and `Stop` alone, so a hook
    // that writes `async = false` on a pre-call event still asked for a field those events
    // do not accept and is dropped like an `async = true` one (ADR 0045 §5).
    if !accepts_async && (table.contains_key("async") || table.contains_key("asyncRewake")) {
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

    /// A bad `args` hook followed by a structurally invalid hook keeps both warnings: the
    /// per-hook `args` line and the entry's own line. Discarding the collected indexes on
    /// the structural skip (the old `ParsedEntry::skipped()`) loses the `args` warning.
    #[test]
    fn a_bad_args_hook_is_still_named_when_a_sibling_skips_the_entry() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[[hooks.PreToolUse]]\n",
            "[[hooks.PreToolUse.hooks]]\n",
            "type = \"command\"\n",
            "command = \"/work/example/hooks/bad\"\n",
            "args = \"x\"\n",
            "[[hooks.PreToolUse.hooks]]\n",
            "type = \"prompt\"\n",
            "command = \"/work/example/hooks/other\"\n",
        ));
        assert!(
            config.hooks.pre_tool_use.is_empty(),
            "the structural defect skips the entry: {:?}",
            config.hooks
        );
        assert_eq!(
            config.skipped,
            vec![
                "hooks.PreToolUse[0]".to_owned(),
                "hooks.PreToolUse[0].hooks[0]".to_owned(),
            ],
            "both the bad-args hook and the entry are named"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse_str(text: &str) -> Config {
        super::super::parse(Path::new("/tmp/test/config.toml"), text).unwrap()
    }

    /// The subset parses: the four deciding events, matchers kept raw, commands with
    /// timeouts carried.
    #[test]
    fn the_four_deciding_events_parse_with_matchers_and_timeouts() {
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

    /// The observe-only events parse with their closed-set matchers, they accept `async`
    /// (inert), and that field never moves their default timeout to 600 s (ADR 0045 §6).
    #[test]
    fn stop_failure_and_notification_parse_and_async_is_inert() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[[hooks.StopFailure]]\n",
            "matcher = \"usage_limit\"\n",
            "[[hooks.StopFailure.hooks]]\n",
            "type = \"command\"\n",
            "command = \"echo stopped\"\n",
            "async = true\n",
            "[[hooks.Notification]]\n",
            "matcher = \"permission_prompt\"\n",
            "[[hooks.Notification.hooks]]\n",
            "type = \"command\"\n",
            "command = \"echo notify\"\n",
        ));
        assert!(config.skipped.is_empty(), "{:?}", config.skipped);
        assert_eq!(config.hooks.stop_failure.len(), 1);
        assert_eq!(
            config.hooks.stop_failure[0].matcher.as_deref(),
            Some("usage_limit")
        );
        let hook = &config.hooks.stop_failure[0].hooks[0];
        assert!(hook.async_hook, "async is accepted and inert");
        assert_eq!(
            hook.observe_timeout(),
            std::time::Duration::from_secs(DEFAULT_HOOK_TIMEOUT_SECS),
            "async never moves an observe hook's default to 600 s"
        );
        assert_eq!(
            hook.timeout(),
            std::time::Duration::from_secs(DEFAULT_ASYNC_HOOK_TIMEOUT_SECS),
            "the async field still has its old meaning for the events that use it"
        );
        assert_eq!(config.hooks.notification.len(), 1);
        assert_eq!(
            config.hooks.notification[0].matcher.as_deref(),
            Some("permission_prompt")
        );
        assert_eq!(
            config.hooks.notification[0].hooks[0].observe_timeout(),
            std::time::Duration::from_secs(DEFAULT_HOOK_TIMEOUT_SECS)
        );
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
            "if = \"true\"\n",
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

    /// The async fields (ADR 0045 §5): `async` and `asyncRewake` parse on `PostToolUse` and
    /// `Stop`, `asyncRewake` implies `async`, and the async default timeout is 600 s. On
    /// `PreToolUse` that hook is dropped with one line while its sibling still loads.
    #[test]
    fn async_hooks_parse_on_post_and_stop_and_drop_on_pre() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[[hooks.PostToolUse]]\n",
            "matcher = \"patch\"\n",
            "[[hooks.PostToolUse.hooks]]\n",
            "type = \"command\"\n",
            "command = \"make lint\"\n",
            "async = true\n",
            "[[hooks.Stop]]\n",
            "[[hooks.Stop.hooks]]\n",
            "type = \"command\"\n",
            "command = \"make verify\"\n",
            "asyncRewake = true\n",
            "[[hooks.PreToolUse]]\n",
            "matcher = \"shell\"\n",
            "[[hooks.PreToolUse.hooks]]\n",
            "type = \"command\"\n",
            "command = \"echo sync\"\n",
            "[[hooks.PreToolUse.hooks]]\n",
            "type = \"command\"\n",
            "command = \"echo async\"\n",
            "async = true\n",
        ));
        assert!(config.hooks.post_tool_use[0].hooks[0].async_hook);
        assert!(!config.hooks.post_tool_use[0].hooks[0].async_rewake);
        assert_eq!(
            config.hooks.post_tool_use[0].hooks[0].timeout().as_secs(),
            DEFAULT_ASYNC_HOOK_TIMEOUT_SECS,
            "an async hook without `timeout` defaults to 600 s, not 60"
        );
        assert!(config.hooks.stop[0].hooks[0].is_async());
        assert!(
            config.hooks.stop[0].hooks[0].async_rewake,
            "asyncRewake implies async"
        );
        assert_eq!(
            config.hooks.pre_tool_use[0].hooks.len(),
            1,
            "the async PreToolUse hook drops, its sibling still loads"
        );
        assert_eq!(config.hooks.pre_tool_use[0].hooks[0].command, "echo sync");
        assert_eq!(
            config.skipped,
            vec!["hooks.PreToolUse[0].hooks[1]".to_owned()],
            "one line names the dropped async hook"
        );
    }

    /// A pre-call hook that writes `async = false` or `asyncRewake = false` still carries a
    /// field those events do not accept: presence, not the effective value, drops it with
    /// one line, and its siblings still load (ADR 0045 §5).
    #[test]
    fn an_explicitly_false_async_field_on_pre_is_still_dropped() {
        let config = parse_str(concat!(
            "schema_version = 1\n",
            "[[hooks.PreToolUse]]\n",
            "[[hooks.PreToolUse.hooks]]\n",
            "type = \"command\"\n",
            "command = \"echo async-off\"\n",
            "async = false\n",
            "[[hooks.PreToolUse.hooks]]\n",
            "type = \"command\"\n",
            "command = \"echo rewake-off\"\n",
            "asyncRewake = false\n",
            "[[hooks.PreToolUse.hooks]]\n",
            "type = \"command\"\n",
            "command = \"echo keep\"\n",
            "[[hooks.PermissionRequest]]\n",
            "[[hooks.PermissionRequest.hooks]]\n",
            "type = \"command\"\n",
            "command = \"echo ask\"\n",
            "async = false\n",
        ));
        assert_eq!(
            config.hooks.pre_tool_use[0].hooks.len(),
            1,
            "the false-valued fields still drop their hooks: {:?}",
            config.hooks.pre_tool_use
        );
        assert_eq!(config.hooks.pre_tool_use[0].hooks[0].command, "echo keep");
        assert!(
            config.hooks.permission_request.is_empty(),
            "PermissionRequest drops it too: {:?}",
            config.hooks.permission_request
        );
        assert_eq!(config.skipped.len(), 3, "one line per dropped hook");
        for key in [
            "hooks.PreToolUse[0].hooks[0]",
            "hooks.PreToolUse[0].hooks[1]",
            "hooks.PermissionRequest[0].hooks[0]",
        ] {
            assert!(
                config.skipped.iter().any(|line| line == key),
                "{key}: {:?}",
                config.skipped
            );
        }
    }
}
