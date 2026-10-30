---
description: The `[hooks]` points, their rules and their environment.
---

# arc: hooks

The `[hooks]` points.

## Hooks

`[hooks]` runs configured shell commands at fixed points in a task's life.
The point names and their variables are fixed by the binary; only the command lines come from `config.toml`.
Each point is an array of shell command lines, default `[]`:

| Point | Fires after |
|---|---|
| `after_merge` | A task enters `done.merged`. |
| `after_fail` | A task enters any `failed.*` phase; a plan cancel records one point per task it ends. |
| `after_rework` | A task enters `review.rework_pending`; a conflict rework back to `todo.ready` is not this point. |
| `after_awaiting_human` | A task enters `review.awaiting_human`. |
| `after_proposed` | A new task enters `todo.proposed` (MCP `add_task` or the review follow-up); the MCP process only records it. |

[`radian arc init`](planning.md#radian-arc-init-track) writes three `after_merge` lines that delete the task's remote branch, worktree and local branch, naming the configured remote; a repository that wants to keep one removes its line. The commands read `[hooks]` when they run, not when the task moved. See [the config keys](config.md#configtoml) for the default.

**Rules.** The write that moves a task into one of those phases records the point in [`pending_hooks`](files.md#task-fields) and the `<state>.<phase>` it fires for in [`pending_hook_states`](files.md#task-fields) in the same transaction; a write that leaves the phase as it is records nothing. Only the holder of `runner.lock` runs hooks: every `run`/`serve` pass runs them first, and the owner's `merge`, `reject`, `cancel`, `start` and `reverify` run them after their transition when they can take the lock without waiting; the MCP server never runs one. A point is cleared only after its commands ran, so a killed pass runs it again: **a hook command must be safe to run twice**. A failing or timed-out command logs one `warning:` line and the next still runs; hooks never change a task's state or make a command fail. Each command's timeout is [`gate.command_timeout_secs`](config.md#configtoml), and its output goes to `runs/<task>/hooks/<point>-NN.log` (see [the layout](files.md#the-radianarc-layout)).

**Environment.** Every command receives these variables on top of arc's own environment:

| Variable | Value |
|---|---|
| `ARC_TASK` | The task id. |
| `ARC_PLAN` | The task's plan id. |
| `ARC_ATTEMPT` | The task's current attempt, `<task>-<n>`. |
| `ARC_BRANCH` | The task's branch, `work/<task>`. |
| `ARC_WORKTREE` | The task's worktree, absolute: `.radian/arc/worktrees/<task>`. |
| `ARC_TARGET_BRANCH` | The branch the merge targeted: [`integration.target_branch`](config.md#configtoml), else [`repository.main_branch`](config.md#configtoml). |
| `ARC_MERGED_SHA` | The sha the last merge put on the target branch, whether pushed or reported by an `[integration] command`. `after_merge` only. |
| `ARC_STATE` | The state and phase the point fires for: `done.merged`, a `failed.*` phase, `review.rework_pending`, `review.awaiting_human`, or `todo.proposed`. It is the phase the point's own write entered, even if the task moved again before the hook ran. |
| `ARC_HOOK` | The point name. |

**A phone notification.** This `after_awaiting_human` command sends only the repository name, task id and phase; it carries no transcript, command text, model, usage or cost:

```toml
[hooks]
after_awaiting_human = [
  "curl -s -X POST https://example.com/arc -H 'Content-Type: application/json' -d \"{\\\"repo\\\":\\\"\"$(basename \"$PWD\")\"\\\",\\\"task\\\":\\\"$ARC_TASK\\\",\\\"phase\\\":\\\"$ARC_STATE\\\"}\"",
]
```

The hook runs in the repository root; `$(basename "$PWD")` is its directory name. `after_fail` and `after_proposed` are the other points a notification usually watches.

