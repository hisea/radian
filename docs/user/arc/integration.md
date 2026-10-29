---
description: How the merge builds and pushes the integration candidate: the styles, the key commands and the after-push check.
---

# Integration

Integration is the step `radian arc merge` runs after your approval (see [`arc/index`](index.md#radian-arc-merge-task)).
It builds the candidate commit from the task's `head_sha` on the current target branch (`[integration] target_branch`, else `[repository] main_branch`), runs the `[integration] key_commands` on it, and pushes it to that target branch without force.
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
The first non-zero exit or timeout fails the integration: the task goes to `review.rework_pending` with the reason and nothing is pushed.
Integration reads the current list, never the `key_commands` an older attempt recorded in its `profile.json`.

## The push

The candidate is pushed to the remote target branch without force: `git push <remote> <candidate>:refs/heads/<target>`.
When the target moved after the integration (or the push is rejected as non-fast-forward), the task returns to `review.integrating` and `merge` integrates again, at most 3 times in one command.
After that it exits `3` with the task in `review.integrating`; a later `merge` continues from there.
Integration never checks out or updates the main checkout's working tree: it fetches, pushes and reads refs only.

## The after-push check

Before `done.merged`, arc checks that what landed is what the gate verified:

- for `merge`, the remote target branch is the candidate and the candidate contains `head_sha`;
- for `squash` and `rebase`, the remote target branch's tree is the candidate's tree.

A failed check records the reason and moves the task to `review.rework_pending`; it never reaches `done.merged`.
A target that moves between the push and the check therefore cannot leave `done.merged` with a tree the gate did not verify.
