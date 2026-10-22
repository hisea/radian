---
description: Run your own commands when a tool runs or the model stops.
---

# Hooks

Hooks run your own commands at three moments: before a tool runs, after a
tool runs, and when the model stops. Use them to log, to format, to block, or
to continue a turn with your own check.

A hook never widens permissions. A hook that says `allow` or `ask` is
refused with a warning and treated as no decision. The call still follows
the permission rules in `permissions.md`.

## Events

Three events exist. Their names are case-sensitive.

| Event | When it runs | The matcher |
|---|---|---|
| `PreToolUse` | Before each tool call | Filters by tool name |
| `PostToolUse` | After each tool call | Filters by tool name |
| `Stop` | After a completed turn | Ignored; every entry runs |

Any other event name is skipped with one warning. Only a completed turn runs
`Stop`: a cancelled, failed or otherwise unfinished turn runs nothing.

## Where hooks live

Two layers hold hooks. The user layer is the `[hooks]` section of the config
file (`configuration.md` holds the path). The project layer is
`<workspace>/.radian/hooks.json`. The project file loads only when the
directory is trusted, and it accepts two shapes: the bare events object, or
the wrapped settings shape with a top-level `hooks` object. When both are
present the wrapped object wins.

At runtime the project entries run first, then the user entries, so user
hooks see the final arguments. An identical command string runs once per
event.

A missing file is empty, never an error. An unreadable project file warns
once and loads nothing. A hook whose `type` is not `"command"`, an entry
with handler keys outside the command subset (`async`, `if`, `http`,
`prompt`, `agent`), a missing or empty `command`, a bad `timeout`, a
non-string matcher, or a missing or empty `hooks` list is
skipped with one warning each. Nothing outside the subset ever refuses
startup.

## The user config shape

```toml
[[hooks.PreToolUse]]
matcher = "shell|write"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "echo tool call starting >> /tmp/radian-hooks.log"
timeout = 10

[[hooks.PostToolUse]]
matcher = "patch"
[[hooks.PostToolUse.hooks]]
type = "command"
command = "make fmt"

[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "notify-send radian stopped"
timeout = 30
```

Each event holds a list of entries. Each entry holds a `matcher` and a list
of `hooks`. Each hook holds `type = "command"`, a `command`, and an optional
`timeout` in seconds. The default timeout is 60 seconds; zero means the
default. A second `[[...hooks]]` table under one entry runs a second
command.

The project file spells the same shape in JSON:

```json
{
  "PreToolUse": [
    {"matcher": "shell", "hooks": [{"type": "command", "command": "make lint", "timeout": 10}]}
  ],
  "Stop": [
    {"hooks": [{"type": "command", "command": "notify-send radian stopped"}]}
  ]
}
```

Set `[hooks] intent_stop = false` to turn off the built-in stop validator
that also runs on the `Stop` path. It defaults to on. It runs even with no
hooks configured. It stays quiet while a goal or `/loop` owns the turn end
or while any background job runs. After two consecutive continuations it
stops opening turns and waits visibly instead; your next message clears it.

## The matcher

The matcher filters by Radian tool name: `shell`, `write`, `patch`,
`read`, `grep`, `glob`, `job`, `memory_search`, `ask_user`, `skill`,
`subagent`, and the MCP tools as `server__tool`. The pattern is
case-sensitive. `*`, a blank string, or a missing matcher matches every
tool. Any other value is a regular expression anchored to the whole tool
name, so `shell|write` matches the `shell` and `write` tools, and `sh` does
not match `shell`. A pattern that does not compile matches nothing. `Stop`
ignores the matcher.

Type `/hooks` to open the inspector's hooks page. It shows each configured
entry and whether its matcher compiles.

## What a hook receives

The hook command runs as `/bin/sh -c` with the workspace as its working
directory, inside the session sandbox when one is enforced. Outside the
sandbox it inherits radian's environment; inside it sees only the sandbox's
allowed variables. Its stdin is one
JSON object. The fields depend on the event.

`PreToolUse` receives the event name, the tool name, the tool input, the
working directory, and the session id:

```json
{
  "hook_event_name": "PreToolUse",
  "tool_name": "shell",
  "tool_input": {"command": "make check"},
  "cwd": "/path/to/workspace",
  "session_id": "0193…"
}
```

`PostToolUse` receives the same fields plus the tool result:

```json
{
  "hook_event_name": "PostToolUse",
  "tool_name": "shell",
  "tool_input": {"command": "make check"},
  "tool_response": {"content": "…", "is_error": false},
  "cwd": "/path/to/workspace",
  "session_id": "0193…"
}
```

`Stop` receives the event name, the working directory, the session id, and
the last assistant message's text:

```json
{
  "hook_event_name": "Stop",
  "cwd": "/path/to/workspace",
  "session_id": "0193…",
  "last_assistant_message": "Done. …"
}
```

## What a hook returns

The exit code decides.

| Exit code | `PreToolUse` | `PostToolUse` | `Stop` |
|---|---|---|---|
| `0` | Continue. Stdout may carry JSON (below); empty stdout continues silently, and stdout that is not JSON warns and continues. | Continue. | Continue. |
| `2` | Deny: stderr (at most 2000 characters) becomes the reason. Empty stderr denies with a default message. | The tool already ran, so there is nothing to deny: stderr is fed back to the model with the result, and the result is marked as an error. | Ask for more: the reasons join into one message that opens another turn. Past the wake budget the turn still opens its message but does not wake: it waits visibly with a wake-budget notice instead. |
| other, timeout, or signal | Hook error: the call is denied and the turn ends. | Hook error: the turn ends. | The turn ends with a warning. |

An exit-0 hook may print one JSON object to stdout:

```json
{"hookSpecificOutput": {"permissionDecision": "deny", "reason": "no network writes"}}
```

`permissionDecision` accepts only `deny`. `allow` and `ask` are refused with
a warning. An unknown value is ignored with a warning. Either way the rest
of the object still applies. The deny reason is `hookSpecificOutput.reason`,
or a top-level `reason` as fallback; an empty or missing both denies with a
default message.

`updatedInput` replaces the tool input for a `PreToolUse` hook. It must be
an object; anything else is a hook error. The replacement is re-validated
against the permission rules, and later hooks see the replaced input:

```json
{"hookSpecificOutput": {"updatedInput": {"command": "echo safe"}}}
```

## Worked examples

Block a command. Deny every `shell` call whose input mentions a protected
host by exiting 2 with a reason on stderr:

```sh
#!/bin/sh
# hooks/block-hosts.sh
input=$(cat)
case "$input" in
  *protected-host*) echo "protected host is off limits" >&2; exit 2;;
esac
exit 0
```

```toml
[[hooks.PreToolUse]]
matcher = "shell"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "sh /tmp/block-hosts.sh"
```

Format after an edit. Run the formatter after every `patch` and let a
failure ride back to the model with the result:

```toml
[[hooks.PostToolUse]]
matcher = "patch"
[[hooks.PostToolUse.hooks]]
type = "command"
command = "make fmt"
```

Notify on stop. Run one notifier when the turn completes:

```toml
[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "notify-send radian stopped"
```

## Debugging

Type `/hooks` to see what loaded. The startup line names every skipped entry
and every failed run. A hook that never fires usually has a matcher that
does not compile (the hooks page says so) or a tool name that is not a
Radian tool name (MCP tools carry their `server__` prefix). A hook that
denies everything usually exits non-zero by accident: check the timeout
(a slow command past 60 seconds fails closed) and check stderr. A project
file that never loads is in an untrusted directory: trust it first.
