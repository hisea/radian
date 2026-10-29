---
description: Run your own commands when a tool runs or the model stops.
---

# Hooks

Hooks run your own commands at four moments: before a tool runs, when a
permission rule is about to ask you, after a tool runs, and when the model
stops. Use them to log, to format, to block, to answer a prompt yourself, or
to continue a turn with your own check.

A hook never widens permissions. On `PreToolUse`, a hook that says `allow` or
`ask` is refused with a warning and treated as no decision. The call still
follows the permission rules in `permissions.md`. `PermissionRequest` is the
one event that may say `allow`: it answers a call the rules already judged
`ask`, so it removes a question and never lifts a `deny` rule or the sandbox.

## Events

Four events exist. Their names are case-sensitive.

| Event | When it runs | The matcher |
|---|---|---|
| `PreToolUse` | Before each tool call | Filters by tool name |
| `PermissionRequest` | Before a permission prompt, when the rules say `ask` | Filters by tool name |
| `PostToolUse` | After each tool call | Filters by tool name |
| `Stop` | After a completed turn | Ignored; every entry runs |

Any other event name is skipped with one warning. Only a completed turn runs
`Stop`: a cancelled, failed or otherwise unfinished turn runs nothing.
`PermissionRequest` fires after the permission rules and an existing session
grant resolved a call to `ask`, and before anyone is asked. A `deny` rule hit,
an allowed call, and an `auto`-approved call never fire it.

## Where hooks live

Two layers hold hooks. The user layer is the `[hooks]` section of the config
file (`configuration.md` holds the path). The project layer is
`<workspace>/.radian/hooks.json`. The project file loads only when the
directory is trusted, and it accepts two shapes: the bare events object, or
the wrapped settings shape with a top-level `hooks` object. When both are
present the wrapped object wins.

At runtime the project entries run first, then the user entries, so user
hooks see the final arguments. An identical hook — the same `command` and the
same `args` — runs once per event. The same program with different `args` is
two hooks and both run.

`PermissionRequest` decides across both layers: any layer's `deny` ends the
chain, later hooks do not run; with no `deny`, one layer's `allow` is enough;
and when no layer gives a decision the ordinary prompt (or denial) follows.

A missing file is empty, never an error. An unreadable project file warns
once and loads nothing. A hook whose `type` is not `"command"`, an entry
with handler keys outside the command subset (`async`, `if`, `http`,
`prompt`, `agent`), a missing or empty `command`, a bad `timeout`, a
non-string matcher, or a missing or empty `hooks` list is
skipped with one warning each. A hook whose `args` is not an array of strings
is the one exception: only that hook is skipped, the warning names it, and the
entry's other hooks still load. Nothing outside the subset ever refuses
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
command = "/work/example/hooks/lint"
args = ["--check", "src/lib.rs"]
timeout = 30

[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "notify-send radian stopped"
timeout = 30
```

Each event holds a list of entries. Each entry holds a `matcher` and a list
of `hooks`. Each hook holds `type = "command"`, a `command`, an optional
`args`, and an optional `timeout` in seconds.

Without `args`, `command` is a shell command string and runs through
`/bin/sh -c`. With `args`, `command` names an executable — an absolute or
relative path, or a name found on `PATH` — and radian starts it directly with
exactly those arguments, never through a shell. Nothing splits on spaces and
`$`, `;`, `*` and quotes stay literal, so each argument arrives as written.
`args` must be an array of strings; an empty array runs the executable with no
arguments. A hook whose `args` is not an array of strings is skipped with one
warning, and the entry's other hooks still load.

The default timeout is 60 seconds; zero means the default. A second
`[[...hooks]]` table under one entry runs a second command.

The project file spells the same shape in JSON:

```json
{
  "PreToolUse": [
    {"matcher": "shell", "hooks": [{"type": "command", "command": "make lint", "timeout": 10}]},
    {"matcher": "patch", "hooks": [{"type": "command", "command": "/work/example/hooks/lint", "args": ["--check", "src/lib.rs"]}]}
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

A `Stop` continuation that keeps reopening turns where the model only talks
is capped too. Radian counts consecutive hook continuations (exit 2,
`decision: "block"`, or `additionalContext`) whose turn ended `Completed`
with no tool call. At 8 in a row the continuation no longer opens a turn: its
message waits visibly instead, the same degraded `inject` as a used-up wake
budget. One notice line starts with `hook stop:` and says to send any
message; herdr reports `blocked` with the category `hook stop`, never the
hook's text. The label clears as soon as you send any message, including a `!`
command that starts no turn. A continued turn that calls a tool resets the count,
and so does a cancelled or otherwise unfinished turn and any message from you.
The 10-continuations-per-10-minutes wake budget still applies as a hard cap, and
the built-in validator's own two-in-a-row rule stays separate from this count.
In `radian exec -p` the cap ends the run like a used-up wake budget: exit `3`,
outcome `budget_exceeded`, `stop_reason` `wake_budget`, and the waiting verdict
counted in `unhandled_stop_notices` (see [`cli`](cli.md)).

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
entry and whether its matcher compiles, and a Recent runs section (see
[Recent runs](#recent-runs)).

## What a hook receives

The hook command runs with the workspace as its working directory, inside the
session sandbox when one is enforced. Without `args` it runs as `/bin/sh -c`;
with `args` radian starts `command` directly with those arguments and no shell.
Outside the sandbox it inherits radian's environment; inside it sees only the
sandbox's allowed variables. Its stdin is one
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

`PermissionRequest` receives the `PreToolUse` fields: the event name, the tool
name, the tool input, the working directory, and the session id. It has no
`permission_suggestions` field: radian's permission rules are types, not text,
so there is nothing to suggest.

```json
{
  "hook_event_name": "PermissionRequest",
  "tool_name": "shell",
  "tool_input": {"command": "git push"},
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
  "last_assistant_message": "Done. …",
  "stop_hook_active": false,
  "background_tasks": [],
  "session_crons": []
}
```

`Stop` always carries three more fields, present in every run:

| Field | Type | Value when it does not apply | When it is filled |
|---|---|---|---|
| `stop_hook_active` | boolean | `false` | `true` only on the turn that claims a `Stop` continuation: a hook exit 2, a `Stop` hook's exit-0 `decision: "block"` or `additionalContext`, or the built-in stop validator. |
| `background_tasks` | array | `[]` | The background jobs running when `Stop` runs. An entry is `{"id": "job-…", "type": "shell", "status": "running", "description": "make check", "command": "make check\necho done"}`: `description` is the command's first line, `command` the full command. A host with no job registry reports `[]`. |
| `session_crons` | array | `[]` | The session-level schedulers that open turns by themselves: `{"id": "loop", "type": "loop"}` while `/loop` is on, and `{"id": "goal", "type": "goal", "status": "active"}` or `{"id": "goal", "type": "goal", "status": "paused"}` while a goal is active or paused. `radian exec` has no `/loop`, so it can carry only the goal entry. |

`stop_hook_active` describes the turn the hook runs for. It is `true` when that
turn claimed a continuation radian sent, and `false` for every other turn. A user
message (`followup`, `steer` or `inject`) drops a continuation that is still
waiting to start, so the turn that later claims it reports `false`; a turn that
already started keeps its value. A turn that ends without completing runs no
`Stop` hook and clears the value.

## What a hook returns

The exit code decides.

| Exit code | `PreToolUse` | `PermissionRequest` | `PostToolUse` | `Stop` |
|---|---|---|---|---|
| `0` | Continue. Stdout may carry JSON (below); empty stdout continues silently, and stdout that is not JSON warns and continues. | Decide. Stdout may carry a decision (below); empty stdout means no decision. Stdout that is not JSON is a hook error. | Continue. Stdout may carry JSON (below). | Continue. Stdout may carry JSON (below). |
| `2` | Deny: stderr (at most 2000 characters) becomes the reason. Empty stderr denies with a default message. | Deny: stderr (at most 2000 characters) becomes the denied result's reason. Empty stderr denies with a default message. | The tool already ran, so there is nothing to deny: stderr is fed back to the model with the result, and the result is marked as an error. | Ask for more: the reasons join into one message that opens another turn. Past the wake budget, or past the consecutive idle-continuation cap, the turn still opens its message but does not wake: it waits visibly with the matching notice instead. |
| other, timeout, or signal | Hook error: the call is denied and the turn ends. | Hook error: the call is denied with the reason code `hook_error`, and no prompt is shown. | Hook error: the turn ends. | The turn ends with a warning. |

A `PermissionRequest` `deny`, or a hook error, is never a fallback to asking you:
an answer that did not arrive is a denial.

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

`PostToolUse` and `Stop` also read these fields from their exit-0 JSON
output. Empty stdout does nothing, and stdout that is not JSON warns once and
changes nothing.

`hookSpecificOutput.additionalContext` (string) adds feedback without
failing. On `PostToolUse` it appends `[PostToolUse hook] <text>` to the result
and does not mark it an error. On `Stop` it opens another turn through the
same path as exit 2: it joins every other hook's exit 2 and the built-in
validator in one message, counts one wake, and warns nothing.

```json
{"hookSpecificOutput": {"additionalContext": "formatted 3 files"}}
```

`decision: "block"` with `reason` (string) is the JSON form of exit 2:

```json
{"decision": "block", "reason": "the formatter failed"}
```

On `PostToolUse` it appends the reason and marks the result an error; on
`Stop` it asks for another turn. When both `decision: "block"` and
`additionalContext` are given, both texts enter the result or message, block
first.

`hookSpecificOutput.updatedToolOutput` (string, `PostToolUse` only) replaces
the result's content with one text part. `is_error` and `error_code` do not
change:

```json
{"hookSpecificOutput": {"updatedToolOutput": "the reformatted file"}}
```

The project layer replaces first, so a user-layer hook sees the replaced
result in its `tool_response`. The replacement, not the tool's original text,
is what the transcript and the journal record. A value that is not a string
is a hook error.

These three fields, like stderr, keep at most 2000 characters; longer text
ends with `…[truncated]`.

### `PermissionRequest`

`PermissionRequest` is the one event whose exit-0 JSON decides the call. Print
one object with a `decision`:

```json
{"hookSpecificOutput": {"decision": {"behavior": "deny", "message": "no network writes"}}}
```

| Field | Type | Default | What it does |
|---|---|---|---|
| `decision.behavior` | string | none | `allow` or `deny`. Any other value, or a missing value, warns once and means no decision. |
| `decision.message` | string | none | On `deny`, the text of the denied tool result (its error code is `denied`); at most 2000 characters. On `allow` it is ignored. Without it the denial carries a default message. |
| `decision.updatedPermissions` | array | none | Ignored in this version. |
| `decision.updatedInput` | object | none | Ignored with one warning line. |
| `decision.interrupt` | boolean | none | Ignored with one warning line. |

A decision is one layer's answer. Any layer's `deny` ends the chain; with no
`deny`, one layer's `allow` allows the call; when no layer gives a decision,
the ordinary path follows: the approval prompt on a turn from you, or a denial
when nobody can be asked (a `Stop` continuation turn, a job-notice turn, or
`radian exec` without a terminal). `allow` here answers a question the rules
already asked. It does not lift a `deny` rule, and does not skip the sandbox.

`permissionDecision` applies only to `PreToolUse`; on `PostToolUse` and
`Stop` it is ignored with one warning line, and on `PermissionRequest` it has
no effect (that event reads `decision` instead). `updatedInput` on `Stop` is
ignored with one warning line too.

## Recent runs

Every hook run records one entry when it finishes:

```
<event>:<tool name> [<first line of the command>] <end> (<n>ms)
```

`Stop` has no tool name. The command is the `command` as written, cut to its
first line; with `args` it is the program only. `<end>` is exactly one of:

| End | Meaning |
|---|---|
| `status <code>` | The process exited with that code. |
| `timed out` | The run passed its `timeout` and its process group was killed. Its milliseconds are at least the timeout. |
| `cancelled` | The run was cancelled: the process ended on a signal (for example `kill`), or radian could not finish waiting on it. |
| `failed to start` | The process never started: the command was not found, or the sandbox could not build it. |

Milliseconds are whole milliseconds, rounded up, so a fast hook reports at
least `1ms`.

The session keeps the last 50 runs in memory, newest first. The `/hooks`
page lists them in its Recent runs section below the configured entries, in the
same scrolling list: `↑`/`↓` reaches every retained run, and `⏎` on a run shows
its record. Each run is also logged with `tracing` at info as it finishes. Runs
are not journaled: they never enter the transcript or the session journal.
`radian exec` has no `/hooks` page, so there it is only the log line.

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

Type `/hooks` to see what loaded and the session's recent runs. The startup
line names every skipped entry and every failed run. The Recent runs section
names the command's first line and how each run ended, with its duration, so a
slow or failing hook is visible without guessing. A hook that never fires
usually has a matcher that
does not compile (the hooks page says so) or a tool name that is not a
Radian tool name (MCP tools carry their `server__` prefix). A hook that
denies everything usually exits non-zero by accident: check the timeout
(a slow command past 60 seconds fails closed) and check stderr. A project
file that never loads is in an untrusted directory: trust it first.
