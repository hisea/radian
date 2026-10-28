---
description: arc's worker: the session, the brief, the steps before an attempt, the limits, the checks after it exits, and another agent as the worker.
---

# arc: the worker

Each attempt of a task runs one worker in the worktree arc made for the task.
This page covers that worker: the session it runs in, the brief it receives, the steps before an attempt, the limits the wrapper enforces, the checks after it exits, and how to run another agent in its place.
The command that starts it, its arguments and its exit codes are in [`radian arc start`](index.md#radian-arc-start-task); the paths it writes are in [`arc/files`](files.md).

## The session and the brief

The worker runs in a session of its own: with [`worker.rework_session`](files.md#configtoml) at `fresh`, which is the default, each attempt gets the session `<task>-<attempt>`.
With `resume` the task has one session instead, the task id, and every attempt after the first continues it.
The claim records the attempt's session id in its `start.json` before the worker starts, and the wrapper records the same id in its `identity.json` and `execution.json`; `show` prints it as `session_id` from the first of those records that names one, so a running attempt's session is known from the claim on.
A fresh attempt after the first cannot see the earlier conversation, so the brief it receives opens with a hand-off section, `## Where the previous attempt left off`: the newest earlier attempt's final report (cut at 8,000 characters, with a note when it is cut; a `reverify` attempt runs no worker and writes none, so the section walks back to the attempt before it and names it), the `git log --oneline` of the task's own commits, and the `git diff --stat` of the task's own change, both from the merge base of the remote main and the branch tip.
The section says the full history is on the branch and to read the files rather than assume.
The brief follows it, and a `merge-main` conflict section still comes first.
When [`worker-brief.md`](files.md#worker-briefmd) exists, its content is the last section of every brief, so the order is: the `merge-main` conflict section, the hand-off, the task's brief, then the standing worker instructions.

## The radian worker

By default an attempt runs the radian worker: [`radian acp`](../acp.md), the same coding agent `radian exec` serves headless, spoken to over the [Agent Client Protocol](https://agentclientprotocol.com).
The wrapper is its ACP client and passes the flags the `radian exec` worker used to get, without `-p -`, `--output jsonl` and `--yes`:

```text
radian acp --session-id <session> --workspace <worktree> --profile workspace --deny-read <path>... <exec_args>
```

with `--resume <session>` in place of `--session-id <session>` on a `resume`-mode attempt whose session file exists.
The task's worktree is the session request's `cwd`, so `--workspace` and the cwd name the same directory.
The worktree is the workspace the agent works in, and the `--deny-read` paths are every entry of `.radian/arc/` except `worktrees/`, so the worker cannot read the backlog, the ledger or another attempt's records.
[`worker.exec_args`](files.md#configtoml) and [`flows.<name>.exec_args`](files.md#configtoml) are the extra arguments after those, exactly as they used to reach `radian exec`. Nothing in `config.toml` changes for this: the same keys name the same worker, and `flows.<name>.acp` still names some other agent.

The brief arrives as the single content block of the attempt's one `session/prompt`.
There is no second prompt, and the attempt ends when the turn ends.
A `rework_session = "resume"` attempt opens the task's session with `session/load`, which replays the journal and continues it; every other attempt starts one with `session/new` (see [the session](#the-session-and-the-brief)).

Approvals and `ask_user` questions arrive as `session/request_permission`, and the wrapper answers them with the ACP default policy described under [An ACP agent as the worker](#an-acp-agent-as-the-worker): `allow_once` when every path the request names is inside the worktree, `reject_once` when one is outside it. `--yes` is not passed, so a tool that asks is answered by the wrapper instead of being pre-approved.
The shell still runs under the `workspace` tier: the policy answers the question, the sandbox decides what the command can do.

The attempt's `Result` comes from the response's `_meta.radian`: `text` is the last assistant message in full, and `cost` is the fee of the turn that answered the prompt, in the `{micros, incomplete, estimated}` shape `radian exec` writes, or `null` when no call of that turn had a price.
The wrapper writes that line itself, as for any ACP worker, and the cost, budget, delivery and push rules are [the ones below](#what-the-wrapper-checks-after-the-worker-exits) and in [the result table](gate.md#failures-retries-and-the-budget).
`radian exec` itself remains the headless command ([`radian exec`](../cli.md#radian-exec-the-headless-command)) and the gate's review session ([`gate`](gate.md)); it is no longer the worker.

## The steps before an attempt

Before the worker starts, the wrapper runs the profile's `before_attempt` entries in the worktree, in the order listed (see the [`before_attempt` row](files.md#configtoml)).
An entry is `merge-main` or a table `{ run = "<command line>", timeout_secs = <integer> }`.
`merge-main` fetches the remote main and merges it with `git merge --no-ff --no-edit` unless it is already an ancestor; it does nothing on a first attempt, whose worktree already starts from the fetched main.
A conflict leaves the worktree mid-merge and the worker's brief opens with the conflicted paths (see [A conflict with main](gate.md#a-conflict-with-main)).
A failed fetch or merge ends the attempt with git's message.

A command entry runs in the worktree with no sandbox before every attempt, the first included, after any entry listed before it:

```toml
before_attempt = [{ run = "cargo build --locked", timeout_secs = 1200 }]
```

It is `sh -c <run>`, gets the wrapper's environment plus [`ARC_WORKTREE`, `ARC_TASK` and `ARC_ATTEMPT`](#the-environment), and its stdout and stderr go to `runs/<task>/attempts/<attempt id>/before/<n>.log`, where `n` is the entry's position in the list from 1.
Its output never reaches the worker's stdin: a worker that needs it reads the log or the files the command produced.
`timeout_secs` defaults to [`gate.command_timeout_secs`](files.md#configtoml), and `0` means no limit; the timeout kills the command's whole process group.
The time counts against the attempt's `max_wall_time_secs`.
A command that exits non-zero, or runs out of time, ends the attempt before the worker starts: it is an infrastructure failure under the retry rules, with no worker and no rework count, `attempt_finished.json` names the entry, its exit code or timeout and the log path, and [`show`](index.md#radian-arc-show-id-json) prints that reason.
A `reverify` attempt runs no worker and no `before_attempt` command.

The wrapper records the repository's git control files before the first entry and compares them after the entries, before the worker starts, so a command that changes one fails the attempt the same way a worker does (see [what the wrapper checks](#what-the-wrapper-checks-after-the-worker-exits)).

## The limits

The wrapper enforces the attempt's limits itself, so they hold even after the process that started the wrapper is gone:

- no new line in `runs/<task>/attempts/<attempt id>/result.jsonl` for the task's `stall_timeout_secs` (else `[limits] stall_timeout_secs`) sends SIGTERM to the worker's process group;
- `max_wall_time_secs` (else `[limits] max_wall_time_secs`) since the wrapper started does the same; this clock also bounds the wrapper's own git steps, so a fetch or a push still running at the limit is stopped the same way.
- a fetch, a push or an `ls-remote` is also bounded by [`limits.git_remote_timeout_secs`](files.md#configtoml); whichever of it and the attempt's remaining wall time is shorter applies, and the call, with everything it started, is killed when it runs out.
- after `[limits] cancel_grace_secs` the worker gets SIGKILL; a `before_attempt` command's group gets the same when a stop reaches it while it runs (see [The steps before an attempt](#the-steps-before-an-attempt)).

A limit of `0` means no limit for that timer.
The wrapper records the class in `runs/<task>/attempts/<attempt id>/signal.json` (`stall` or `wall_time`), stops the worker (every ACP worker, the radian worker included, gets `session/cancel` first; see [An ACP agent as the worker](#an-acp-agent-as-the-worker)), and still writes its `AttemptFinished` record.
On a cancel the wrapper is itself killed only after the grace and one more second, the window in which it reaps what it stopped and writes that record; the grace is never shortened to make room for the record.
`start` waits for the wrapper and applies the result; `radian arc run` does not wait (see `run`).

## What the wrapper checks after the worker exits

`Result.outcome` `completed` means that the run ended, not that the task is done (see the [result table](gate.md#failures-retries-and-the-budget)); the wrapper checks delivery itself before it pushes `work/<task>`, and it checks the repository's git control files before it runs any git command of its own after the worker. It requires that:

- `work/<task>` exists, `head_sha` descends from `base_sha` and from the tip the attempt started on, and the worktree HEAD is on the branch;
- the worktree has no uncommitted and no untracked files, unless the task's flow sets [`commit_dirty`](files.md#configtoml) `true` and the outcome is `completed`: the wrapper then commits them first (below);
- the task's own change is not empty, unless [`allow_noop`](files.md#task-fields) is on: the task's own value when its file carries one, else its flow's `flows.<name>.allow_noop` (see [An empty change](gate.md#an-empty-change));
- the task's own change touches no path under `.radian/arc/`;
- the repository's git control files are as the wrapper recorded them before the first `before_attempt` entry ran: `<common dir>/config`, `<git dir>/config.worktree` (the worktree's own git dir), `<common dir>/config.worktree`, every file under `<common dir>/hooks/` and every file under `<common dir>/info/` except `<common dir>/info/refs` (an automatic `git gc` rewrites it through `git update-server-info`, and only the dumb HTTP transport reads it), and every `config` file and every file under `hooks/` in `<common dir>/modules/**` (submodules). A file the worker creates, removes or changes counts as a change: the wrapper compares content, not time. The record stays in the wrapper's memory, never in the worktree, and the check does not depend on the worker's sandbox. A control file larger than 1 MiB is not read whole: an attempt does not start while one of those paths is that large, and one the worker creates fails the attempt and is removed.

The wrapper compares the control files once after the `before_attempt` entries and before the worker starts, and again when the worker exits. A `before_attempt` command that changes one therefore fails the attempt before the worker starts, with the same restore and reason as a worker that changes one.

A task whose flow sets [`flows.<name>.commit_dirty`](files.md#configtoml) `true` lets the wrapper commit a dirty worktree for a `completed` worker, after the control-file check and before the delivery checks. It does not run for a failed or cancelled worker, and a changed control file still fails the attempt with nothing committed. It applies to every worker kind: the radian worker, a `command`, and any worker this build gains later. The commit's identity is the one arc's `merge-main` merge commit uses, `arc <arc@localhost>`; its subject is the first line of the worker's `Result.text` (or `<task id>: worker changes`), and the attempt's notes carry `committed dirty worktree: <short sha>`, which `radian arc show` prints. The key is read at the claim, so a change takes effect at the next claim.

A failed check fails the attempt with its reason in `runs/<task>/attempts/<attempt id>/attempt_finished.json`, which [`show`](index.md#radian-arc-show-id-json) prints; nothing is pushed. The git control check also restores every recorded file to its recorded content and removes every file that was not there before, then fails the attempt with the reason `the worker changed git control files: <paths, comma-separated>`. A control file the wrapper cannot read back counts as a change and is restored the same way. When the restore itself fails, the reason carries both failures, and the wrapper runs no further git command for that attempt. The wrapper stops everything the worker left in its process group before it compares the files, so a background process the worker started cannot change one after the worker exits.

## Another agent as the worker

`[worker] command` replaces the radian worker with one program of your choice for every attempt of every task ([`worker.command`](files.md#configtoml)).
A task can pick a different worker with its `flow`: a `[flows.<name>]` section can set `command`, `acp` for an ACP agent, or `exec_args` for the radian worker, and each task names the flow it runs ([the flow rows](files.md#configtoml)).
A flow sets one worker line: `acp`, `command` or a non-empty `exec_args`, never more than one.
A repository can then run, say, image tasks through a Codex script and code tasks through the radian worker in the same plan:

```toml
[flows.codex]
command = ["/absolute/path/to/codex-worker.sh"]

[flows.default]
exec_args = ["--model", "some-model", "--system-file", ".radian/arc/code.md"]
```

A task with `flow = "codex"` starts the script; a task with no `flow` uses `default`, here the radian worker with those arguments.
Without a `[flows.default]` section, `default` comes from the `[worker]` keys ([the flow rows](files.md#configtoml)).
`[worker] rework_session` is a `[worker]` key, not a flow key, and it applies to every flow.
The rest of this section describes the command worker; it applies to a flow's `command` exactly as it did to `[worker] command`. [An ACP agent as the worker](#an-acp-agent-as-the-worker) below describes `acp`.
The array's first element is the program and the rest are its arguments; there is no shell in between, so a pipeline or a variable belongs in a script you pass as the program.
Unset, a flow's worker is the radian worker and this section does not apply to it.

The program runs in the task's worktree, gets the same brief as the radian worker receives (the conflict section, the hand-off, the task body, then [`worker-brief.md`](files.md#worker-briefmd)), and its stdout goes to `runs/<task>/attempts/<attempt id>/result.jsonl` and its stderr to `stderr.log`.
The arc TUI's conversation tab shows those rows while the attempt runs, under the program's name (see [the task page's conversation tab](tui.md#the-task-page)).
The wrapper gives it its own process group and enforces the attempt's limits itself, exactly as it does for the radian worker: no new stdout line for [`stall_timeout_secs`](files.md#task-fields), `max_wall_time_secs` in total, the cancel grace period after SIGTERM.
After it exits the wrapper runs the same git control-file check, [delivery check](#what-the-wrapper-checks-after-the-worker-exits), push and records; nothing about those changes. A flow with [`commit_dirty`](files.md#configtoml) `true` lets the wrapper commit the command's dirty worktree when its outcome is `completed`.

### The environment

The wrapper adds these variables to the command's or a flow's ACP agent's environment; they describe the attempt and are never read by radian itself. The radian worker gets the wrapper's own environment and its flags instead.

| Variable | Value |
|---|---|
| `ARC_WORKTREE` | The attempt's worktree, absolute. The program's working directory. |
| `ARC_TASK` | The task id, such as `TK0001-work`. |
| `ARC_ATTEMPT` | The attempt id, `<task>-<n>`. |
| `ARC_SESSION` | The session id the claim chose (`<task>-<attempt>` with `rework_session = "fresh"`, the task id with `resume`). |
| `ARC_SESSION_START` | `new` or `resume`: `resume` on a `resume`-mode attempt whose session file exists, `new` on every other attempt. Always set. |
| `ARC_GIT_DIR` | `git rev-parse --absolute-git-dir` in the worktree. |
| `ARC_GIT_COMMON_DIR` | The absolute common git directory. A linked worktree's objects and refs live there. A sandboxed agent that commits from inside its own sandbox needs it writable; Codex's `workspace-write` keeps `.git` read-only even when it is given, so the example below commits outside the sandbox instead. |
| `ARC_DENY_READ` | The paths the wrapper passes to the radian worker as `--deny-read`, joined with `:`. A hint for your own sandbox, not a rule radian enforces on the command. |
| `ARC_STATE_DIR` | An absolute directory outside the worktree, `attempts/<task>/state/`, kept across the task's attempts. The command or ACP agent may keep its own state there, for example a session map. |

### The stdout contract

The program writes JSON lines to stdout.
The last line whose `type` is `Result` decides the attempt, exactly as it does for the radian worker:

- `outcome` `"completed"` passes, and any other value fails the attempt with that outcome;
- no `Result` line at all fails it as `terminated`;
- `text` (a string) is the attempt's final report; the next fresh attempt's hand-off section reads it;
- `cost` is optional and has the shape `radian exec` writes (`{"micros": <integer>, "incomplete": <boolean>, "estimated": <boolean>}`). Without it the attempt's cost is unknown: it adds nothing to the task's or the plan's cost total, a budget never stops on it, and `show`, `list` and the arc TUI show it as unknown — and it stays unknown for the task after a later attempt reports a cost, because a number would leave this attempt out (see [`cost_unknown`](files.md#task-fields)). A `cost` that is `null`, or that carries no `micros`, is a reported zero, the shape `radian exec` writes when it recorded no fee. When a claim starts a command worker for a task or plan that has a cost limit, `start`, `run` and `serve` print one line for the attempt: `<task>: worker.command reports no cost; the cost limit does not apply`.

The command writes no `Result` line on a crash, and the attempt then fails as `terminated`, as a crashed radian worker does.

### What the command must do itself

arc checks the result, not the work: the program must commit its change on the branch, leave the worktree clean, and print the `Result` line. A flow with [`commit_dirty`](files.md#configtoml) `true` takes the commit off its hands for a `completed` outcome: the wrapper commits the worktree after the program exits.
A dirty or untracked file (without `commit_dirty`), no commit past the base, a commit that touches `.radian/arc/`, or a change to the repository's git control files fails the attempt exactly as it fails the radian worker.

### The differences from the radian worker

- **`--deny-read` does not reach it.** radian does not sandbox the command, so what it reads is up to its own sandbox: the arc store and `~/.radian/credentials` are readable unless the command's own sandbox denies them. `ARC_DENY_READ` carries the paths the wrapper would deny so your program can deny them itself.
- **Its sandbox is its own.** radian cannot select a profile or a tier for it. A sandboxed agent needs `ARC_WORKTREE` writable. One that commits from inside its sandbox also needs `ARC_GIT_COMMON_DIR` writable; Codex's `workspace-write` does not allow that, so the example below has Codex leave the commit to the script.
- **The cost limit does not apply.** A budget can never stop a command worker, because arc does not know its cost unless the command reports one. The one warning line above says so once per attempt.

### A complete Codex example

The commit-outside-the-sandbox flow in this example was checked against `codex-cli 0.159.1` in a real task run.
It passes Codex's JSON events to stdout unchanged and then prints one `Result` line: `completed` when `codex exec` and the script's commit both succeed, `error` otherwise, with `text` the content of the `-o` file, JSON-escaped by `jq -Rs`.

Codex's `workspace-write` sandbox keeps the repository's `.git` read-only, even when `--add-dir "$ARC_GIT_COMMON_DIR"` is given, so `git commit` inside Codex fails on `.git/worktrees/<task>/index.lock` with `Operation not permitted`.
The script therefore tells Codex not to commit and commits outside the sandbox once `codex exec` exits `0`.
Codex's sandbox also refuses some destructive commands such as `rm -f`, so a brief that needs files deleted should expect Codex to decline or do the deletion another way.

```sh
#!/bin/sh
# radian arc command worker: codex exec (commit-outside-the-sandbox flow checked with codex-cli 0.159.1)
set -e
text='""'
status=0
# Codex's sandbox keeps `.git` read-only: the brief tells it not to commit, and the
# script commits outside the sandbox after `codex exec` succeeds.
{ printf '%s\n\n' 'Do not run git add or git commit; this worker script commits your changes.'; cat; } \
  | codex exec --json -C "$ARC_WORKTREE" -s workspace-write \
      -o "$ARC_STATE_DIR/last-message.txt" - || status=$?
if [ -f "$ARC_STATE_DIR/last-message.txt" ]; then
  text=$(jq -Rs . < "$ARC_STATE_DIR/last-message.txt")
fi
if [ "$status" -eq 0 ]; then
  if [ -n "$(git -C "$ARC_WORKTREE" status --porcelain)" ]; then
    git -C "$ARC_WORKTREE" add -A
    git -C "$ARC_WORKTREE" commit -m "$ARC_TASK: the worker's change" >/dev/null
  fi
  outcome=completed
else
  outcome=error
fi
printf '{"type":"Result","outcome":"%s","text":%s}\n' "$outcome" "$text"
```

Save it as an executable file and set both keys:

```toml
[worker]
command = ["/absolute/path/to/codex-worker.sh"]
rework_session = "fresh"
```

`codex exec resume` has no `-C` or `-s` in `0.159.1`, so the example always starts a new Codex session: keep `rework_session = "fresh"`, which is the default.
The wrapper's hand-off section still carries the previous attempt's report, and `ARC_STATE_DIR` is where the script can keep a session map of its own.

## An ACP agent as the worker

A flow that sets [`flows.<name>.acp`](files.md#configtoml) makes the wrapper an [Agent Client Protocol](https://agentclientprotocol.com) client with that program as the agent, instead of the radian worker's `radian acp`: it starts the program and talks ACP v1 to it over stdin and stdout.
The first element of the array is the program and the rest are its arguments, with no shell in between, resolved exactly as a `command` program is.
The agent works in the task's worktree, gets its own process group, and gets the [environment below](#the-environment), the `command` worker's on top of the wrapper's own.
radian does not sandbox it: the agent's own sandbox decides what it may read, and `ARC_DENY_READ` names the paths the wrapper denies the radian worker.

The radian worker and this agent share one ACP path; only the program and the session rules differ.
One attempt is one agent process. The wrapper sends `initialize` with no `fs/*` or `terminal/*` client capability, one `session/new` with the attempt's worktree as `cwd` and `mcpServers` `[]` (the radian worker calls `session/load` instead when it continues its session), and one `session/prompt` whose single content block is the brief the worker would receive (the `merge-main` conflict section, the hand-off, the task body, then [`worker-brief.md`](files.md#worker-briefmd)).
It sends no second prompt: the turn's end is the attempt's end, and the agent is stopped then by closing its stdin and, after a moment, its process group.

Every `session/update` the agent sends is written to `runs/<task>/attempts/<attempt id>/result.jsonl` as one line, in this envelope that keeps the protocol's own JSON whole:

```json
{"type":"acp","message":{"jsonrpc":"2.0","method":"session/update","params":{...}}}
```

The arc TUI's conversation tab folds these lines into the transcript the attach view draws: message and thought chunks become one Markdown message or one thinking block, a tool call becomes a tool card with its title and its `queued`/`running`/`done`/`failed` status (a `diff` block draws the patch card's diff and `content` text its folded output, every `diff` block first and the text after it when one update carries both, and a `terminal` block the title and status only), plan entries become one list, and a permission answer one weak line after the tool. `usage_update` and `session_info_update` become at most one weak status line with the latest `used`/`size` and title; `available_commands_update`, `current_mode_update` and an unknown update are not drawn (see [the task page's conversation tab](tui.md#the-task-page)).

`session/request_permission` is answered by one fixed policy, with no configuration key:

- a request whose path or paths all lie inside the attempt's worktree is answered `allow_once`;
- any path outside the worktree rejects it, `reject_once`;
- a request that names no path counts as inside.

The request and the answer are each one line of `result.jsonl`; the answer is `{"type":"acp","answer":{"requestId":<id>,"kind":"allow_once|reject_once|cancelled","optionId":<id or null>}}`.
The paths are the request's `toolCall.locations` (and a `path` in its `rawInput`).
`ARC_DENY_READ` names paths outside the worktree, so they fall on the rejecting side.

The `session/prompt` response's `stopReason` decides the attempt:

| `stopReason` | `Result.outcome` | What happens |
|---|---|---|
| `end_turn` | `completed` | The delivery check runs; the attempt passes when it delivers. |
| `max_tokens` | `max_tokens` | A failed attempt named after the reason; the retry rules apply. The turn ended `Incomplete` for a truncated answer that could not continue; see [the exit-code rule](../cli.md#a-truncated-answer) for when that happens. |
| `max_turn_requests` | `max_turn_requests` | The same. |
| `refusal` | `refusal` | The same. |
| `cancelled` | `cancelled` | The same, unless arc itself cancelled the attempt: then it is a cancel and the task ends `failed.cancelled`. |
| any other value | `failed` | A failed attempt under the retry rules. |

The last contiguous run of `agent_message_chunk` text becomes `Result.text`, so the next fresh attempt's hand-off reads it and `commit_dirty` can use its first line as the commit subject; see [`flows.<name>.commit_dirty`](files.md#configtoml), which applies to every worker kind.
The wrapper writes that `Result` line itself, after the agent has ended, in the same shape a `command` worker's last line has.
Nothing after it changes: the git control-file check, the delivery check, the push and `AttemptFinished` are the same.

The protocol has no fee, only `usage_update` token counts, which stay in `result.jsonl`.
A flow's agent's attempt cost is therefore unknown, exactly as a command worker's that reports none: it adds nothing to a task's or plan's totals, a budget never stops on it, and the task records [`cost_unknown`](files.md#task-fields).
When a claim starts a flow's ACP agent for a task or plan with a cost limit, `start`, `run` and `serve` print one line: `<task>: worker.acp reports no cost; the cost limit does not apply`. The radian worker reports `_meta.radian.cost` instead (see [the radian worker](#the-radian-worker)).

`radian arc cancel`, a stall and the wall clock stop an ACP worker the same way: the wrapper first sends `session/cancel` and answers a pending permission request `cancelled`, gives the agent a moment to end its turn, and then sends SIGTERM to its process group, waits `[limits] cancel_grace_secs` and sends SIGKILL.
An agent that ends its turn with `stopReason: cancelled` after an arc cancel is recorded as cancelled, not terminated.

`[worker] rework_session = "resume"` cannot be combined with a flow that sets `acp`: the file is refused before `start`, naming both keys.
A flow's ACP agent runs every attempt fresh, so an attempt after the first gets the hand-off section and a new agent session. The radian worker is not refused: `resume` opens its session with `session/load`.

### Installing an ACP agent

The Codex adapter is one package; install it globally so the worker does not need the network:

```sh
npm i -g @agentclientprotocol/codex-acp
```

Then one line starts it:

```toml
[flows.codex]
acp = ["codex-acp"]
```

`npx -y @agentclientprotocol/codex-acp` also works, but it downloads the package on first use: a worker without network access fails before its first turn.
Claude Code and Gemini CLI have their own adapters, and opencode speaks ACP natively; the same `acp` array starts any of them.

An adapter's environment variables belong in a wrapper script, not in a second configuration key: the `acp` array's program is the one process the wrapper starts, so a script that exports what the adapter needs and execs it is the whole configuration.

```sh
#!/bin/sh
# codex-acp with an agent mode the adapter reads from its environment.
export INITIAL_AGENT_MODE=workspace-write
exec codex-acp "$@"
```

```toml
[flows.codex]
acp = ["/absolute/path/to/codex-acp.sh"]
commit_dirty = true
```

The script keeps `acp` to one program; the wrapper passes the ACP traffic through it unchanged.

