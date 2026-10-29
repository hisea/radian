---
description: Run your own commands when a tool runs, the model stops, a turn fails, or radian needs you.
---

# Hooks

Hooks run your own commands at six moments: before a tool runs, when a
permission rule is about to ask you, after a tool runs, when the model
stops, when a turn ends on a model error, and when radian is waiting for
you. Use them to log, to format, to block, to answer a prompt yourself, to
continue a turn with your own check, or to notify you from a session you are
not watching.

A hook never widens permissions. On `PreToolUse`, a hook that says `allow` or
`ask` is refused with a warning and treated as no decision. The call still
follows the permission rules in `permissions.md`. `PermissionRequest` is the
one event that may say `allow`: it answers a call the rules already judged
`ask`, so it removes a question and never lifts a `deny` rule or the sandbox.

## Events

Six events exist. Their names are case-sensitive.

| Event | When it runs | The matcher |
|---|---|---|
| `PreToolUse` | Before each tool call | Filters by tool name |
| `PermissionRequest` | Before a permission prompt, when the rules say `ask` | Filters by tool name |
| `PostToolUse` | After each tool call | Filters by tool name |
| `Stop` | After a completed turn | Ignored; every entry runs |
| `StopFailure` | After a turn that ends on a model error | Filters by error kind |
| `Notification` | When radian is about to ask you, or a TUI turn sits idle | Filters by notification type |

Any other event name is skipped with one warning. Only a completed turn runs
`Stop`: a cancelled, failed or otherwise unfinished turn runs nothing.
`PermissionRequest` fires after the permission rules and an existing session
grant resolved a call to `ask`, and before anyone is asked. A `deny` rule hit,
an allowed call, and an `auto`-approved call never fire it.

`StopFailure` and `Notification` only observe. They run in the background,
their stdout is not read, and their exit code or timeout only adds one Recent
runs entry: they never block a turn, a prompt or an exit, and never change an
exit code. Their default timeout is 60 seconds; an explicit `timeout` still
wins. `async` on either is accepted and inert: they always run in the
background, so the field changes nothing (the 600-second async default does
not apply either).

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
with handler keys outside the command subset (`if`, `http`, `prompt`,
`agent`), a missing or empty `command`, a bad `timeout`, a non-string matcher,
or a missing or empty `hooks` list is skipped with one warning each. A hook
whose `args` is not an array of strings is one exception: only that hook is
skipped, the warning names it, and the entry's other hooks still load. So is
`async` or `asyncRewake` on `PreToolUse` or `PermissionRequest`: those two
events decide before the call, so the hook is skipped with one warning and
the entry's other hooks still load. A `StopFailure` or `Notification` matcher
outside its closed set warns once and that entry never runs, like an invalid
tool pattern. Nothing outside the subset ever refuses
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

[[hooks.PostToolUse]]
matcher = "write"
[[hooks.PostToolUse.hooks]]
type = "command"
command = "/work/example/hooks/lint-async"
async = true

[[hooks.Stop]]
[[hooks.Stop.hooks]]
type = "command"
command = "notify-send radian stopped"
timeout = 30

[[hooks.StopFailure]]
matcher = "usage_limit"
[[hooks.StopFailure.hooks]]
type = "command"
command = "notify-send radian 'the quota window is used up'"

[[hooks.Notification]]
matcher = "permission_prompt"
[[hooks.Notification.hooks]]
type = "command"
command = "notify-send radian 'waiting for your approval'"
```

Each event holds a list of entries. Each entry holds a `matcher` and a list
of `hooks`. Each hook holds `type = "command"`, a `command`, an optional
`args`, an optional `timeout` in seconds, and the optional `async` and
`asyncRewake` booleans (`false` by default).

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

## Async hooks

`async = true` starts the hook in the background instead of waiting: the tool
result returns at once, and a `Stop` hook does not hold the turn end. Only
`PostToolUse` and `Stop` accept it. `PreToolUse` and `PermissionRequest` run to
decide before the call, so a hook with `async` there is skipped with one
warning and the entry's other hooks still load. `StopFailure` and
`Notification` accept `async` but it is inert: those events always run in the
background and keep their 60-second default. `asyncRewake = true` implies
`async`.

An async hook starts exactly like a synchronous one: same working directory,
same sandbox, same environment rules, same stdin. The default timeout is 600
seconds instead of 60; an explicit `timeout` still wins. It is not part of a
`Stop` continuation chain and does not count toward the consecutive
idle-continuation cap. It is not a job either: it never appears in `/jobs` or
the `job` tool, and the model cannot wait on or kill it. The `/hooks` page
shows running async hooks in a Running section, and `Stop` lists each one in
`background_tasks` with `"type": "hook"`. While one runs, the built-in stop
validator stays off, like a running job.

Async hooks have a lifecycle. Two runs are the same hook when their event,
`command` and `args` match. When that hook fires while an earlier run of it is
still running, radian kills the earlier run's process group: it delivers
nothing, and its Recent runs entry says `cancelled`. A run whose process has
already exited is delivering its result and is not superseded. At most 8 async
hook processes run at once: a run over the cap does not start, shows one
warning line, and its Recent runs entry says `failed to start`. Only running
processes count: a run whose process already exited while its result waits to
be delivered does not hold one of the 8 places. A cancelled turn leaves
running async hooks alone. When the session ends — you quit,
`/new` opens the next session, or a `radian exec` run ends — every running
async hook's process group is killed and nothing more is delivered; `exec -p`
never waits for one. A result whose delivery has already begun when the
session ends finishes before the end returns, and nothing arrives afterwards.

Its result is delivered when the process exits. An async hook cannot change a
call or a turn: a returned `permissionDecision`, `updatedInput`,
`updatedToolOutput` or `decision` is ignored with one warning.

| End | What happens |
|---|---|
| exit 0 with `hookSpecificOutput.additionalContext` | The text enters context as a `System` `inject`: `[<event> hook] <text>`, at most 2000 characters. It waits for the next model call — the running turn's next step, or the next turn — and never wakes an idle session. |
| exit 2 without `asyncRewake` | The stderr text takes the same `inject` path, at most 2000 characters. Empty stderr uses a fallback line. |
| exit 2 with `asyncRewake` | The stderr text takes the `followup` lane: an idle session wakes one turn and the wake counts against the same wake budget as `Stop` continuations. Past the budget it degrades to an `inject` with the same three signals as a degraded background notice. |
| other exit, timeout, or start failure | One warning line and one Recent runs entry. It never fails closed: the action the hook checked already happened. |

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

## `StopFailure`

`StopFailure` runs once per turn that ends `Failed` on a model error: a provider
or transport failure, a protocol error, a used-up plan window, a
provider-recognized context overflow, or an empty response. A hook error or a
compaction error that ends the turn runs nothing, and a journal error ends the
session, so nothing runs; a
cancelled turn runs nothing (those are not model errors; see `cli.md` for the
turn outcomes). It also never runs after a `Completed` turn, even one whose
answer looked wrong.

The `matcher` is the error kind. `*`, a blank string, or a missing matcher
matches every kind.

| Matcher | The failure it names |
|---|---|
| `transport` | A connection, stream or idle-timeout failure (including a cancellation sentinel; a user cancel is a cancelled turn, not this). |
| `protocol` | A bad request or response: 400/401/403/404, a malformed stream. |
| `unavailable` | A 408/429/5xx/529 or an explicit overload the provider may recover from. |
| `usage_limit` | A recognized plan or quota window that is used up. |
| `context_overflow` | A provider-recognized request over its context window. |
| `empty_response` | A final answer with no text and no tool call. |

Any other value is a load-time warning and that entry never runs; `rate_limit`
is one of those (it is Claude's name, not a radian kind). The table above is
also the Claude import mapping for `authentication_failed` → `protocol`,
`billing_error` → `usage_limit`, `invalid_request` → `protocol`,
`rate_limit` → `unavailable`, and `server_error` → `unavailable`; Claude's
`max_output_tokens` and `unknown` have no radian kind, so an entry naming one
is dropped with one report line. `import-claude` reports every translation and
drop.

Its stdin:

```json
{
  "hook_event_name": "StopFailure",
  "cwd": "/path/to/workspace",
  "session_id": "0193…",
  "error": "usage_limit",
  "error_details": "model usage limit reached: …",
  "last_assistant_message": "I was checking the tests when …"
}
```

`error` is one of the matcher values. `error_details` is the error's own text,
cut to 2000 characters. `last_assistant_message` has the same rule as `Stop`:
the text parts of the turn's last assistant message joined together; an empty
string means the turn has none.

The hook cannot block the turn or change its outcome. `radian exec -p` waits at
most 5 seconds after its last turn for a running `StopFailure` hook, then kills
its process group; the run's exit code and output are unchanged. A TUI session
keeps the hook running in the background and kills it when the session ends.

## `Notification`

`Notification` observes two moments. `matcher` is a closed set of two values;
`*`, a blank string, or a missing matcher matches both. Any other value is a
load-time warning and that entry never runs. `import-claude` keeps
`permission_prompt` and `idle_prompt` entries and drops Claude's other
notification types with one report line.

| Matcher | When it runs |
|---|---|
| `permission_prompt` | Each time radian is about to ask a human for approval: the TUI approval modal, or `exec`'s terminal prompt. It runs after the permission rules produced `ask` and after a `PermissionRequest` hook gave no decision. An ask that a hook answered, an allowed call, and an `auto`-approved call never fire it. |
| `idle_prompt` | In the TUI only: once when a turn has ended and 60 seconds pass with no input. Any input restarts the timer. It does not run while `/loop` or an active or paused goal has the next turn scheduled. `radian exec -p` exits at its turn end, so it has no idle state. |

The 60 seconds is a fixed constant; there is no config key. It fires for a
turn of any outcome, including a failed or cancelled one.

Its stdin:

```json
{
  "hook_event_name": "Notification",
  "cwd": "/path/to/workspace",
  "session_id": "0193…",
  "notification_type": "permission_prompt",
  "title": "radian",
  "message": "radian needs your permission to use shell"
}
```

`notification_type` is the value the matcher chose. `title` is always
`radian`. `message` is `radian needs your permission to use <tool name>` for
`permission_prompt` and `radian is waiting for your input` for `idle_prompt`.
It never carries tool arguments, a command, or transcript text: the user's
command decides what leaves the machine (radian sends nothing itself).

The hook cannot delay the prompt, the frame, or an `exec` exit: it starts in
the background and the modal or the idle transition appears at once. Its
output and exit code are ignored (one Recent runs entry only).

## `StopFailure` and `Notification` return nothing

Neither event decides anything. Any stdout is ignored, exit 2 has no meaning,
and any other exit or a timeout only records the Recent runs entry. There is
no JSON to print and no field to set. The only visible effect is the command
the hook itself runs.

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
ignores the matcher. `StopFailure` and `Notification` do not use regexes: their
matchers are the closed sets in their sections above (`*` or blank matches
all).

Type `/hooks` to open the inspector's hooks page. It shows each configured
entry and whether its matcher compiles, a Running section for async hooks
(see [Async hooks](#async-hooks)), and a Recent runs section (see
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
| `background_tasks` | array | `[]` | The background jobs and async hooks running when `Stop` runs. A job entry is `{"id": "job-…", "type": "shell", "status": "running", "description": "make check", "command": "make check\necho done"}`: `description` is the command's first line, `command` the full command. An async hook entry has the same fields with `"type": "hook"`. A host with no job registry still lists running async hooks and reports `[]` when there are none. |
| `session_crons` | array | `[]` | The session-level schedulers that open turns by themselves: `{"id": "loop", "type": "loop"}` while `/loop` is on, and `{"id": "goal", "type": "goal", "status": "active"}` or `{"id": "goal", "type": "goal", "status": "paused"}` while a goal is active or paused. `radian exec` has no `/loop`, so it can carry only the goal entry. |

`stop_hook_active` describes the turn the hook runs for. It is `true` when that
turn claimed a continuation radian sent, and `false` for every other turn. A user
message (`followup`, `steer` or `inject`) drops a continuation that is still
waiting to start, so the turn that later claims it reports `false`; a turn that
already started keeps its value. A turn that ends without completing runs no
`Stop` hook and clears the value.

## What a hook returns

The exit code decides for the four deciding events. `StopFailure` and
`Notification` ignore output entirely: they only observe, and their exit code
or timeout adds one Recent runs entry and nothing else (see their sections).

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
changes nothing. A synchronous hook applies them to the result or the turn as
described below. An async hook ignores every control field with one warning;
only its `additionalContext` reaches context (see [Async hooks](#async-hooks)).

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
| `decision.updatedPermissions` | array | none | The one item `{"type": "addRules", "destination": "session", "behavior": "allow"}` records the same session grant as the approval prompt's `a` ("allow for this session", `keybindings.md`): the next identical call — or, for a `shell` call, one with the same command prefix — neither asks nor runs the hook again, for this session only. The item's `rules` are not read: radian has no text rule syntax. Any other `type`, `destination` or `behavior` warns once and records nothing, and a value that is not an array warns once; the call is still allowed once. |
| `decision.updatedInput` | object | none | Ignored with one warning line. |
| `decision.interrupt` | boolean | none | Ignored with one warning line. |

A decision is one layer's answer. Any layer's `deny` ends the chain; with no
`deny`, one layer's `allow` allows the call; when no layer gives a decision,
the ordinary path follows: the approval prompt on a turn from you, or a denial
when nobody can be asked (a `Stop` continuation turn, a job-notice turn, or
`radian exec` without a terminal). `allow` here answers a question the rules
already asked. It does not lift a `deny` rule, and does not skip the sandbox.

`updatedPermissions` items count only from a layer whose decision is `allow`, and
only when the final decision is `allow`: a later layer's `deny` discards them,
and a layer that gives no decision contributes nothing.

`permissionDecision` applies only to `PreToolUse`; on `PostToolUse` and
`Stop` it is ignored with one warning line, and on `PermissionRequest` it has
no effect (that event reads `decision` instead). `updatedInput` on `Stop` is
ignored with one warning line too.

## Recent runs

Every hook run records one entry, when it finishes or when it never starts:

```
<event>:<tool name> [<first line of the command>] <end> (<n>ms)
```

`Stop`, `StopFailure` and `Notification` have no tool name. The command is the
`command` as written, cut to its first line; with `args` it is the program only.
`<end>` is exactly one of:

| End | Meaning |
|---|---|
| `status <code>` | The process exited with that code. |
| `timed out` | The run passed its `timeout` and its process group was killed. Its milliseconds are at least the timeout. |
| `cancelled` | The run was cancelled: the process ended on a signal (for example `kill`), radian killed its process group because the same hook fired again or the session ended, or radian could not finish waiting on it. |
| `failed to start` | The process never started: the command was not found, the sandbox could not build it, or the session already has 8 async hook processes running. |

Milliseconds are whole milliseconds, rounded up, so a fast hook reports at
least `1ms`.

The session keeps the last 50 finished runs in memory, newest first. The
`/hooks` page lists them in its Recent runs section below the configured
entries, in the same scrolling list: `↑`/`↓` reaches every retained run, and
`⏎` on a run shows its record. Running async hooks sit above that section in
their own Running list, each as `<event>:<tool> [<command first line>] running
(<n>ms)` (or `<event> [<command first line>] running (<n>ms)` for `Stop`).
`StopFailure` and `Notification` runs are not listed as running (they always
run in the background); their finished run still appears in Recent runs.
Each run is also logged with `tracing` at info as it finishes. Runs are not
journaled: they never enter the transcript or the session journal. `radian
exec` has no `/hooks` page, so there it is only the log line.

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
(a slow synchronous command past 60 seconds fails closed; an async one past
600 only warns and records the run, because the action it checked already
happened) and check stderr. A project
file that never loads is in an untrusted directory: trust it first.
