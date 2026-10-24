# radian-arc

arc is the plan and task orchestrator (ADR 0048). It is part of the `radian` binary; it is
not a separate program. This crate is its control plane, in these modules:

- `store` — `<repository>/.radian/arc/`: `config.toml`, plans, tasks, the ledger, the locks
  and the transactions that change them, behind `radian arc init`, `add`, `list`, `show`,
  `adopt`, `confirm`, `auto`, `pause`, `start`, `reverify`, `cancel`, `reset-budget`, `merge`,
  `reject` and `run`.
- `attempt` — one attempt's life: the claim, the wrapper child, the applied result, cancel, and
  the reconcile step.
- `run` — `radian arc run`, the scheduling loop: reconcile, release retries, start the gate or
  a worker, apply what they left, and merge at auto level `merge`. A pass only schedules.
- `gate` — a fixed sequence of stages against the verification candidate: each stage's separate
  checkout, the `commands` stage, verdicts and their digest, the integration candidate, and
  `gate::background`, the gate as a separate process a pass starts and does not wait for.
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

`run_with_timeout` is the wrapper's variant: the command leads its own process group, its
pipes are drained, and a limit that runs out kills the whole group and reports `TimedOut`. The
worker wrapper gives every git command the wall time that is left, so a fetch or a push cannot
outlive the attempt.

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

[gate.review]
exec_args = ["--model", "m"]  # unset: `[worker] exec_args`
prompt_file = "review.md"      # unset: the built-in prompt only
timeout_secs = 900

[gate.profiles.default]
stages = [{ stage = "commands", required = true }, { stage = "human", required = true }]
commands = []        # shell command lines, run in order by the `commands` stage
key_commands = []    # shell command lines, run on the merge candidate by `integration`
```

- `Stage` is the closed set `commands`, `mutation`, `review`, `ci`, `human` plus
  `integration`. `integration` always runs at merge and a profile cannot list it.
  `LISTED_STAGES` is the closed list the parser, `Profile::validate` and the manual's drift
  test read; `STAGE_NAMES` is its names.
- `Profile::validate` rejects `integration`, a duplicate stage, and `review` listed before
  `commands` (the order ADR 0048 decision 10 fixes).
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
setup, naming `radian arc init`; with setup it opens the TUI that lives in `radian-tui`.

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

It then runs the current executable as `arc work --task … --attempt … --token … --session …
--remote … --main-branch … --stall-timeout-secs … --max-wall-time-secs …
--cancel-grace-secs … [--resume] [--allow-noop] -- <[worker] exec_args>`, with the task body
on stdin, and moves the task to `doing.running`. The claim picks the session from
`[worker] rework_session`: `<task>-<attempt>` with `fresh` (the default), the task id with
`resume`, and only the latter sets `--resume`. The wrapper, not the caller, enforces the
attempt's limits: no growth
in `result.jsonl` for the stall timeout, or `max_wall_time_secs` in total, sends SIGTERM to
the worker's process group (then SIGKILL after `cancel_grace_secs`); the reason is in
`signal.json`. The limits travel with the wrapper, so they hold after the `run` process that
started it exits or is killed. The wall clock starts at the wrapper's first step and bounds
its own git commands too: a fetch or a push still running at the deadline is killed with its
process group, `signal.json` gets `wall_time`, and the attempt still writes its records.

The wrapper also writes `attempts/<task>/<attempt>/heartbeat.json` (worker pid, process start
time, last beat) from the moment the attempt directory exists until both end records are
written: a beat at most every 15 seconds and at least three inside the attempt's stall
window, so a long worktree setup or push is not mistaken for a dead wrapper.

`attempt::spawn_claim` starts the wrapper and returns; `attempt::run_claim` waits for it and
applies the result (`start`). `radian arc run` uses `spawn_claim` and leaves the result to its
next pass.

The result is applied in one transaction, and only when its token and attempt id are the
task's current ones. `radian arc cancel` signals the wrapper by the pid **and** start time in
the attempt record, waits for its `AttemptFinished`, and leaves a task that never confirms in
`doing.reconciling`. `attempt::reconcile` applies a result that is there, leaves a liveness
heartbeat younger than the attempt's stall window alone, and turns a dead attempt without a
result into an `infrastructure` retry. It reads no process list; a claim without a heartbeat
yet is judged by its `start.json`.

`store::attempt_ops` holds the store side (`claim`, `claim_reverify`, `apply`, `release_retries`,
`record_auto_rework`, `reset_budget`), the closed `FAILURE_CLASSES` list the manual's drift test
reads, `REVERIFY_CLASS` (the worker-less attempt's class), and the backoff constants (10 s,
doubling, capped at 5 minutes). Cost is the
`Result.cost` delta against the previous attempt (ADR 0044), added to the task's `cost_usd`; a
reached task or plan budget pauses the task at `todo.paused_budget` instead of retrying, and a
plan budget also sets the plan's `auto` to `off`. `record_auto_rework` counts a stage or
integration failure while the plan is not `off` in the task's `auto_rework`; past
`[limits] max_auto_rework` it writes `paused.json` and sets the plan's `auto` to `off`.

`attempt_ops::apply` counts retries, not attempts: a retryable failure raises the task's
`retry_count` and sets `todo.retry_wait` while the count is below the task's `max_attempts`
(else `[limits] max_attempts`), and the failure after the limit is `failed.exhausted`. Every
rework (`plan_ops::add_rework_with`) sets `retry_count` back to `0`, so a rework's
instructions get their own retries; a `reverify` attempt is not a retry and changes neither
`retry_count` nor the decision.

## The scheduling loop

`run::run` takes `runner.lock` without waiting (a held lock is exit 4). One pass repeats until
a pass changes nothing, in this order:

1. `attempt::reconcile` for every task in `doing`, then
   `attempt::end_reconciling_of_cancelled_plans`, which ends a `doing.reconciling` task of a
   `cancelled` plan at `failed.cancelled` (the plan is final; nothing else would end it);
2. `attempt_ops::release_retries` for every due `todo.retry_wait`;
3. for every task in `review.verifying` (every plan; a plan at `auto` `off` is verified too):
   apply the record a background gate left, or start the gate with `gate::background::start`;
4. for each confirmed plan at `auto` `run` or `merge`, claim every `todo.ready` task whose
   `after` tasks are merged, up to `[limits] concurrency` claims, and start each wrapper with
   `attempt::spawn_claim` without waiting for it;
5. for each confirmed plan at `auto` `merge`, `review::merge_step` for every task in
   `review.awaiting_human` (take the approval and start the integration gate), in
   `review.integrating` (apply the integration verdict the gate left and push), or in
   `review.ready_to_merge` (finish the last passing integration).

The long work runs outside the pass: a worker is a separate `arc work` process and a gate is a
separate `arc gate` process. A one-shot `run` therefore exits after starting work; `--watch`
is what collects it. `[limits] concurrency` counts tasks in `doing`; a running gate takes no
slot.

A stage failure or a failed integration counts one automatic rework
(`attempt_ops::record_auto_rework`); past the limit the plan's `auto` becomes `off` and the
task stays in `rework_pending`. A merge refusal that changed the task back to
`review.verifying` (a moved branch, a changed digest) is a transition, so the loop verifies it
on the next pass; a refusal that changed nothing is a summary note, not a spin.

At quiet, the report lists each open plan and task with what it waits on (a person, an
`after` task, a retry time, a rework brief, a free concurrency slot). `run::run` hands every
line it prints (each transition, then the summary) to its sink, and every line also goes to
`.radian/arc/loop.log` through `run::LoopLog`.

`run::watch` repeats the pass every `--interval` seconds (default 30, 5 to 3600) until SIGINT
or SIGTERM; it holds `runner.lock` for the whole loop, prints the summary only when it changed,
prints a pass error as one `radian arc: <message>` line and runs the next pass, starts no new
worker or gate once the signal arrived, leaves a running worker alone, and signals a running
gate (`gate::background::stop`) before it returns.

`run::LoopLog` is the one writer of `loop.log`, used by `run` and `watch`: each line gets an
RFC 3339 UTC timestamp and a space, is appended and flushed at once, and a log at or above
1 MiB is renamed to `loop.log.1` before the next line (one old file is kept). A write error
prints one `radian arc:` line for the whole process and the loop goes on. The arc TUI reads
the file for its ticker and log view; it never holds `runner.lock`. `Store::try_runner_lock`
writes the holder's pid into `runner.lock` (the flock is the lock) so a viewer's header can
name the loop it is watching.

## Gate checkout

`GateCheckout::new(repository, stage, remote)` owns
`<repository>/.radian/arc/gates/<stage>/`, a detached linked worktree of the main checkout.
`GateCheckout::review_slot(repository, slot, remote)` owns the review stage's slot checkout,
`<repository>/.radian/arc/gates/review-<slot>/`, so two review sessions never share a tree.
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

`review` takes one of the slot locks `<repository>/.radian/arc/gates/review-<n>.lock`
(`SlotLock`), `n` from 0 to `[gate.review] concurrency - 1`; a free slot is taken at once and a
busy one is waited for. Slot `n` also names the checkout that session alone reads
(`gates/review-<n>/`), so the lock count is what bounds concurrent reviews.

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

`radian arc work --task <id> --attempt <n> --token <token> [--session <id>] [--resume]
[--allow-noop] [--remote <name>] [--main-branch <name>] [--stall-timeout-secs <n>]
[--max-wall-time-secs <n>] [--cancel-grace-secs <n>] [-- <extra radian exec args>]` runs one
attempt of one task, the brief on stdin. It is an **internal** subcommand: arc runs it, a
person does not. It is hidden from `--help` and has no manual page. `--remote`,
`--main-branch` and the three limit flags are internal too: `radian-tui` takes the names from
the repository's `[repository] remote` and `main_branch` when they are absent, and from
`origin`/`main` when there is no `config.toml`; `--stall-timeout-secs` and
`--max-wall-time-secs` of `0` mean no limit, while `--cancel-grace-secs` of `0` sends the
worker SIGKILL right after SIGTERM.

`--session <id>` is the worker's session id (the task id when absent), and `--resume` is the
`resume` mode's flag: only then does the wrapper pass `--resume <session>` instead of
`--session-id <session>`, and only when the session file exists. A `fresh` attempt
(ADR 0048 decision 4: `[worker] rework_session`, the default) never gets the flag, so it never
resumes; an attempt after the first also gets the hand-off section described in step 4.

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
   start time). The heartbeat writer already runs (it starts with the attempt directory, so
   the worktree setup counts too) and picks up the worker's identity here.
4. Runs the current executable as `radian exec -p - --session-id <session> --workspace
   <worktree> --profile workspace --yes --output jsonl` (a `resume` attempt whose session file
   exists passes `--resume <session>` instead of `--session-id`), with the caller's extra
   arguments and one hidden `--deny-read <path>` per entry of `.radian/arc/` except
   `worktrees/`. The worker runs in its own process group, so a SIGTERM reaches it and every
   child that stays in the group. Its stdout is `runs/<task>/attempts/<task>-<n>/result.jsonl`,
   its stderr `stderr.log`; the attempt fields, the session id included, go to
   `execution.json`. A fresh attempt after the first gets a hand-off section in front of the
   brief (`## Where the previous attempt left off`): the newest earlier attempt's
   `Result.text` cut at 8,000 characters with a note (a worker-less `reverify` attempt writes
   none, so the section walks back and names the attempt it came from), `git log --oneline`
   and `git diff --stat` of the task's own change from the merge base with the remote main. A
   `merge-main` conflict section stays first. A hidden `--worker-command <shell command>`
   replaces the `radian exec` child for the forwarding tests; it exists only for tests and is
   not a user input.
   While the child runs, the wrapper enforces the limits itself: a stall, the wall time, or a
   `cancel` records `signal.json` with its class, sends SIGTERM to the child's group and then
   SIGKILL after the grace period. The scheduling loop does not watch the attempt. The wall
   clock runs from `run_inner`'s first step; every wrapper git command (`crate::git::
   run_with_timeout`) gets the time that is left, and a command still running at the deadline
   is killed with its group, so the setup fetch and the push are bounded as well.
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
   and does not update `last_head`. Every failure after the attempt directory exists (a
   refused argument, a held worktree lock, a fetch that failed, a worktree that could not
   be prepared, a worker that could not start) writes the same record before the wrapper
   exits, so the next pass applies it and frees the attempt's concurrency slot at once;
   only a failure before those directories exist leaves no record, and reconcile then
   applies the attempt after the stall window.

8. Exits with the child's exit code (ADR 0044 §3). A check that fails after a `completed`
   child exits 1, with the reason in `failure`. A second attempt of the same task exits 4.

The `workspace` tier is required: unattended work never runs in `full`. The worker's sandbox
denies reading every entry of `.radian/arc/` except `worktrees/`, so the worker cannot read
plans, tasks, the ledger, the runs, the gate checkouts or another attempt's records. The push
credential is in the wrapper's environment only; in v0 the wrapper and the model are the same
OS user, so that is not isolation.

## Background gates

`gate::background` is the gate as a separate process (ADR 0048 decision 10). `start(store,
task, start)` opens `gates/<task>.gate.lock`, locks it, clears the descriptor's `FD_CLOEXEC`
and writes the digest of what the gate will read, and how many times a gate already died for
it, to `runs/<task>/gate.json` (`started`), then spawns `radian arc gate --task <task>
--lock-fd <fd>`: the child inherits the lock, so no second gate can start for that task until
it exits. The pass keeps no child handle it waits on; a thread reaps the child so a `--watch`
loop collects no zombies, and the child's pid goes into the `started` record so a stop that
finds only that record has a process to signal.

The child (`radian arc gate`) owns the lock for its life, reads the task and picks the gate by
its phase: `review.verifying` plans the verification and runs its stages, and
`review.integrating` runs `review::gate_integration` (the next free integration label). It
writes only verdicts and `runs/<task>/gate.json`: `running` with its pid first (keeping the
pass's inputs and death count), then `done`, `refused` or `error` with the attempt it ran
for. A verification record also carries the `inputs` digest from `plan_verification` (config
text, shas, extra stages, remote sha); an integration record carries a digest of the shas, the
config text and the remote main; a pass reports a `refused` record and starts no new gate
while the digest is unchanged. A gate process that exits having written nothing leaves the
pass's `started` record: the pass rewrites it as `no_record` with those inputs, reports it in
the summary and does not count the pass as changed, so a gate that fails at once is not
started on every pass. One that wrote `running` and died is `background::count_death`'s
business: the death goes up, and past `[limits] max_gate_deaths` the record becomes `died`,
which behaves like `no_record`. A gate the loop stops itself (`stop`) is `interrupted`, and
the next pass starts it again without counting. It never changes the task's state; the pass
holding `runner.lock` does that from the record.

The gate installs its own SIGTERM handler (`gate::stage`); a stage tracks its child's group
(`commands::run_one`, `ReviewRun::run_session`) and forwards the stop to it, SIGKILL after
`[limits] cancel_grace_secs`. A stopped stage writes no verdict, and the gate exits without a
completion record; `background::stop` then records `interrupted`, so the next loop starts it
again as unverified and counts no death.

## Verification

`radian arc start` runs verification after a pushed attempt in the same process
(`review::verify` = `plan_verification`, `run_stages`, `finish_verification`); a `radian arc
run` pass starts the same stages in a background gate and calls `review::apply_verification`
when the gate's record says `done`. Either way the remote's `work/<task>` must still equal the
task's `head_sha`, then the effective stages of the profile plus the task's extra stages run
in order, except `human`, each through the gate on the verification candidate
(`candidate_for`: `verified_sha` when the remote main is its ancestor, else a merge of
`verified_sha` onto the remote main). This build runs `commands` and `review`; a profile or
task that lists `mutation` or `ci` is refused with `stage <name> is not built yet` and the
task stays in `review.verifying`. An existing verdict for the current attempt and stage is
reused, so a gate that died between stages resumes.

`review` (`ReviewRun`) is a read-only session on the same candidate, after `commands` and
before `human` (a profile that lists both must list `commands` first). Up to
`[gate.review] concurrency` sessions run at once (default 2), each in its slot checkout
`gates/review-<n>/`. It runs one `radian exec` per run with `--tools read,grep,glob`, no
`--mcp-config` and `--no-config-mcp`. Its model route and wall-time limit come from
`[gate.review] exec_args` (default `[worker] exec_args`) and `[gate.review] timeout_secs`; a
task's first review reads `[gate.review.first] exec_args` and `timeout_secs` first, and a
rework review never does (`ReviewConfig::choice`). The prompt is built from the brief, the
commit list and the diff `base_sha..verified_sha`, plus `[gate.review] prompt_file`. The last
fenced `json` block of the final message is parsed; an empty `blocking` list passes, any item
fails with `review_blocking`. An item is `{level, file, line, text, pre_existing}`; a plain
string is an item with only its text, so an old verdict reads and re-serializes unchanged (the
digest is computed from the re-serialized verdict). Unusable output (no block, a missing
field, a level outside `REVIEW_LEVELS`, a timeout, a session error) runs the session once more;
two unusable runs fail with `review_output`, or `review_timeout` when the last run timed out.
The verdict records `review`, `review_session_ids`, `review_cost_micros`, `review_settings`
(`round`, the `exec_args` source key, `timeout_secs` and its source key) and, in a rework
round, `review_pre_existing`.

A review that follows an earlier review of the task, of a different commit, is a rework round
(`review::previous_review`); a review of the same commit (a `reverify` of an unchanged head)
is a first review. A rework round's prompt holds the previous blocking items, the commits
`<previous sha>..<sha>` and the increment, the hunks of the task's own diff that hold a line
`git diff <previous sha> <sha>` also has. arc, not the session, moves a blocking item to
`suggestions` with `pre_existing` unless its `file:line` is within 3 lines of an increment
line or within 10 lines of a previous blocking item in the same file (`ReworkRound`). A
`critical` item and an item without a `file` or a `line` are never moved; a moved item keeps
its `level`, and a `major` item outside both windows is still moved.

Any failing stage moves the task to `review.rework_pending`; a candidate merge conflict is
such a failure, with reason `conflict` and the paths. `review::conflict_auto_rework` writes
the conflict brief while the task's run of conflict reworks (`runs/<task>/conflict_rework.json`,
`CONFLICT_REWORK_FILE`) is below `[limits] max_conflict_rework`; once it has written the limit,
no brief is written, the task stays in `review.rework_pending`, and the stop text is part of
the recorded reason, so `show` prints it. A verification that does not end in a conflict, an
attempt that never reaches the gate, and the lead's own rework (`plan_ops::add_rework`, the
empty heading suffix) call `reset_conflict_rework`; arc's review and conflict reworks keep the
run. A `review_blocking` failure on a plan
whose `auto` is not `off` instead writes `## Rework <n> (review)` itself
(`review::review_auto_rework`), counts an automatic rework, and returns the task to
`todo.ready`; at `max_auto_rework` the plan pauses and the task stays in
`review.rework_pending`. When every required non-`human` stage passed, the task records
`verified_sha` and `verdict_set_digest` and moves to `review.awaiting_human`. The digest covers
the verification verdicts for `verified_sha` without the `human` and `integration` verdicts,
which are written later.

`review::rework_reason` is what `show` prints for a task in `review.verifying` or
`review.rework_pending`: the owner's reject note (`reject.md`), the last verify/merge reason
(`review.json`), or the automatic-rework pause (`paused.json`).

`radian arc reverify <task>` (`review::reverify`) claims a new attempt without a worker
(`attempt_ops::claim_reverify`) and calls the same `verify`: the attempt number goes up,
`head_sha` stays, the attempt result's class is `REVERIFY_CLASS`, and no `result.jsonl` is
written. It applies when the task is `review.rework_pending` or `review.awaiting_human` and its
rework reason is a verification stage; a reject note and a delivery failure are refused. The
reverify attempt is not a retry: it changes neither `retry_count` nor `max_attempts`, costs
nothing, and is not an automatic rework.

## Merge and reject

`radian arc merge <task>` requires `review.awaiting_human`, the remote `work/<task>` still at
`verified_sha`, every required stage passed for that sha, and the recomputed digest equal to
the recorded one. A changed digest returns the task to `review.verifying` with the reason,
like a moved branch, so `run` or `start` verifies it again. `prepare_merge` writes the owner's
`human` pass verdict (`approver` absent, or `auto` when the loop approves) and moves to
`review.integrating`; then `radian arc merge` runs `IntegrationRun` with the profile's
`key_commands` and finishes the push in the same command, while `review::merge_step` starts
the integration in a background gate and the next pass calls `finish_integration`. Either
path moves to `review.ready_to_merge` and pushes `<candidate>:refs/heads/<main>` without
force. A main that moved after integration, or a rejected push, makes it integrate again
(one round per pass in the loop; at most three runs per `radian arc merge` command). On
success the task is `done.merged` and its worktree is removed; the branch stays.

`finish_integration` then asks `review::propose_followup` for the task's review follow-up
(ADR 0048 decision 10): it reads every `review` verdict of the task, oldest attempt first, and
collects every round's `suggestions` (the moved `pre_existing` items among them), never a
`blocking` item, so a round that only had blocking items contributes nothing. Repeats are
dropped: two items are the same when their `file`, `line` and `text` match after a leading tag
that repeats the item's own level is removed and whitespace is collapsed, and the newest copy
is kept. The rest become one `todo.proposed` task of the same plan, `after` the merged task,
when `[gate.review] propose_followups` is true and the plan does not already hold it; its brief
groups the items by the round they came from (`From review <n> (attempt <n>):`) and names the
path of every review verdict. A task
that is itself a review follow-up gets none of its own; its review's unfixed items stay in its
verdict and in the task's final report. A failure to add the follow-up is a warning after the
merge, not a failed merge.

`merge_step` reads `runs/<task>/gate.json` for a finished integration, and a
`review.ready_to_merge` task finishes the newest passing integration verdict for its head.

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
- The follow-up's slug is `<merged task slug>-followups` cut from the end to
  `SLUG_MAX_CHARS`, then a trailing `-` from the cut is removed so it stays a valid slug. A
  plan that already holds the follow-up (the same title and `after`) gets no second one. A
  follow-up task gets none of its own: its title and single `after` entry are the ones arc
  gives a follow-up, the same test `has_followup` uses, so the chain stops after one level.
- `confirm` of a task checks `worker_args` first, as `confirm` of a plan does, so a broken
  worker line leaves the task `todo.proposed` instead of making it runnable.
- The remote is always a parameter, never the literal `origin`: the caller passes
  `[repository] remote`.
- A checkout that exists and is not a linked worktree is an error, unless it is an empty
  directory directly inside `gates/`; only that leftover is cleared.
- `checkout --detach --force` is used before `reset --hard`, so a modified tracked file from a
  previous command cannot block the prepare.
- `0` seconds means no command timeout.
- Reconcile decides liveness from the heartbeat's age, not from a process list: the
  `workspace` sandbox refuses `ps`. Where a signal is due, `ps` is read to compare a pid's
  start time (a reused pid is never signalled); without a reported start time the recorded pid
  alone is signalled.
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

The arc TUI lives in `radian-tui` (`src/arc/`, with its terminal loop in `src/cli/arc.rs`);
this crate owns the read-only views it draws ([`read`](src/read.rs): the remote main, a
task's attempts and verdicts, a task's diff, and the merge page's stage rows, review and
commits). The owner's keys there are the same library calls as the CLI's subcommands. The
gate stages `mutation` and `ci` are refused until they are built.
