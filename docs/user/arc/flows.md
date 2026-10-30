---
description: What an arc flow is: the two-flow example, how a task picks one, the two concurrency limits, binary deliverables, and the five questions every flow answers.
---

# arc: flows

A **flow** is one named bundle of the settings a task's route needs: which worker runs its attempts, which gate profile verifies the result, how many of its tasks may run at once, and its defaults for `allow_noop` and `commit_dirty`.
It is not a new worker kind and not a new state machine: every flow's worker is the radian worker (`radian acp`) with its own arguments, one program of its own, or an ACP agent ([the worker](worker.md)), and the claim, the delivery check, the gate, the merge and the ledger are the same for every flow.
One repository can hold several flows, so coding, image and writing tasks can share one plan: a plan knows nothing about flows, and `after` may join tasks of different flows.

The authority for every key and field is [`config.toml`](config.md#configtoml) and [the task fields](files.md#task-fields); this page says what a flow is for and shows a complete setup.

## A complete example: `code` and `image`

A repository with a `code` flow for its source tree and an `image` flow for its artwork. `code` runs the radian worker and gates with a `commands` profile; `image` runs its own command worker and gates with `review` + `human`. The repository runs two attempts at once, one per flow:

```toml
schema_version = 1

[limits]
concurrency = 2

[gate]
profile = "code-check"

[gate.profiles.code-check]
stages = [{ stage = "commands", required = true }]
commands = ["cargo nextest run --workspace"]

[gate.profiles.image-review]
stages = [{ stage = "review", required = true }, { stage = "human", required = true }]

[flows.code]
concurrency = 1
gate_profile = "code-check"
exec_args = ["--model", "coding-model"]

[flows.image]
concurrency = 1
gate_profile = "image-review"
command = ["/usr/local/bin/image-worker", "--assets", "art"]
commit_dirty = true
```

- `code` starts the radian worker with `--model coding-model` on every attempt, and its gate runs the `code-check` profile: one `commands` stage on the gate's own checkout.
- `image` starts `/usr/local/bin/image-worker` instead of the radian worker, and its gate runs `image-review`: a `review` session, then the task stops at `review.awaiting_human` until you merge or reject it. A command worker receives the brief on standard input and writes its `Result` to standard output; [another agent as the worker](worker.md#another-agent-as-the-worker) gives the contract. Its `commit_dirty = true` lets the wrapper commit what the worker leaves uncommitted, so the worker may write its files and print a `completed` `Result` without running git itself.
- `[limits] concurrency = 2` lets two attempts run in the repository, and each flow's `concurrency = 1` lets one `code` and one `image` attempt run at the same time.

Every key's type, default, allowed values and effect time is in the [`flows.<name>.*` rows](config.md#configtoml).
The worker's session, brief and checks are in [the worker](worker.md), the stages in [the gate](gate.md), and the claim and the queue in [the scheduling loop](loop.md).

A flow can run an ACP agent instead of the radian worker or its own command: set `acp` to the program and its arguments, and the wrapper becomes the ACP client (see [An ACP agent as the worker](worker.md#an-acp-agent-as-the-worker)).
With the Codex adapter installed globally, the `image` flow above reads as one line:

```toml
[flows.image]
concurrency = 1
gate_profile = "image-review"
acp = ["codex-acp"]
commit_dirty = true
```

A flow sets one worker line: `acp`, `command` or a non-empty `exec_args`, never more than one, and `acp` cannot be combined with `[worker] rework_session = "resume"`.

## How a task picks its flow

A task's [`flow` field](files.md#task-fields) names one `[flows.<name>]` section; a task whose file has no `flow` field runs `default`.
`radian arc add task --flow <name>` and the MCP `add_task` tool set it when the task is created (see [`add task`](planning.md#radian-arc-add-task-plan-id-slug-slug-title-title-after-id-allow-noop-bool-stage-name-flow-name)).
The field may change only before the task's first attempt; after that, a different worker is a new task with `supersedes`.
`radian arc list` and the arc TUI show a task's flow only when the repository defines a flow other than `default`; a repository whose `config.toml` has no such flow prints no flow column, and the task page always names it (`flow default` when the file has none).

A flow the file does not define is not a silent fallback: the claim of a ready task refuses and names the flow (`task \`<task>\` uses flow \`<flow>\`, which \`config.toml\` does not define`), so `radian arc start` exits `3` and the task stays `todo.ready`. `radian arc run`'s summary repeats that line for a ready task, and so does the queue line `radian arc confirm --dry-run` prints for a proposed one. `radian arc list`'s task line, `radian arc show`'s `queue_reason` and the arc TUI print no reason for it: they name a slot only when a full limit holds the task, and a missing flow is not a limit (see [the queue reason](owner.md#trying-it-first-dry-run)).

The flow's settings reach the attempt at different moments, each stated in its own row:

- the worker (`acp`, `command` or `exec_args`) is chosen at the claim and recorded in the attempt's `start.json`, so a `config.toml` change takes effect at the next claim;
- the gate's profile is resolved when the gate is planned, and the attempt's `profile.json` records the profile the verification ran under, so a later merge is approved and integrated by that profile;
- `allow_noop` is read at the claim of each attempt, when the delivery check runs;
- `commit_dirty` is read at the same claim: when it is `true`, a `completed` worker's uncommitted or untracked changes are committed on the branch before that attempt's delivery check.

## The two concurrency limits

Two limits apply to every claim, and both must have room:

| Limit | Row | What it counts |
|---|---|---|
| `[limits] concurrency` | [`limits.concurrency`](config.md#configtoml) | every task in `doing` in the repository, whatever its flow. |
| `[flows.<name>] concurrency` | [`flows.<name>.concurrency`](config.md#configtoml) | the tasks in `doing` of that one flow. |

With the example above, one `code` and one `image` attempt run together, and a third task of either flow waits; two `code` tasks may never run at once, even though the repository total would allow them.
A ready task whose flow is full is skipped, so it never holds up another flow: the same pass claims a ready task of a flow that still has room further down the claim order.
A running gate takes no slot of either kind.

When a limit holds a ready task, the queue line names the limit that is full: `waiting for a free <flow> slot` (for example `waiting for a free image slot`) when the flow is full, else `waiting for a free concurrency slot` when the repository total is full; when both are full, the flow's reason wins.
The same words are `radian arc run`'s summary, the task line `radian arc list` prints, `radian arc show`'s `queue_reason` field, and the reason the arc TUI draws on the task's row (see [the queue reason](owner.md#trying-it-first-dry-run) and [the TUI](tui.md#the-plans-page)).
A file with no `[flows]` table has no per-flow limit at all: its `default`-flow tasks count against `limits.concurrency` alone, exactly as before flows existed.
A file that writes any `[flows]` section, the implicit `default` flow included, holds every flow to its own `concurrency`.

## Binary deliverables

A flow whose work is images, audio or any other binary file commits those files as ordinary git commits.
The delivery check is unchanged: the attempt must end on a commit that descends from the tip it started on, with a clean tree, and the change is what that commit range contains ([the checks after the worker exits](worker.md#what-the-wrapper-checks-after-the-worker-exits)).
Git LFS and reference-only deliverables — a commit that names a path or a hash in external storage instead of the bytes — are not supported; arc would need to learn the pointer format and the external store first, and no repository needs that yet.

## The five questions

Each flow's entry answers one question for itself, and the other four have one answer for every flow:

1. **What starts its tasks?** This is the flow's own answer. In this build every flow starts the same way — from `after` and `auto`: the scheduling pass claims a `todo.ready` task when every task in its `after` list is `done.merged` and its plan's `auto` level allows a start (`run` or `merge`), or you start it by hand with `radian arc start`. There is no clock and no external event yet, and a flow cannot make itself start: `auto` belongs to the plan. A flow would answer this question differently only once a trigger exists.
2. **What does it read and write?** Its worker's worktree for the task and the branch `work/<task>`. The worktree is made from the task's `base_sha` at the first claim and reused by later attempts, where the profile's `before_attempt` steps run first (a `merge-main` entry merges the newest main into it), so the worker reads that worktree as it is and writes only inside it, then pushes only its own branch ([the session and the brief](worker.md#the-session-and-the-brief), [the `.radian/arc/` layout](files.md#the-radianarc-layout)).
3. **What follows it?** The gate, and then `review.awaiting_human`: the task's flow decides the profile's stages, but the transition and the review are the same for every flow ([the gate](gate.md)). A failure goes to `review.rework_pending`, and the merge at `auto merge` follows the same path.
4. **Where does a person come back in?** At the gate's `human` stage, which every flow either lists in its profile or does not, and then at `review.awaiting_human`: you merge the task with [`radian arc merge`](owner.md#radian-arc-merge-task), reject it with a note with [`radian arc reject`](owner.md#radian-arc-reject-task-note-text), or end it with [`radian arc cancel`](running.md#radian-arc-cancel-id).
5. **What stops two attempts doing the same work?** The claim: one transaction under `state.lock` that moves the task out of `todo.ready` and writes a fresh `token`, and a result is applied only when its token matches the task's current attempt. A second pass that reads the same task finds it claimed and starts nothing.
