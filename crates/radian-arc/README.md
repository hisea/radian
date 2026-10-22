# radian-arc

arc is the plan and task orchestrator (ADR 0048). It is part of the `radian` binary; it is
not a separate program. This crate is its control plane, in these modules:

- `store` — `<repository>/.radian/arc/`: `config.toml`, plans, tasks, the ledger, the locks
  and the transactions that change them, behind `radian arc init`, `add`, `list`, `show`,
  `adopt`, `confirm`, `auto`, `pause`, `start`, `cancel` and `reset-budget`.
- `attempt` — one attempt's life: the claim, the wrapper child, the stall and wall timers,
  the applied result, cancel, and the reconcile step.
- `gate` — a fixed sequence of stages against one commit: each stage's separate checkout, the
  `commands` stage, verdicts and their digest, and the integration candidate.
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

`radian arc start <task>` is the v0 runner: there is no scheduling loop yet, so the owner
runs one task at a time. It reconciles `doing`, releases a due retry, and claims the task in
one transaction (`todo.ready` → `doing.starting`, `attempt` + 1, `<task>-<n>`, a random
token). A claim is refused (exit 3) when the task is not ready, its plan is not confirmed, an
`after` task is not done, or `[limits] concurrency` tasks are already in `doing`.

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

`store::attempt_ops` holds the store side (`claim`, `apply`, `release_retries`,
`reset_budget`) and the two closed lists the manual's drift test reads: `FAILURE_CLASSES` and
the backoff constants (10 s, doubling, capped at 5 minutes). Cost is the `Result.cost`
delta against the previous attempt (ADR 0044), added to the task's `cost_usd`; a reached task
or plan budget pauses the task at `todo.paused_budget` instead of retrying, and a plan budget
also sets the plan's `auto` to `off`.

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
stage (`ExclusiveLock`, a blocking `flock`). `CommandRun::run` takes it before `prepare` and
drops it after the verdict is written, so a second exclusive stage waits. A crashed process
releases the lock when its descriptor closes.

`integration` holds its own `<repository>/.radian/arc/gates/integration.lock` for its whole
stage, so two integration runs never work in the shared `gates/integration/` checkout at the
same time.

## `commands`

`CommandRun` takes the exclusive lock, prepares the `commands` checkout at `sha` (fetching
`remote`), runs the profile's lines with `sh -c` in order, and stops at the first failure. Each
command gets its own process group, its own log file (`<attempt>/logs/commands-<n>.log`,
absolute when the repository path is), and the profile's timeout; a timeout sends `SIGKILL` to
the whole group and records `exit_code: null` and reason `timeout`. A non-zero exit records
reason `command_failed`.

## Verdict

One verdict per stage per attempt, written once with create-new semantics to
`<repository>/.radian/arc/runs/<task>/attempts/<attempt>/verdicts/<stage>.json`. A second
write for the same path is `GateError::VerdictExists`; a verdict is never changed.

```json
{
  "schema_version": 1,
  "stage": "commands",
  "sha": "0123456789abcdef0123456789abcdef01234567",
  "result": "fail",
  "reason": "command_failed",
  "commands": [
    {
      "line": "cargo nextest run --workspace",
      "exit_code": 101,
      "duration_ms": 4210,
      "log": "/repo/.radian/arc/runs/0001-x/attempts/0001-x-1/logs/commands-01.log"
    }
  ],
  "started_at": "2026-01-01T00:00:00.000000Z",
  "ended_at": "2026-01-01T00:04:12.000000Z"
}
```

`result` is `pass` or `fail`; `reason` is always present and `null` on a pass. An
`integration` verdict also carries `base_sha` (the main it used) and `candidate_sha`, and a
conflicted merge carries `conflict_paths` (sorted). `Verdict::human` builds the owner's `human`
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

`IntegrationRun` takes `gates/integration.lock`, fetches `remote`, then compares `main_ref`
(after the fetch, for example `refs/remotes/<remote>/main`) with the task's `base_sha`:

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

`merge_ready(repository, remote, main_ref, verdict)` fetches `remote` again and is true only for
a passing integration verdict with a candidate whose recorded `base_sha` is still the remote
main. A main that moved after integration makes it false (ADR 0048 decision 6).

## `work`: the trusted worker wrapper

`radian arc work --task <id> --attempt <n> --token <token> [--resume] [--allow-noop]
[--remote <name>] [--main-branch <name>] [-- <extra radian exec args>]` runs one attempt
of one task, the brief on stdin. It is an **internal** subcommand: arc runs it, a person does
not. It is hidden from `--help` and has no manual page. `--remote` and `--main-branch` are
internal too: `radian-tui` takes them from the repository's `[repository] remote` and
`main_branch` when they are absent, and from `origin`/`main` when there is no `config.toml`.

One run:

1. Creates or reuses the linked worktree `<repository>/.radian/arc/worktrees/<task>` on
   branch `work/<task>`. A first attempt fetches `<remote> <main-branch>` and starts from the
   fetched tip, recording `base_sha` in `<repository>/.radian/arc/attempts/<task>/base_sha`;
   a later attempt reuses that base. A flock on the worktree directory keeps two attempts of
   one task apart (a second wrapper exits 4).
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
   hidden `--deny-read <path>` per entry of `.radian/arc/` except `worktrees/`. Its stdout
   is `runs/<task>/attempts/<attempt>/result.jsonl`, its stderr `stderr.log`; the attempt
   fields go to `execution.json`.
5. After the child exits, reads the last `Result` line and the exit code, reads
   `head_sha`, lists uncommitted and untracked paths, and checks: the branch exists,
   `head_sha` descends from `base_sha` and from the tip the attempt started on, the worktree
   HEAD is on the branch, the tree is clean, and the delivery check (a branch with no change
   past the base fails unless `--allow-noop`; any changed path under `.radian/arc/` fails).
6. Pushes `work/<task>` to the configured remote only when the outcome is `completed` and
   every check passed, then verifies the remote ref with `git ls-remote`.
7. Prints one `AttemptFinished` JSON line as its last stdout line (and writes it to
   `runs/<task>/attempts/<attempt>/attempt_finished.json` and
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

## Decisions where the ADR is silent

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
- A stored plan and a stored task use separate id sequences, so `0001-x` can name both. `show`
  and `adopt` look among the plans first, then the tasks.

## Not here yet

The scheduling loop, merge, reject, the arc TUI, and the gate's subcommands.
