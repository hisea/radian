# radian-arc

arc is the plan and task orchestrator (ADR 0048). It is part of the `radian` binary; it is
not a separate program. This crate is its control plane, in these modules:

- `store` — `<repository>/.radian/arc/`: `config.toml`, plans, tasks, the ledger, the locks
  and the transactions that change them, behind `radian arc init`, `add`, `list`, `show`,
  `adopt`, `confirm`, `auto`, `pause`, `start`, `reverify`, `cancel`, `reset-budget`, `merge`,
  `reject` and `run`.
- `attempt` — one attempt's life: the claim, the wrapper child, the stall and wall timers,
  the applied result, cancel, and the reconcile step.
- `run` — `radian arc run`, the scheduling loop: reconcile, release retries, verify, start
  what the auto levels allow, and merge at auto level `merge`.
- `gate` — a fixed sequence of stages against the verification candidate: each stage's separate
  checkout, the `commands` stage, verdicts and their digest, and the integration candidate.
- `review` — verification of a pushed attempt, the owner's worker-less `reverify`, the
  owner's approval and merge, and reject.
- `work` — `radian arc work`, the trusted wrapper around one worker attempt. It is internal:
  arc runs it, a person does not.

Every git call of these modules is built in the private `git` module.

## The git helper

The private `git` module is the one place arc builds a git command. It starts from
`radian_coding::host_git::host_git_command`, the shared host builder (ADR 0039): every call
carries `core.fsmonitor=false`, `core.hooksPath=/dev/null` and `core.attributesFile=/dev/null`,
and the configured filter and diff driver commands neutralised, so no file a person or a worker
can write changes what arc runs. On top of that the helper:

- appends `-c core.hooksPath=`, which wins over the shared static value: hooks run none, and the
  hooks path is empty rather than `/dev/null`;
- sets `GIT_CONFIG_NOSYSTEM=1` (the system configuration does not take part) and
  `GIT_TERMINAL_PROMPT=0` (an unattended fetch or push fails at once instead of prompting);
- removes `GIT_CONFIG_COUNT` and `GIT_CONFIG_PARAMETERS`: a caller's injected configuration
  could otherwise replace the empty hooks path;
- removes `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`,
  `GIT_OBJECT_DIRECTORY` and `GIT_ALTERNATE_OBJECT_DIRECTORIES`: every call names its
  repository, so a caller's environment can never point it at another repository or object
  store.

The repository, global and system configuration files are otherwise unaffected.

## Configuration shape

The `store` owns parsing `config.toml`; the `gate` owns these types, and the field names are
the file's names.

```toml
[worker]
exec_args = []               # extra `radian exec` arguments, after the wrapper's own

[ids]
plan_prefix = "PL"           # 1 to 4 uppercase letters; the first letters of a new plan id
task_prefix = "TK"           # 1 to 4 uppercase letters; the first letters of a new task id

[gate]
profile = "default"
command_timeout_secs = 3600

[gate.profiles.default]
stages = [{ stage = "commands", required = true }, { stage = "human", required = true }]
commands = []        # shell command lines, run in order by the `commands` stage
key_commands = []    # shell command lines, run on the merge candidate by `integration`
```

- `Stage` is the closed set `commands`, `mutation`, `review`, `ci`, `human` plus
  `integration`. `integration` always runs at merge and a profile cannot list it;
  `Profile::validate` rejects it and duplicate stages. `LISTED_STAGES` is the closed list the
  parser, `Profile::validate` and the manual's drift test read; `STAGE_NAMES` is its names.
- A profile stage's `required` defaults to `true` when the field is omitted.
- `effective_stages(profile, extra)` adds a task's extra stages in their order. An existing
  stage merges into its profile entry and can only raise `required`: an extra stage can never
  remove or weaken a profile stage. An extra `integration` entry is ignored.
- `command_timeout_secs` is per command for `commands` and `integration`; `0` waits forever.

## Store

Everything arc keeps for one repository is in `<repository>/.radian/arc/`. `Store::open` finds
the repository from any directory inside it and refuses, naming `radian arc init`, when
`config.toml` is missing or does not parse. `Store::repository_names()` returns the
`[repository] remote` and `main_branch`; the internal `work` wrapper uses them when its own
flags are absent.

Every change is one transaction under `state.lock`: take the lock (waiting up to five seconds,
then exit 4), compare the file with the ledger, write a temporary file, fsync, rename, fsync the
directory, write the ledger entry the same way, release. A file that does not match its ledger
entry changed outside arc; `adopt` records the current content as arc's own and refuses an
outside change to a field only arc or the owner may set.

`init` creates the layout, writes `config.toml` once, appends the local exclude lines to
`.git/info/exclude` once, and runs its requirement checks. A bare `radian arc` refuses without
setup, naming `radian arc init`; with setup, this build has no TUI and says so.

## Attempts

`radian arc start <task>` runs one task and waits; `radian arc run` is the scheduling loop and
uses the same attempt path. Before `start`, each `run` pass and `confirm` act, `worker_args::check`
parses the worker's command line — the wrapper's fixed arguments with placeholder values plus
`[worker] exec_args` — with `radian-exec`'s own parser, and refuses (exit 3) a line that does not
parse or that repeats an argument the wrapper sets (`worker_args::OWNED_EXEC_ARGS`, the closed list
the manual's drift test reads). Nothing is claimed, started or written by the check.

`start` reconciles `doing`, releases a due retry, and claims the
task in one transaction (`todo.ready` → `doing.starting`, `attempt` + 1, `<task>-<n>`, a
random token). A claim is refused (exit 3) when the task is not ready, its plan is not
confirmed, an `after` task is not done, or `[limits] concurrency` tasks are already in
`doing`. A task already in `review.verifying` resumes verification instead of being claimed.

It then runs the current executable as `arc work --task … --attempt … --token … --remote …
--main-branch … [--resume] [--allow-noop] -- <[worker] exec_args>`, with the task body on
stdin, and moves the task to `doing.running`. While the wrapper runs, no growth in
`result.jsonl` for the stall timeout, or `max_wall_time_secs` in total, sends SIGTERM (then
SIGKILL after `cancel_grace_secs`); the reason is in `signal.json`.

The result is applied in one transaction, and only when its token and attempt id are the
task's current ones. `radian arc cancel` signals the wrapper by the pid **and** start time in
the attempt record, waits for its `AttemptFinished`, and leaves a task that never confirms in
`doing.reconciling`. `attempt::reconcile` applies a result that is there, leaves a live
wrapper or `start` process alone, and turns a dead attempt without a result into an
`infrastructure` retry.

`store::attempt_ops` holds the store side (`claim`, `claim_reverify`, `apply`, `release_retries`,
`record_auto_rework`, `reset_budget`), the closed `FAILURE_CLASSES` list the manual's drift test
reads, `REVERIFY_CLASS` (the worker-less attempt's class), and the backoff constants (10 s,
doubling, capped at 5 minutes). Cost is the
`Result.cost` delta against the previous attempt (ADR 0044), added to the task's `cost_usd`; a
reached task or plan budget pauses the task at `todo.paused_budget` instead of retrying, and a
plan budget also sets the plan's `auto` to `off`. `record_auto_rework` counts a stage or
integration failure while the plan is not `off` in the task's `auto_rework`; past
`[limits] max_auto_rework` it writes `paused.json` and sets the plan's `auto` to `off`.

## The scheduling loop

`run::run` takes `runner.lock` without waiting (a held lock is exit 4). One pass repeats until
a pass changes nothing, in this order:

1. `attempt::reconcile` for every task in `doing`;
2. `attempt_ops::release_retries` for every due `todo.retry_wait`;
3. `review::verify` for every task in `review.verifying`, for every plan (a plan at `auto`
   `off` is verified too);
4. for each confirmed plan at `auto` `run` or `merge`, claim every `todo.ready` task whose
   `after` tasks are merged, up to `[limits] concurrency` claims, then run them on one thread
   per claim (`attempt::run_claim`);
5. for each confirmed plan at `auto` `merge`, `review::merge_as(store, task, Some("auto"))`
   for every task in `review.awaiting_human`, and for a task an earlier merge left in
   `integrating` or `ready_to_merge`.

A stage failure or a failed integration counts one automatic rework
(`attempt_ops::record_auto_rework`); past the limit the plan's `auto` becomes `off` and the
task stays in `rework_pending`. A merge refusal that changed the task back to
`review.verifying` (a moved branch, a changed digest) is a transition, so the loop verifies it
on the next pass; a refusal that changed nothing is a summary note, not a spin.

At quiet, the report lists each open plan and task with what it waits on (a person, an
`after` task, a retry time, a rework brief, a free concurrency slot). `run::run` hands every
transition line to its sink as it makes it and returns the report; `radian arc run` prints the
summary with `Report::render_summary`.

`run::watch` repeats the pass every `--interval` seconds (default 30, 5 to 3600) until SIGINT
or SIGTERM; it holds `runner.lock` for the whole loop, prints the summary only when it changed,
and lets a running attempt finish before it returns. A signal starts no new pass.

## Gate checkout

`GateCheckout::new(repository, stage, remote)` owns
`<repository>/.radian/arc/gates/<stage>/`, a detached linked worktree of the main checkout.
`remote` is the repository's remote name, the `[repository] remote` of `config.toml`; the gate
never assumes `origin`. `prepare(sha)` fetches that remote, resolves `sha` to a full commit
(failing when it does not exist), creates the worktree when it is missing, otherwise detaches
it, then runs `reset --hard` and `clean -ffdx -e target`. Each stage keeps its own `target/`
between runs; trees of different stages never share build state.

A path that exists but is not a linked worktree is refused, never deleted — with one
exception: an **empty** directory directly inside `gates/` (what a failed `worktree add` leaves
behind) is removed and the worktree is added again. A directory with any content in it is
still `GateError::CheckoutOccupied`.

## Stage locks

`commands` and `mutation` hold `<repository>/.radian/arc/gates/exclusive.lock` for the whole
stage (`ExclusiveLock`, a blocking `flock`). `CommandRun::run` takes it before building the
candidate and drops it after the verdict is written, so a second exclusive stage waits. A
crashed process releases the lock when its descriptor closes.

`integration` holds its own `<repository>/.radian/arc/gates/integration.lock` for its whole
stage, so two integration runs never work in the shared `gates/integration/` checkout at the
same time.

## `commands`

`CommandRun` takes the exclusive lock, builds the verification candidate for `sha` in the
`commands` checkout (fetching `remote` and its main), runs the profile's lines with `sh -c` on
it in order, and stops at the first failure. The candidate is `sha` when the remote main is an
ancestor of it; otherwise it is a `--no-ff` merge of `sha` onto the remote main, created in
the checkout with the fixed identity `arc <arc@localhost>` and never pushed. A conflicted
merge aborts, writes a `fail` verdict with reason `conflict` and the sorted paths, and runs no
command.

Each command gets its own process group, its own log file
(`<attempt>/logs/commands-<n>.log`, absolute when the repository path is), and the profile's
timeout; a timeout sends `SIGKILL` to the whole group and records `exit_code: null` and reason
`timeout`. A non-zero exit records reason `command_failed`.

## Verdict

One verdict per stage per attempt, written once with create-new semantics to
`<repository>/.radian/arc/runs/<task>/attempts/<attempt>/verdicts/<stage>.json`. A second
write for the same path is `GateError::VerdictExists`; a verdict is never changed.

```json
{
  "schema_version": 1,
  "stage": "commands",
  "sha": "0123456789abcdef0123456789abcdef01234567",
  "main_sha": "fedcba9876543210fedcba9876543210fedcba98",
  "result": "fail",
  "reason": "command_failed",
  "commands": [
    {
      "line": "cargo nextest run --workspace",
      "exit_code": 101,
      "duration_ms": 4210,
      "log": "/repo/.radian/arc/runs/TK0001-x/attempts/TK0001-x-1/logs/commands-01.log"
    }
  ],
  "started_at": "2026-01-01T00:00:00.000000Z",
  "ended_at": "2026-01-01T00:04:12.000000Z",
  "candidate_sha": "fedcba9876543210fedcba9876543210fedcba98"
}
```

`result` is `pass` or `fail`; `reason` is always present and `null` on a pass. A verification
stage's `sha` is the task's `verified_sha`; `main_sha` is the remote main its candidate was
built on and `candidate_sha` is the commit the stage ran on (`sha` itself when main had not
moved, a merge commit otherwise, absent when the merge conflicted). `conflict_paths` (sorted)
is present when the candidate's merge conflicted. An `integration` verdict carries `base_sha`
(the main it used) and `candidate_sha` instead. `Verdict::human` builds the owner's `human`
record; it has no commands and carries the owner's note as `reason`.

`valid_verdicts(verdicts, head_sha)` keeps only verdicts for that commit, so a moved branch
invalidates the old set. `required_passed(profile, extra, verdicts, sha)` is true when every
required stage of the effective profile has a passing verdict for that sha; `integration` is
not part of it, because integration runs at merge after the approval.

The newest verdict per stage decides. `required_passed` reads the same counted set as the
digest, so a newer attempt that failed a stage outweighs an older attempt's pass, and the order
of the input does not matter.

`verdict_set_digest(verdicts, sha)` is the SHA-256 of the compact JSON array of the counted
verdicts, sorted by stage name, in the struct's field order (the declaration order above).
When a stage appears more than once for the same sha (several attempts), the newest by
`ended_at` counts; `started_at` and then the verdict's own JSON break ties. The digest does
not depend on input order, and a new attempt's verdict for the same sha and stage changes it.

## Integration

`IntegrationRun` takes `gates/integration.lock`, fetches `remote` for the work branch and
fetches the remote main with an explicit refspec
(`+refs/heads/<main>:refs/remotes/<remote>/<main>`), then compares `main_ref` (for example
`refs/remotes/<remote>/main`) with the task's `base_sha`:

- still `base_sha`: the candidate is `head_sha`, and the key commands run in the
  `integration` checkout at `head_sha`;
- moved: the checkout goes to the remote main and `git merge --no-ff` creates
  `Merge work/<task> (<short head>)` (the first seven characters of the head sha) with the
  fixed identity `arc <arc@localhost>`. The
  candidate is that merge commit. There is no rebase: a rebase changes the sha the gate
  verified. A conflict aborts the merge and writes a `fail` verdict with reason `conflict`,
  the sorted paths, and no candidate.

The key commands run on the candidate under the same rules as `commands` (order, stop at the
first failure, own timeout, own logs). The verdict records the main it used as `base_sha`.

`merge_ready(repository, remote, main_ref, verdict)` fetches the remote main with the same
explicit refspec and is true only for a passing integration verdict with a candidate whose
recorded `base_sha` is still the remote main. A main that moved after integration makes it
false (ADR 0048 decision 6). A clone whose configured fetch refspec leaves main out therefore
still reads the current main.

## `work`: the trusted worker wrapper

`radian arc work --task <id> --attempt <n> --token <token> [--resume] [--allow-noop]
[--remote <name>] [--main-branch <name>] [-- <extra radian exec args>]` runs one attempt
of one task, the brief on stdin. It is an **internal** subcommand: arc runs it, a person does
not. It is hidden from `--help` and has no manual page. `--remote` and `--main-branch` are
internal too: `radian-tui` takes them from the repository's `[repository] remote` and
`main_branch` when they are absent, and from `origin`/`main` when there is no `config.toml`.

One run:

1. Creates or reuses the linked worktree `<repository>/.radian/arc/worktrees/<task>` on
   branch `work/<task>`. Creation takes a blocking flock on `worktrees/` itself, because two
   attempts that start together would otherwise both write `.git/config` (`config.lock`). A
   first attempt fetches `<remote> <main-branch>` with an explicit refspec and starts from
   the fetched tip, recording `base_sha` in
   `<repository>/.radian/arc/attempts/<task>/base_sha`; a later attempt reuses that base. A
   flock on the worktree directory keeps two attempts of one task apart (a second wrapper
   exits 4).
2. Before a reused worktree's child runs, checks that `work/<task>` still points at the tip
   the previous attempt recorded in `attempts/<task>/last_head` and that the worktree HEAD
   is still on that branch. A ref that moved outside this task (another worker can move it
   from its `workspace` sandbox) fails the attempt with that reason before any child runs;
   nothing is pushed. Every attempt that ran ends by recording its branch tip there again.
3. Writes the host-local attempt record `attempts/<task>/<attempt>/identity.json` (token,
   the worker's pid and process start time, the session id, the worktree, the branch, the
   start time).
4. Runs the current executable as `radian exec -p - --session-id <task> --workspace
   <worktree> --profile workspace --yes --output jsonl` (a rework attempt passes
   `--resume <task>` instead of `--session-id`), with the caller's extra arguments and one
   hidden `--deny-read <path>` per entry of `.radian/arc/` except `worktrees/`. The worker
   runs in its own process group, so the SIGTERM the wrapper forwards from a cancel reaches
   it and every child that stays in the group. Its stdout is
   `runs/<task>/attempts/<task>-<n>/result.jsonl`, its stderr `stderr.log`; the attempt
   fields go to `execution.json`. A hidden `--worker-command <shell command>` replaces the
   `radian exec` child for the forwarding tests; it exists only for tests and is not a user
   input.
5. After the child exits, reads the last `Result` line and the exit code, reads
   `head_sha`, lists uncommitted and untracked paths, and checks: the branch exists,
   `head_sha` descends from `base_sha` and from the tip the attempt started on, the worktree
   HEAD is on the branch, the tree is clean, and the delivery check (a branch with no change
   past the base fails unless `--allow-noop`; any changed path under `.radian/arc/` fails).
6. Pushes `work/<task>` to the configured remote only when the outcome is `completed` and
   every check passed, then verifies the remote ref with `git ls-remote`.
7. Prints one `AttemptFinished` JSON line as its last stdout line (and writes it to
   `runs/<task>/attempts/<task>-<n>/attempt_finished.json` and
   `attempts/<task>/<attempt>/status.json`):

   ```json
   {"type": "AttemptFinished", "attempt_id": "<task>-<n>", "token": "…",
    "exit_code": 0, "outcome": "completed", "base_sha": "…", "head_sha": "…",
    "dirty": [], "pushed": true, "remote_sha": "…", "failure": null}
   ```

   `outcome` is the child's `Result.outcome`, or `terminated` when the child exited
   without a `Result`. A start refusal (the moved-ref check) also ends with
   `outcome: "terminated"`, `exit_code: 1` and its reason in `failure`; it runs no child
   and does not update `last_head`.

8. Exits with the child's exit code (ADR 0044 §3). A check that fails after a `completed`
   child exits 1, with the reason in `failure`. A second attempt of the same task exits 4.

The `workspace` tier is required: unattended work never runs in `full`. The worker's sandbox
denies reading every entry of `.radian/arc/` except `worktrees/`, so the worker cannot read
plans, tasks, the ledger, the runs, the gate checkouts or another attempt's records. The push
credential is in the wrapper's environment only; in v0 the wrapper and the model are the same
OS user, so that is not isolation.

## Verification

`radian arc start`, and `radian arc run` for every plan, run verification after a pushed
attempt (`review::verify`): the remote's `work/<task>` must still equal the task's
`head_sha`, then the effective stages of the profile plus the task's extra stages run in
order, except `human`, each through the gate on the verification candidate (`candidate_for`:
`verified_sha` when the remote main is its ancestor, else a merge of `verified_sha` onto the
remote main). This build runs only `commands`; a profile or task that lists `mutation`,
`review` or `ci` is refused with `stage <name> is not built yet` and the task stays in
`review.verifying`. An existing verdict for the current attempt and stage is reused.

Any failing stage moves the task to `review.rework_pending`; a candidate merge conflict is
such a failure, with reason `conflict` and the paths. When every required non-`human` stage
passed, the task records `verified_sha` and `verdict_set_digest` and moves to
`review.awaiting_human`. The digest covers the verification verdicts for `verified_sha`
without the `human` and `integration` verdicts, which are written later.

`review::rework_reason` is what `show` prints for a task in `review.verifying` or
`review.rework_pending`: the owner's reject note (`reject.md`), the last verify/merge reason
(`review.json`), or the automatic-rework pause (`paused.json`).

`radian arc reverify <task>` (`review::reverify`) claims a new attempt without a worker
(`attempt_ops::claim_reverify`) and calls the same `verify`: the attempt number goes up,
`head_sha` stays, the attempt result's class is `REVERIFY_CLASS`, and no `result.jsonl` is
written. It applies when the task is `review.rework_pending` or `review.awaiting_human` and its
rework reason is a verification stage; a reject note and a delivery failure are refused. The
reverify attempt is left out of `max_attempts`, costs nothing, and is not an automatic rework.

## Merge and reject

`radian arc merge <task>` requires `review.awaiting_human`, the remote `work/<task>` still at
`verified_sha`, every required stage passed for that sha, and the recomputed digest equal to
the recorded one. A changed digest returns the task to `review.verifying` with the reason,
like a moved branch, so `run` or `start` verifies it again. It writes the owner's `human`
pass verdict (`approver` absent, or `auto` when `merge_as` was called by `run`) and moves to
`review.integrating`, runs `IntegrationRun` with the profile's `key_commands`, moves to
`review.ready_to_merge`, and pushes `<candidate>:refs/heads/<main>` without force. A main
that moved after integration, or a rejected push, makes it integrate again, at most three
runs per command. On success the task is `done.merged` and its worktree is removed; the
branch stays.

Each integration run writes its own verdict, under
`runs/<task>/attempts/<attempt id>[.i<n>]/verdicts/integration.json`, so no verdict is ever
rewritten when main moves.

`radian arc reject <task> --note <text>` requires `review.awaiting_human`, writes the note to
the attempt's `reject.md`, and moves to `review.rework_pending`.

## Decisions where the ADR is silent

- The verification digest excludes the `human` and `integration` verdicts because both are
  written after verification, at merge; otherwise the recorded digest could never match
  again.
- `merge` resumes from `review.integrating` and `review.ready_to_merge`: a command that ran
  out of integration runs leaves the task there, and the next `merge` continues instead of
  asking for a new approval.
- `list` and `show` never use the network; only `merge` checks a moved branch.
- A reject note that is empty after trimming is a usage error.
- The gate's verdict, reason and worker output paths all use the attempt id (`<task>-<n>`) as
  the run directory; the host-local `attempts/<task>/<n>/` records keep the numeric attempt
  (decision 7).
- The merged task's worktree is removed with plain `git worktree remove`; a checkout with
  uncommitted files is left in place and reported as a warning, never forced away.
- The remote is always a parameter, never the literal `origin`: the caller passes
  `[repository] remote`.
- A checkout that exists and is not a linked worktree is an error, unless it is an empty
  directory directly inside `gates/`; only that leftover is cleared.
- `checkout --detach --force` is used before `reset --hard`, so a modified tracked file from a
  previous command cannot block the prepare.
- `0` seconds means no command timeout.
- A merge that fails without conflicted paths writes reason `merge_failed: <git stderr>`; a
  merge that already contains `head_sha` yields the current main as the candidate.
- The newest verdict per stage is what both the digest and `required_passed` count, so passing
  every attempt's verdicts in any order is safe.
- The integration stage has its own lock file, separate from the exclusive lock of `commands`
  and `mutation`, so an integration run and a `commands` stage may run at the same time.
- `init` records `origin` as the remote; it is the only name it knows before a config exists.
- Plans and tasks count in separate id sequences, and each new id carries its kind's
  configured prefix (`[ids] plan_prefix`, default `PL`; `task_prefix`, default `TK`), then
  the next number above every item of that kind regardless of the item's own prefix. An id
  without a prefix is malformed (exit `2`); the kind comes from the directory, so an item
  created under an older prefix still works. An id that names both a plan and a task is
  refused by `show`, `adopt` and `reset-budget` (exit `3`, naming both paths); nothing
  resolves it by preferring the plan.

## Not here yet

The arc TUI. The gate stages `mutation`, `review` and `ci` are refused until they are built.
