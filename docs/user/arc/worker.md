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
A fresh attempt after the first cannot see the earlier conversation, so its stdin opens with a hand-off section, `## Where the previous attempt left off`: the newest earlier attempt's final report (cut at 8,000 characters, with a note when it is cut; a `reverify` attempt runs no worker and writes none, so the section walks back to the attempt before it and names it), the `git log --oneline` of the task's own commits, and the `git diff --stat` of the task's own change, both from the merge base of the remote main and the branch tip.
The section says the full history is on the branch and to read the files rather than assume.
The brief follows it, and a `merge-main` conflict section still comes first.
When [`worker-brief.md`](files.md#worker-briefmd) exists, its content is the last section of every brief, so the order is: the `merge-main` conflict section, the hand-off, the task's brief, then the standing worker instructions.

## The steps before an attempt

Before the worker starts, the wrapper runs the profile's `before_attempt` steps in the worktree (see the [`before_attempt` row](files.md#configtoml)); a first attempt's steps do nothing.
`merge-main` fetches the remote main and merges it with `git merge --no-ff --no-edit` unless it is already an ancestor; a conflict leaves the worktree mid-merge and the worker's brief opens with the conflicted paths (see [A conflict with main](gate.md#a-conflict-with-main)).
A failed fetch or merge ends the attempt with git's message.

## The limits

The wrapper enforces the attempt's limits itself, so they hold even after the process that started the wrapper is gone:

- no new line in `runs/<task>/attempts/<attempt id>/result.jsonl` for the task's `stall_timeout_secs` (else `[limits] stall_timeout_secs`) sends SIGTERM to the worker's process group;
- `max_wall_time_secs` (else `[limits] max_wall_time_secs`) since the wrapper started does the same; this clock also bounds the wrapper's own git steps, so a fetch or a push still running at the limit is stopped the same way.
- a fetch, a push or an `ls-remote` is also bounded by [`limits.git_remote_timeout_secs`](files.md#configtoml); whichever of it and the attempt's remaining wall time is shorter applies, and the call, with everything it started, is killed when it runs out.
- after `[limits] cancel_grace_secs` the worker gets SIGKILL.

A limit of `0` means no limit for that timer.
The wrapper records the class in `runs/<task>/attempts/<attempt id>/signal.json` (`stall` or `wall_time`), forwards SIGTERM, waits for the worker, and still writes its `AttemptFinished` record.
`start` waits for the wrapper and applies the result; `radian arc run` does not wait (see `run`).

## What the wrapper checks after the worker exits

`Result.outcome` `completed` means that the run ended, not that the task is done (see the [result table](gate.md#failures-retries-and-the-budget)); the wrapper checks delivery itself before it pushes `work/<task>`, and it checks the repository's git control files before it runs any git command of its own after the worker. It requires that:

- `work/<task>` exists, `head_sha` descends from `base_sha` and from the tip the attempt started on, and the worktree HEAD is on the branch;
- the worktree has no uncommitted and no untracked files;
- the task's own change is not empty, unless [`allow_noop`](files.md#task-fields) is set (see [An empty change](gate.md#an-empty-change));
- the task's own change touches no path under `.radian/arc/`;
- the repository's git control files are as the wrapper recorded them before the worker started: `<common dir>/config`, `<git dir>/config.worktree` (the worktree's own git dir), `<common dir>/config.worktree`, every file under `<common dir>/hooks/` and `<common dir>/info/`, and every `config` file and every file under `hooks/` in `<common dir>/modules/**` (submodules). A file the worker creates, removes or changes counts as a change: the wrapper compares content, not time. The record stays in the wrapper's memory, never in the worktree, and the check does not depend on the worker's sandbox. A control file larger than 1 MiB is not read whole: an attempt does not start while one of those paths is that large, and one the worker creates fails the attempt and is removed.

A failed check fails the attempt with its reason in `runs/<task>/attempts/<attempt id>/attempt_finished.json`, which [`show`](index.md#radian-arc-show-id-json) prints; nothing is pushed. The git control check also restores every recorded file to its recorded content and removes every file that was not there before, then fails the attempt with the reason `the worker changed git control files: <paths, comma-separated>`. A control file the wrapper cannot read back counts as a change and is restored the same way. When the restore itself fails, the reason carries both failures, and the wrapper runs no further git command for that attempt. The wrapper stops everything the worker left in its process group before it compares the files, so a background process the worker started cannot change one after the worker exits.

## Another agent as the worker

`[worker] command` replaces the `radian exec` worker with one program of your choice for every attempt of every task ([`worker.command`](files.md#configtoml)).
The array's first element is the program and the rest are its arguments; there is no shell in between, so a pipeline or a variable belongs in a script you pass as the program.
Unset, the worker is `radian exec` and this section does not apply.

The program runs in the task's worktree, gets the same brief on stdin as a `radian exec` worker (the conflict section, the hand-off, the task body, then [`worker-brief.md`](files.md#worker-briefmd)), and its stdout goes to `runs/<task>/attempts/<attempt id>/result.jsonl` and its stderr to `stderr.log`.
The wrapper gives it its own process group and enforces the attempt's limits itself, exactly as it does for `radian exec`: no new stdout line for [`stall_timeout_secs`](files.md#task-fields), `max_wall_time_secs` in total, the cancel grace period after SIGTERM.
After it exits the wrapper runs the same git control-file check, [delivery check](#what-the-wrapper-checks-after-the-worker-exits), push and records; nothing about those changes.

### The environment

The wrapper adds these variables to the program's environment; they describe the attempt and are never read by radian itself.

| Variable | Value |
|---|---|
| `ARC_WORKTREE` | The attempt's worktree, absolute. The program's working directory. |
| `ARC_TASK` | The task id, such as `TK0001-work`. |
| `ARC_ATTEMPT` | The attempt id, `<task>-<n>`. |
| `ARC_SESSION` | The session id the claim chose (`<task>-<attempt>` with `rework_session = "fresh"`, the task id with `resume`). |
| `ARC_SESSION_START` | `new` or `resume`: `resume` on a `resume`-mode attempt whose session file exists, `new` on every other attempt. Always set. |
| `ARC_GIT_DIR` | `git rev-parse --absolute-git-dir` in the worktree. |
| `ARC_GIT_COMMON_DIR` | The absolute common git directory. A linked worktree's objects and refs live there, so give it to a sandboxed agent as writable if the agent must commit. |
| `ARC_DENY_READ` | The paths the wrapper would pass to `radian exec` as `--deny-read`, joined with `:`. A hint for your own sandbox, not a rule radian enforces on the command. |
| `ARC_STATE_DIR` | An absolute directory outside the worktree, `attempts/<task>/state/`, kept across the task's attempts. The command may keep its own state there, for example a session map. |

### The stdout contract

The program writes JSON lines to stdout.
The last line whose `type` is `Result` decides the attempt, exactly as it does for `radian exec`:

- `outcome` `"completed"` passes, and any other value fails the attempt with that outcome;
- no `Result` line at all fails it as `terminated`;
- `text` (a string) is the attempt's final report; the next fresh attempt's hand-off section reads it;
- `cost` is optional and has the shape `radian exec` writes (`{"micros": <integer>, "incomplete": <boolean>, "estimated": <boolean>}`). Without it the attempt's cost is unknown: it adds nothing to the task's or the plan's cost total, a budget never stops on it, and `show`, `list` and the arc TUI show it as unknown — and it stays unknown for the task after a later attempt reports a cost, because a number would leave this attempt out (see [`cost_unknown`](files.md#task-fields)). A `cost` that is `null`, or that carries no `micros`, is a reported zero, the shape `radian exec` writes when it recorded no fee. When a claim starts a command worker for a task or plan that has a cost limit, `start`, `run` and `serve` print one line for the attempt: `<task>: worker.command reports no cost; the cost limit does not apply`.

The command writes no `Result` line on a crash, and the attempt then fails as `terminated`, as a crashed `radian exec` does.

### What the command must do itself

arc checks the result, not the work: the program must commit its change on the branch, leave the worktree clean, and print the `Result` line.
A dirty or untracked file, no commit past the base, a commit that touches `.radian/arc/`, or a change to the repository's git control files fails the attempt exactly as it fails a `radian exec` worker.

### The differences from the `radian exec` worker

- **`--deny-read` does not reach it.** radian does not sandbox the command, so what it reads is up to its own sandbox: the arc store and `~/.radian/credentials` are readable unless the command's own sandbox denies them. `ARC_DENY_READ` carries the paths the wrapper would deny so your program can deny them itself.
- **Its sandbox is its own.** radian cannot select a profile or a tier for it. A sandboxed agent needs `ARC_WORKTREE` writable and, to commit, `ARC_GIT_COMMON_DIR` writable as well.
- **The cost limit does not apply.** A budget can never stop a command worker, because arc does not know its cost unless the command reports one. The one warning line above says so once per attempt.

### A complete Codex example

The example was checked against `codex-cli 0.159.1`.
It passes Codex's JSON events to stdout unchanged and then prints one `Result` line: `completed` when `codex exec` exits `0`, `error` otherwise, with `text` the content of the `-o` file, JSON-escaped by `jq -Rs`.

```sh
#!/bin/sh
# radian arc command worker: codex exec (checked with codex-cli 0.159.1)
set -e
text='""'
status=0
codex exec --json -C "$ARC_WORKTREE" -s workspace-write \
  --add-dir "$ARC_GIT_COMMON_DIR" \
  -o "$ARC_STATE_DIR/last-message.txt" - || status=$?
if [ -f "$ARC_STATE_DIR/last-message.txt" ]; then
  text=$(jq -Rs . < "$ARC_STATE_DIR/last-message.txt")
fi
if [ "$status" -eq 0 ]; then
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

`codex exec resume` has no `-C`, `-s` or `--add-dir` in `0.159.1`, so the example always starts a new Codex session: keep `rework_session = "fresh"`, which is the default.
The wrapper's hand-off section still carries the previous attempt's report, and `ARC_STATE_DIR` is where the script can keep a session map of its own.

