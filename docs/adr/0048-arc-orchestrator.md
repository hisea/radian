# ADR 0048: arc, the Plan and Task Orchestrator

Status: accepted

## Background

Today a lead session dispatches work to radian workers through herdr panes, and a person runs the gate and the merge by hand. The lead writes briefs, watches panes, and copies tips between branches. This work is slow, and it fails when a message is lost or a pane stops.

arc replaces this loop. arc is a deterministic program, not a model. It keeps a backlog of plans and tasks on disk, starts one headless radian worker for each task, runs a fixed gate on the result, and merges only what passes. ADR 0043 keeps radian to one session kind with no in-process orchestration. arc does its work across processes: each worker is a separate `radian exec` process with its own journal.

## Decisions

### 1. Commands

arc is part of the `radian` binary (ADR 0032). It is not a separate program.

```text
radian arc                    open the arc TUI
radian arc <subcommand>       run one CLI operation and exit
```

- `radian arc` with no subcommand opens the TUI. A subcommand runs the CLI. This follows `radian` (TUI) and `radian exec` (headless).
- The CLI subcommands are `init`, `add`, `list`, `show`, `adopt`, `start`, `merge`, `reject`, `auto`, `confirm`, `pause`, `reset-budget`, `cancel`, `run`, `mcp`, and `work`.
- Exit codes: 0 success; 1 error; 2 usage error; 3 the state does not allow the operation (this includes a file changed outside arc); 4 a lock is held by another process. `list` and `show` take `--json`.
- `rad arc` is the same command through the `rad` alias.

### 2. Roles

- **lead**: a model session (Claude Code or the radian TUI). It analyzes a request, writes the plan and the tasks, and writes rework briefs. It writes to the backlog only through the `radian arc mcp` tools. It cannot confirm, start, merge, or approve.
- **arc**: the only writer of the backlog. It claims tasks, starts workers, reconciles attempts, checks delivery, runs the gate, records cost, and merges.
- **worker**: one `radian exec` process for one attempt of one task. It has no interactive input. It exits when the attempt ends.
- **gate**: a fixed sequence of stages. Each stage runs once against one commit and writes one verdict.
- **owner**: the person. The owner confirms plans, starts tasks, merges, rejects, and sets the auto level.

### 3. Objects

arc has four objects.

| Object | Meaning | Created by | State changed by |
|---|---|---|---|
| Plan | One product request: the request text, its tasks, and their order | lead | owner (confirm, auto, pause); arc |
| Task | One unit of work in one plan, with a brief | lead | arc |
| Attempt | One run of one task | arc | arc |
| Verdict | One gate stage result for one commit | gate | nobody: a verdict is never changed |

- A plan has `status` `draft` or `confirmed`. A plan is done when all its tasks are merged; `done` is not a stored status.
- After a plan is confirmed, its request text can only get appended change records. The text is never rewritten, because rework briefs use it as context.
- A confirmed plan gets no new tasks. To change a confirmed plan, the lead appends a change record (with any new tasks). The plan then returns to `draft` with `auto` `off`, and the owner confirms it again. Tasks that already started keep their state.
- A task has a `plan` field and an `after` list. `after` can name tasks in the same plan or tasks that are already merged.
- A plan has no branch. arc merges each task separately.

### 4. Identities

- `plan_id` and `task_id` have the form `NNNN-<slug>`, like ADR file names. The lead gives the slug: `[a-z0-9-]`, 1 to 48 characters. arc gives the number: the next free four-digit number, one sequence shared by plans and tasks, so no two items ever share a number whatever their slugs. An id names one item: every command that takes a plan or task id refuses (exit 3) an id that names both a plan file and a task file, naming both paths, and changes nothing. The id is the file name and never changes.
- `session_id` equals `task_id`: one task has one worker session. The first attempt that reaches the model creates it; every later attempt (a retry or a rework) resumes it (`--resume`). It does not fork (ADR 0022).
- `attempt_id` is `<task_id>-<n>`. Each attempt records its host, a random token, the worker process identity (pid and start time), its worktree, its branch, `base_sha`, `head_sha`, its start and end time, and its session id. arc refuses a result whose token does not match the current attempt.
- When a session cannot continue (for example `Result.outcome = poisoned`), the task goes to `failed` with phase `irrecoverable`. A new task replaces it and names the old one in `supersedes`. The old session does not change.

### 5. Backlog storage

Everything arc stores for a repository is in `<repository>/.radian/arc/`.

```text
<repository>/.radian/arc/
  config.toml                     repository, gate profiles and commands, budgets, concurrency
  plans/<plan>.md                 plan fields and the request text
  tasks/<task>.md                 task fields (state, phase, after, limits) and the brief
  ledger/                         for each plan and task file: state_version and content hash;
                                  approvals; verdict sets
  runs/<task>/attempts/<attempt>/ execution.json, result.jsonl, attempt_finished.json, stderr.log, verdicts
  attempts/<task>/<attempt>/      host-local attempt records (decision 7)
  gates/<stage>/                  the separate checkout for each gate stage (decision 10)
  state.lock                      short lock for each state change
  runner.lock                     held for the life of one scheduling loop
  worktrees/<task>/               the worker workspace for each task (decision 15)
```

- A task file keeps its path for its whole life. Its `state` (`todo`, `doing`, `review`, `done`, `failed`) and its `phase` are front-matter fields. Phases: `todo` has `ready`, `blocked`, `retry_wait`, `paused_budget`; `doing` has `starting`, `running`, `reconciling`; `review` has `verifying`, `rework_pending`, `awaiting_human`, `integrating`, `ready_to_merge`; `done` has `merged`; `failed` has `exhausted`, `cancelled`, `irrecoverable`.
- arc is the only writer. The lead changes plans and tasks only through the `radian arc mcp` tools. The owner changes them only through the arc TUI and CLI.
- **Outside changes.** Each time arc writes a plan or task file, it records the file's `state_version` and content hash in the ledger. Before arc acts on a file, it compares the file with the ledger. When they differ, someone changed the file outside arc: arc does not act on that plan or task, and it shows it as changed outside arc until the owner resolves it. `radian arc adopt <id>` resolves it: arc records the current content as its own. `adopt` refuses (exit 3) when the outside change touched `state`, `phase`, an approval, or a verdict field. This finds mistakes and casual edits. It does not stop a lead that changes the ledger too (decision 11).
- arc reads `config.toml`, plans, tasks, and the ledger only in the main checkout, never in a worktree.
- Workers and gate stages do not read the backlog. A worker gets its brief as a copy on stdin. The wrapper adds every entry of `<repository>/.radian/arc/` except `worktrees/` to the worker's sandbox deny list.
- Each state change is one transaction under `state.lock`: take the lock, check `state_version` and the hash, write a temporary file, fsync, rename on the same file system, fsync the parent directory, update the ledger the same way, release the lock. Git, ssh, and gate runs happen outside the lock.
- `runner.lock` allows one scheduling loop at a time. A second loop that cannot get the lock exits. The MCP server uses only `state.lock`, so the lead can add tasks while a loop runs.
- arc never commits a file in `.radian/arc/`. Whether git tracks `config.toml`, `plans/`, and `tasks/` is the owner's choice (decision 15).
- The fields frozen in v0 are identity, state and phase, attempt, and the verified sha. Other fields change through `schema_version`.
- `retry_at` is stored as an absolute UTC time.

### 6. States

| From | To | Condition |
|---|---|---|
| `todo.ready` | `doing.starting` | Every task in `after` is merged, a slot is free, and the owner started the task or the plan's auto level starts it. arc claims the task and creates an attempt in one transaction. |
| `doing.running` | `review.verifying` | arc receives a valid attempt end record (token matches, result present) and the delivery check passes (decision 8). |
| `doing.running` | `todo.retry_wait` | The worker ended, and the failure is an infrastructure or model failure that can retry. |
| `doing.running` | `doing.reconciling` | arc cannot prove the state of the worker. arc does not start the task again. A person decides. |
| `doing.running` | `todo.paused_budget` | A task or plan budget is used up. Only `reset-budget` returns the task to `ready`. |
| `review.verifying` | `review.awaiting_human` | Each required stage passed for the same `verified_sha`. arc records `verdict_set_digest`. |
| `review.verifying` | `review.rework_pending` | A `commands` or `mutation` stage failed. arc sends the result to the lead. |
| `review.awaiting_human` | `review.integrating` | The owner merges (or the plan is at auto level `merge`) with the sha equal to `verified_sha` and the digest unchanged. |
| `review.integrating` | `review.ready_to_merge` | The integration stage passed against the current main as `base_sha`. |
| `review.ready_to_merge` | `done.merged` | Main is still `base_sha`, main fast-forwards to `candidate_sha`, the push to `origin` succeeds, and `origin/main` contains `head_sha`. |
| `review.awaiting_human` | `review.rework_pending` | The owner rejects the task with a note. |
| `review.*` | `todo.ready` | The lead adds a rework brief. The attempt number goes up by one. |
| any | `failed.*` | Retries or budget used up (`exhausted`), cancel (`cancelled`), or a session that cannot continue (`irrecoverable`). |

- When the branch moves after the gate (`head_sha` changes), all verdicts and approvals for the old sha become invalid, and the task returns to `review.verifying`.
- When main moves after integration (it is no longer `base_sha`), the task returns to `review.integrating`.

### 7. The worker

For each attempt, the scheduling loop runs `radian arc work`. `radian arc work` is the trusted wrapper around the model. A person does not run it.

1. It creates or reuses the linked worktree `<repository>/.radian/arc/worktrees/<task>` on branch `work/<task>` (decision 15). The first attempt fetches the configured remote and main branch, starts from the remote main, and records `base_sha` in the host-local `attempts/<task>/base_sha`. At the end of each attempt the wrapper records the branch tip in `attempts/<task>/last_head`. A later attempt first checks that `work/<task>` still equals `last_head`; when it moved, the attempt fails before the model runs. In single-host v0 all worktrees share one clone.
2. It runs the model:

   ```text
   radian exec -p - --session-id <task> --workspace <worktree> --profile workspace --yes --output jsonl
   ```

   When the task's session file exists in the worker's session directory, the wrapper passes `--resume <task>` in place of `--session-id <task>` (the two flags exclude each other); the resumed session's id is the task id. The wrapper decides from the file, not from the attempt number: an attempt that failed before the model started (a missing credential, a worktree that could not be prepared) leaves no session, and the next attempt starts it with `--session-id`. The `workspace` tier is required: unattended work never runs in `full`.

   Before `start`, each `run` pass and `confirm` act, arc parses the line the worker would get — these fixed arguments, with placeholder values for the task, the worktree and the session, then `[worker] exec_args` — with the `radian exec` parser, in-process, without starting a session, calling a model or writing a file. A line the parser rejects, or that repeats an argument the wrapper owns (`--profile`, `--workspace`, `--session-id`, `--resume`, `--output`, `--yes`, `-p`), refuses the command with exit `3` and the parser's own message, names `worker.exec_args`, claims nothing and changes no file: `start` claims no attempt, `run` refuses before its first pass, `confirm` leaves the plan a draft. When the parser cannot be reached in-process without a dependency cycle, the current executable runs it behind a hidden parse-only flag; that flag is an internal input and is not in the manual.
3. After the model exits, the wrapper (not the model) reads the `Result` and the exit code, reads `head_sha`, and lists uncommitted and untracked files. It pushes `work/<task>` only when `outcome` is `completed` and the change is valid. After the push, it checks the remote ref with `ls-remote`. Every git command of the wrapper sets `core.hooksPath` to empty and `GIT_CONFIG_NOSYSTEM`, so no file the model can write changes what the wrapper runs.
4. Before the push, it checks that `work/<task>` exists, that `head_sha` descends from `base_sha`, that the worktree HEAD is on that branch, and that the tree is clean. If one check fails, the attempt fails, nothing is pushed, and the worktree stays for inspection.
5. Its last stdout line is its own `AttemptFinished` record: attempt id, token, exit code, outcome, `base_sha`, `head_sha`, dirty files, pushed, and remote sha. The radian `Result` is kept unchanged in `result.jsonl`.
6. When the wrapper sees the worker exit without a `Result` (crash, kill), the outcome is `terminated` and the attempt fails. Only when arc cannot reach the wrapper at all does the task go to `reconciling`.
7. It writes a host-local attempt record (token, pid and start time, session, worktree, branch, end result). After a restart, arc reads this record to reconcile. A flock on the worktree directory keeps two attempts of one task apart.

The push credential is in the environment of the wrapper only. In v0 the wrapper and the model run as the same OS user, so this is not isolation.

### 8. Delivery check

`Result.outcome = completed` means that the run ended (ADR 0044). It does not mean that the task is done. arc checks delivery separately: a dirty tree, no valid change (unless the brief allows a no-op), a sha mismatch, or a change to any file under `.radian/arc/` fails the attempt. A task can never change the arc configuration, plans, tasks, or ledger through its branch.

### 9. Failures, retries, stalls, and cancel

- Failure classes are infrastructure, model, verification, and cancel. Only infrastructure and model failures retry, with exponential backoff from 10 seconds to 5 minutes, within `max_attempts`. A cancel does not retry. A verification failure goes to the lead for rework; arc never writes a brief.
- Defaults: `max_attempts` 3, `stall_timeout` 15 minutes, `max_wall_time` 2 hours, `max_cost` not set (no limit), cancel grace period 10 seconds.
- A stall is no new line on the jsonl stream for `stall_timeout`. arc sends SIGTERM. `max_wall_time` is an absolute limit on top of it. Both record a failure class.
- A cancel applies to one attempt: SIGTERM, a grace period, then SIGKILL. arc waits for the wrapper to confirm the end. Without that confirmation the task goes to `reconciling`.
- arc keeps no state in memory. After a restart it takes `runner.lock`, reads each attempt in `doing/` from its host-local record, and checks the token and the process identity.

### 10. Gate

- A gate is a fixed profile: a list of stages. There are no conditions, no dynamic graph, and no override rules. A brief can add stages to the profile. It cannot remove a required stage.
- Stages are `commands`, `mutation`, `review`, `ci`, `human`, and `integration`. Each stage runs once against the verification candidate and writes one verdict.
- Before the verification stages run, arc fetches `[repository] remote` and builds the verification candidate in the stage's checkout. When the remote main is an ancestor of `verified_sha`, the candidate is `verified_sha` itself. Otherwise the candidate is a merge commit of `verified_sha` onto the remote main, made in that checkout and never pushed; the merge uses arc's own commit identity, so the same inputs build the same tree. A merge conflict is a failed stage: its reason is `conflict`, its verdict lists the conflicted paths, and the task goes to `review.rework_pending`. `verified_sha` stays the task's commit: the approval, the digest and the merge bind to it, and the candidate is evidence, not a new identity. A verification stage's verdict records `sha` (`verified_sha`), `main_sha` (the remote main the candidate was built on) and `candidate_sha` (the commit the stage ran on).
- `commands` runs only the fixed commands of the profile on a separate checkout of the candidate. A brief cannot add a shell command.
- `commands` and `mutation` are exclusive: one at a time, in separate trees, with no shared build state.
- `review` is a read-only radian session. It is not the worker session.
- `integration` merges `head_sha` onto the current main in a temporary tree and runs the key commands of the profile on the result. It records `base_sha` and `candidate_sha`. When main is still the task's `base_sha`, `candidate_sha` is `head_sha`. Otherwise it is a merge commit of `head_sha` onto main. arc never rebases a task branch, because a rebase changes the sha that the gate verified.
- Each stage runs in its own checkout under `<repository>/.radian/arc/gates/<stage>/`.
- The default profile is `commands` (required), then `human` (required). The radian repository profile is `commands` (required), `mutation`, `review`, then `human` (required).

### 11. Approval

- The MCP tools for the lead are `add_plan`, `add_task`, `update_plan`, `add_rework`, `list`, and `show`. There is no MCP tool to confirm, start, merge, reject, change the auto level, or approve.
- `reject <task> --note <text>` records the note in the task's run directory and moves the task to `review.rework_pending`. arc does not send it to the lead in v0: the lead reads it with the MCP `list` and `show` tools.
- A merge binds to `verified_sha` and `verdict_set_digest`. A new review of the same sha changes the digest and makes an older approval invalid.
- In v0 this is a process rule, not a security boundary: the lead and arc run as the same OS user, and the arc files are in the repository where the lead works. The lead can run the CLI from a shell or change the files. A real authorization boundary needs a separate identity and a separate store for arc. This ADR does not claim one.

### 12. Auto levels

`auto` is a plan field. Only a confirmed plan can have `auto` other than `off`.

| Level | What arc does | Where it stops |
|---|---|---|
| `off` | Nothing by itself. The owner starts and merges each task. | Each step |
| `run` | Starts each task when its `after` tasks are merged, and runs the gate. | `review.awaiting_human` |
| `merge` | `run`, and merges when every required stage and integration passed. | Failure, budget, or conflict |

- A failed gate sends the task, the verdicts, and the diff to the lead. When the lead adds a rework brief, arc starts the next attempt.
- `max_auto_rework` (default 2) limits automatic rework for one task. When a task reaches it, arc pauses the whole plan and sets `auto` to `off`.
- A plan budget (`max_cost`, `max_wall_time`) adds to the task budgets. When a plan budget is used up, arc pauses the whole plan, and its running tasks go to `todo.paused_budget`.
- Cost is counted per attempt. `Result.cost` is the session total (ADR 0044), so arc counts the cost that the attempt added.

### 13. The scheduling loop

- While the arc TUI is open, it holds `runner.lock` and is the scheduling loop. It advances plans at auto level `run` or `merge`. When the TUI closes, nothing advances.
- `radian arc run` is the same loop without a TUI. It makes every transition that it can make now, persists the state, and exits when nothing more can happen now. Waiting for a person, a retry time, or CI does not keep it running. It writes each transition line and flushes it when the transition happens, and prints the summary when the pass ends.
- `radian arc run --watch [--interval <seconds>]` repeats that loop: after a pass that ends quiet it waits `--interval` seconds (integer, default 30, 5 to 3600) and runs another pass, until SIGINT or SIGTERM. It holds `runner.lock` for its whole life. It prints the summary only when it differs from the one it printed last. On SIGINT or SIGTERM it starts no new work, lets a running attempt finish (attempts are separate processes with their own records; the next `run` reconciles them), prints the summary, releases the lock and exits `0`.
- The TUI and `radian arc run` use the same state machine and the same files.

### 14. v0 scope

v0 runs on one host. Concurrency defaults to 1 task at a time. The v0 gate stages are `commands`, `human`, and `integration`; `mutation`, `review`, and `ci` come later. The host fields stay in the file format and are not used.

v0 does not have: a resident daemon (`radian arc run --watch` is not one: it is a foreground loop that a person starts and stops); messages to a running worker (corrections go through rework); workers that read or write the backlog or hold the push credential; resume on a different host; more than one scheduling loop; a separate clone per task; automatic rework briefs written by arc; a gate language; load-based scheduling; worker kinds other than `radian exec`; plan branches or plan merges.

### 15. Setup, files, and directories

`radian arc init` sets up arc for one repository. arc does not guess the repository or the gate commands, so without setup `radian arc` and its subcommands refuse and name `radian arc init`.

`radian arc init` runs in the repository. It:

- finds the repository root, the `origin` URL, and the main branch, and writes `config.toml`;
- creates the layout of decision 5;
- adds the arc paths to `.git/info/exclude` (a local file: `init` never changes a committed `.gitignore`): always `ledger/`, `runs/`, `attempts/`, the two locks, and `worktrees/`; also `config.toml`, `plans/`, and `tasks/` unless `--track` is given;
- checks the requirements: a stored provider credential (`radian login`), a working push to `origin`, and a working `workspace`-tier sandbox (bubblewrap on Linux);
- prints the command that registers `radian arc mcp` with the lead. It does not change the lead's configuration.

The gate commands in `config.toml` are empty or the default profile. The owner writes them.

- **Tracking is the owner's choice.** With `--track`, git sees `config.toml`, `plans/`, and `tasks/`, and the owner commits them when the owner wants. arc never commits them. A checkout or pull that changes a plan or task file is an outside change (decision 5). The ledger, the runs, the attempt records, the locks, and the worktrees are always local.
- The worktrees are inside the repository directory, so they get the repository's trust decision (ADR 0031) and need no extra flag.
- A worktree is created on the first attempt of a task and removed when the task is merged.
- `.radian-worktrees/` is not a radian location. Search (`grep`, `glob`), skill discovery, and `@` completion skip `.radian/arc/worktrees/` and `.radian/arc/gates/` (they skipped `.radian-worktrees/` before). Both are arc's own full-tree checkouts, one per worker task and one per gate stage.

## Constraints and Costs

- arc code is in the repository from the start, so prototype work goes through the gate and Verify.
- In the `workspace` tier, git in the sandbox can write refs, index, and objects (ADR 0038). Linked worktrees share one common directory, so one worker can change another task's branch. v0 detects this before the push and fails the attempt safely. The remote ref is the truth. It does not prevent the change.
- The arc files, approvals included, are in the repository where the lead works. v0 finds outside changes to plans and tasks; it does not prevent a lead from changing arc's files.
- The push credential and the model share an OS user in v0. On macOS the `workspace` sandbox hides an ssh key under `~/.ssh` (ADR 0020 deny list), but it does not block the keychain: a worker can ask the system credential helper for an https credential that the keychain holds. Before v0 runs unattended, the remote must use ssh, or the https credential must not be in the keychain of the user that runs the workers.
- The arc TUI does not embed a terminal. The lead runs in a separate terminal and talks to arc only through `radian arc mcp`.

## Verification

- Backlog: concurrent `add`, `run`, and `cancel` lose no state and claim no task twice. An edit to a task file outside arc stops that task and shows it as changed outside arc; an edit to its approval-looking fields does not approve or merge it. A second `run` that cannot get `runner.lock` exits. A restart continues from the files.
- Attempts: when ssh or the wrapper connection drops while the worker still runs, arc does not start the task again. A result with an old token changes nothing.
- Wrapper: a worker that changes another task's ref makes that task's attempt fail without a push. A crash without a `Result` gives `terminated`, not `reconciling`. An outcome other than `completed` never pushes. A `core.hooksPath` in the worktree does not run during the wrapper's git commands.
- Gate: a verdict counts only for its sha. A moved branch removes it. A new review of the same sha makes an older approval invalid. Task B is verified on a candidate that contains merged task A. A merge fails when main is no longer `base_sha`.
- Auto: one plan with three tasks and dependencies runs at level `off` with one start and one merge each, at level `run` to `awaiting_human` for each task, and at level `merge` to `done` with no person. A task that fails its gate `max_auto_rework + 1` times pauses the whole plan and sets `auto` to `off`.
- Approval: the MCP tool list contains no tool to confirm, start, merge, reject, change `auto`, or approve.
- Setup: `radian arc init` in a repository creates the files above, and adds the exclude lines once (without `config.toml`, `plans/`, and `tasks/` under `--track`). Without setup, `radian arc` names `radian arc init`. A worker cannot read the arc files outside its worktree. A task branch that changes a file under `.radian/arc/` fails its delivery check. `grep` from the repository root does not enter `.radian/arc/worktrees/` or `.radian/arc/gates/`.
- Commands: `radian arc` opens the TUI. Each subcommand runs and exits with a documented code. `docs/user/` documents every subcommand, option, and file field (`AGENTS.md`).
