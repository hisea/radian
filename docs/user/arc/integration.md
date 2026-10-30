---
description: How the merge builds the integration candidate and gets it onto the target branch: the styles, the key commands, the push, the command method and the checks.
---

# Integration

Integration is the step `radian arc merge` runs after your approval (see [`arc/index`](index.md#radian-arc-merge-task)).
It builds the candidate commit from the task's `head_sha` on the current target branch (`[integration] target_branch`, else `[repository] main_branch`) and runs the `[integration] key_commands` on it.
How the candidate then reaches the target branch is [`integration.method`](files.md#configtoml): `push` (the default) pushes it without force; `command` runs your own merge command and checks what it reports (see [the command method](#the-command-method)); `pr` runs the built-in pull-request preset of `command` (see [the pull-request preset](#the-pull-request-preset)).
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
arc itself never checks out or updates the main checkout's working tree: it fetches and reads refs, and with `method = "command"` or `"pr"` it runs your line or the preset in the repository root.

## The command method

With `method = "command"`, arc does everything above and then runs [`integration.command`](files.md#configtoml) instead of pushing: one shell command line, in the repository root, through the gate's shell runner, with [`gate.command_timeout_secs`](files.md#configtoml) as its limit.
arc itself pushes nothing; the command does the merge.
`method = "pr"` is the same path with the built-in preset script instead of a configured line; the manual shows it in full under [the pull-request preset](#the-pull-request-preset).

It receives the [hooks' environment](index.md#hooks), which describes the variables, plus `ARC_CANDIDATE_SHA`, `ARC_REMOTE` and `ARC_TITLE` (and `ARC_HOOK` is `integration` rather than a hook point); it does not inherit the repository variables of the process arc runs in, so its own git calls always work on the repository root it runs in:

| Variable | Value |
|---|---|
| `ARC_CANDIDATE_SHA` | The candidate commit the command must merge. |
| `ARC_REMOTE` | [`repository.remote`](files.md#configtoml): the remote the command pushes to. For the `pr` preset it also names the GitHub repository every `gh` call uses. |
| `ARC_TITLE` | The task's title. |

`ARC_MERGED_SHA` is not set: the command runs before there is a merged sha, and `after_merge` gets it.
`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY` and `GIT_ALTERNATE_OBJECT_DIRECTORIES` are removed from the environment the command starts with, the same rule arc's own git calls follow: a command that runs `git` never works on another repository than the one it runs in, whatever the process arc started from carried.
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
`method = "command"` without `[integration] command`, `[integration] command` with `method = "push"`, and `[integration] command` with `method = "pr"`, are config errors that name the keys.

### The pull-request preset

`[integration] method = "pr"` is the built-in `command` method for a protected branch on GitHub.
It runs the script below in place of [`integration.command`](files.md#configtoml), through the same runner with the same environment and the same answer, check and `pending` handling (see [the command method](#the-command-method)); `[integration] command` with `method = "pr"` is a config error, as `command` with `method = "push"` is.
It needs `gh` on `PATH` and an authenticated session (`gh auth login`); a missing or unauthenticated `gh` fails the integration at once with its message, and the task goes to `review.rework_pending`.
The script pushes `work/<task>` without force, so the pull request shows the verified head; it opens the pull request against `ARC_TARGET_BRANCH` with the task's title and id when none exists, answers `pending` while it is open, `closed` when it was closed without a merge, and the current target-branch sha when it is merged.
Who merges it (a person, a merge queue, CI auto-merge) is not arc's business: each later pass runs the script again and reads the state.
It derives the GitHub `owner/name` from the configured remote's URL, read with `git config --get remote.<name>.url` (not `git remote get-url`, which expands a `url.*.insteadOf` rewrite); it accepts `git@github.com:owner/name.git`, `https://github.com/owner/name.git` and `ssh://git@github.com/owner/name.git`, and passes `--repo owner/name` to every `gh` call, so [`repository.remote`](files.md#configtoml) decides the repository even when the clone has several remotes.
Any other URL fails the integration with a message that names it, and no `gh` call runs.
For another host or a different pull-request flow, use [`method = "command"`](#the-command-method) with your own line; `pr` is a preset, not another method.

```sh
set -eu

branch="work/$ARC_TASK"

# 1. The GitHub repository of the configured remote. The raw URL is read, not
#    `git remote get-url`, which would expand a url.*.insteadOf rewrite.
url=$(git config --get "remote.$ARC_REMOTE.url") || {
  echo "method = \"pr\": [repository] remote \"$ARC_REMOTE\" has no URL" >&2
  exit 1
}
case "$url" in
  git@github.com:*) repo=${url#git@github.com:} ;;
  ssh://git@github.com/*) repo=${url#ssh://git@github.com/} ;;
  https://github.com/*) repo=${url#https://github.com/} ;;
  *) repo= ;;
esac
repo=${repo%.git}
repo=${repo%/}
case "$repo" in
  ?*/?*) case "${repo#*/}" in */*) repo= ;; esac ;;
  *) repo= ;;
esac
if [ -z "$repo" ]; then
  echo "method = \"pr\": remote \"$ARC_REMOTE\" is \"$url\", not a GitHub URL (git@github.com:owner/name.git, https://github.com/owner/name.git or ssh://git@github.com/owner/name.git)" >&2
  exit 1
fi

command -v gh >/dev/null 2>&1 || {
  echo "method = \"pr\" needs gh on PATH; run gh auth login" >&2
  exit 1
}

# 2. Push the verified head, never with force, so the pull request shows it.
git push "$ARC_REMOTE" "refs/heads/$branch:refs/heads/$branch"

# 3. The pull request whose head is the task branch. `gh pr view` finds an open one;
#    `gh pr list --state all` also finds a closed or merged one.
state=$(gh pr view "$branch" --repo "$repo" --json state --jq .state 2>/dev/null || true)
if [ -z "$state" ]; then
  state=$(gh pr list --repo "$repo" --head "$branch" --state all --json state --jq '.[0].state // empty' 2>/dev/null || true)
fi

if [ -z "$state" ]; then
  # 4. No pull request yet: open one against the target branch and stay pending.
  gh pr create --repo "$repo" --base "$ARC_TARGET_BRANCH" --head "$branch" \
    --title "$ARC_TITLE ($ARC_TASK)" --body "Task $ARC_TASK: $ARC_TITLE" >/dev/null
  echo pending
  exit 0
fi

case "$state" in
  OPEN)
    echo pending
    ;;
  MERGED)
    # 5. The PR merged; print the target branch as it is now, not the PR's merge
    #    commit: others may have merged after it.
    git fetch "$ARC_REMOTE" "$ARC_TARGET_BRANCH" >/dev/null 2>&1
    git rev-parse FETCH_HEAD
    ;;
  CLOSED)
    # 6. Closed without a merge.
    echo closed
    ;;
  *)
    echo "the pull request for $branch is \"$state\"; expected OPEN, MERGED or CLOSED" >&2
    exit 1
    ;;
esac
```

## The command check

When the command prints a sha, arc fetches the remote target branch and checks two things:

- the sha is on the remote target branch: it is the branch's tip, or it is in the branch's history;
- the candidate's tree is the tree of that sha or of one of its ancestors.

Exact sha equality is not required, so a target that moved forward after the merge, or gained another merge after it, still passes when the candidate's content is in it.
The second check is a history lookup, not a comparison with the reported sha's own tree: a later merge gives the tip a larger tree, while the commit that landed the candidate keeps the candidate's tree.
A sha that is not on the remote, or one whose history holds no commit with the candidate's tree, records the reason and sends the task to `review.rework_pending`; it never reaches `done.merged`.

## The after-push check

With `method = "push"`, before `done.merged`, arc checks that what landed is what the gate verified:

- for `merge`, the remote target branch is the candidate and the candidate contains `head_sha`;
- for `squash` and `rebase`, the remote target branch's tree is the candidate's tree.

A failed check records the reason and moves the task to `review.rework_pending`; it never reaches `done.merged`.
A target that moves between the push and the check therefore cannot leave `done.merged` with a tree the gate did not verify.
