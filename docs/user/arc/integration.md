---
description: How the merge builds the integration candidate and gets it onto the target branch: the styles, the key commands, the push, the command method and the checks.
---

# Integration

Integration is the step `radian arc merge` runs after your approval (see [`arc/index`](index.md#radian-arc-merge-task)).
It builds the candidate commit from the task's `head_sha` on the current target branch (`[integration] target_branch`, else `[repository] main_branch`) and runs the `[integration] key_commands` on it.
How the candidate then reaches the target branch is [`integration.method`](files.md#configtoml): `push` (the default) pushes it without force; `command` runs your own merge command and checks what it reports (see [the command method](#the-command-method)).
The style changes the candidate's sha, never the task's `verified_sha`.

## The styles

`[integration] style` picks how the candidate is built:

| Style | What lands |
|---|---|
| `merge` (the default) | When the target is still the task's `base_sha`, the candidate is `head_sha` itself. Otherwise it is a merge commit of `head_sha` onto the target, with the message `Merge work/<task> (<first 7 characters of head_sha>)`. |
| `squash` | One commit on top of the target with the task's changes applied, with the message `<task title> (<task id>)`. It always makes exactly one commit, even when the task's change is empty (an empty commit). |
| `rebase` | The task's commits replayed on the target. The task branch must be linear: a branch whose target..`head_sha` range contains a merge commit is refused (see [A rebase needs a linear branch](#a-rebase-needs-a-linear-branch)). When the target already contains `head_sha`, the candidate is `head_sha` and no rebase runs. |

The merge commit and the squash commit use the fixed identity `arc <arc@localhost>`.

## A rebase needs a linear branch

`rebase` refuses a task branch when the remote target branch the run fetched and `head_sha` have a merge commit between them.
The check runs before the fast path and before any rebase, so nothing is built, no key command runs and nothing is pushed.
The failure reason starts with `rebase_needs_linear_branch: `, names the first seven characters of each merge commit, and says that a `rebase` integration needs a linear branch (use `merge` or `squash`).
The task goes to `review.rework_pending` like any failed integration.

The rule exists because `git rebase` without `--rebase-merges` drops a merge commit, and a change made only in the merge's resolution would disappear without an error; keeping the merge instead would depend on your git configuration.
arc runs the rebase with `--no-autostash` and the overrides `rebase.rebaseMerges=false`, `rebase.autoSquash=false`, `rebase.updateRefs=false` and `rebase.autoStash=false`, so a `rebase.*` setting in the repository or your global configuration cannot change the candidate.

## The key commands

The current `[integration] key_commands` run in order on the candidate, in the integration checkout.
The first non-zero exit or timeout fails the integration: the task goes to `review.rework_pending` with the reason and nothing reaches the target branch.
Integration reads the current list, never the `key_commands` an older attempt recorded in its `profile.json`.

## The push

With `method = "push"` (the default), the candidate is pushed to the remote target branch without force: `git push <remote> <candidate>:refs/heads/<target>`.
When the target moved after the integration (or the push is rejected as non-fast-forward), the task returns to `review.integrating` and `merge` integrates again, at most 3 times in one command.
After that it exits `3` with the task in `review.integrating`; a later `merge` continues from there.
arc itself never checks out or updates the main checkout's working tree: it fetches and reads refs, and with `method = "command"` it runs your line in the repository root.

## The command method

With `method = "command"`, arc does everything above and then runs [`integration.command`](files.md#configtoml) instead of pushing: one shell command line, in the repository root, through the gate's shell runner, with [`gate.command_timeout_secs`](files.md#configtoml) as its limit.
arc itself pushes nothing; the command does the merge.

It receives the [hooks' environment](index.md#hooks) plus `ARC_CANDIDATE_SHA`:

| Variable | Value |
|---|---|
| `ARC_TASK` | The task id. |
| `ARC_PLAN` | The task's plan id. |
| `ARC_ATTEMPT` | The task's current attempt, `<task>-<n>`. |
| `ARC_BRANCH` | The task's branch, `work/<task>`. |
| `ARC_WORKTREE` | The task's worktree, absolute. |
| `ARC_TARGET_BRANCH` | The branch the merge targets: [`integration.target_branch`](files.md#configtoml), else [`repository.main_branch`](files.md#configtoml). |
| `ARC_STATE` | `review.ready_to_merge`, the phase the command runs in. |
| `ARC_HOOK` | `integration`. The command is not a hook point; only the variables are the same. |
| `ARC_CANDIDATE_SHA` | The candidate commit the command must merge. |

`ARC_MERGED_SHA` is not set: the command runs before there is a merged sha, and `after_merge` gets it.
Its stdout and stderr go to `runs/<task>/attempts/<attempt id>/merge-command.log`.

**The answer.** The last non-empty line of the output decides; trailing blank lines are skipped and a CRLF ending's `\r` is trimmed.

| Last line | What arc does |
|---|---|
| A 40-hex sha | The merge is done and this is the target branch's sha after it. arc fetches the target and checks it (see [the command check](#the-command-check)). A passing check moves the task to `done.merged` and records the sha as its `merged_sha` (`after_merge` reads it as `ARC_MERGED_SHA`). A failing check records the reason and moves the task to `review.rework_pending`. |
| `pending` | Nothing merged yet. The task stays in `review.integrating`; the next scheduling pass, or the next `radian arc merge`, runs the same command again. It does not count toward the 3 integration runs, and the candidate is not rebuilt while the remote target branch is still the one the integration verdict was built on; when it moved, arc builds a new candidate. `radian arc merge` prints that the merge is pending and exits `3`. |
| `closed` | The merge was abandoned. The task goes to `review.rework_pending` with the reason `closed by the merge command`. |
| Anything else | A failed integration: the task goes to `review.rework_pending` with the last lines of the output in the reason. |

A non-zero exit or a timeout is a failed integration whatever the last line says: a command that pushes the sha and then fails is not a merge.
A command that answers `pending` runs again on every later pass, and a failed one can run again after a rework, so it must be safe to run more than once (like a hook).
`method = "command"` without `[integration] command`, and `[integration] command` with `method = "push"`, are config errors that name the keys.

## The command check

When the command prints a sha, arc fetches the remote target branch and checks two things:

- the sha is on the remote target branch: it is the branch's tip, or it is in the branch's history;
- its tree is the candidate's tree.

Exact sha equality is not required, so a target that moved forward after the merge still passes when the candidate's content is in it.
A sha that is not on the remote, or one whose tree differs from the candidate's, records the reason and sends the task to `review.rework_pending`; it never reaches `done.merged`.

## The after-push check

With `method = "push"`, before `done.merged`, arc checks that what landed is what the gate verified:

- for `merge`, the remote target branch is the candidate and the candidate contains `head_sha`;
- for `squash` and `rebase`, the remote target branch's tree is the candidate's tree.

A failed check records the reason and moves the task to `review.rework_pending`; it never reaches `done.merged`.
A target that moves between the push and the check therefore cannot leave `done.merged` with a tree the gate did not verify.
