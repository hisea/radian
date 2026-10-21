# ADR 0030 implementation plan: orchestration primitives

- Date: 2026-10-15
- Branch/HEAD the facts were checked against: `oc/orch-plan` at `1a800fa` (identical to `origin/main`).
- Purpose: slice ADR 0030 (`docs/adr/0030-orchestration-primitives.md`, decided 2026-10-14) into an
  order of work before code is written. This is a document. It changes no code.
- Audience: the owner (for the decisions) and a worker the owner briefs to build a slice.

## How to read this

- `path:line` marks a fact read in the source on this branch. A code-reading subagent gathered most
  of them; the load-bearing ones (session setup, `RuntimeEvent`, `ForkParent`, `git_commit`,
  sandbox policy, `InputSource`, `App`) were re-checked by hand.
- `(ADR NNNN)` in a sentence means the fact comes from an ADR, not from code. `(unverified)` marks a
  claim not checked in code. Claims about timing, concurrency and process lifetime are flagged and
  are **not** relied on.
- Decision ids `O1`–`O13` and `R1`–`R10` are ADR 0030's own.
- The slice numbering here is a proposal, not ADR 0030's table; each slice maps to the ADR's table
  entry where one exists.

---

## 1. What exists today, and what each decision still needs

### 1.1 Foundations already built (by other ADRs) that 0030 builds on

These are done and are not re-proposed below; the relevant slice only consumes them.

| Thing 0030 needs | State today | Where |
|---|---|---|
| In-process multi-agent in one process | Two multi-thread runtimes already exist; per-session state is on `LocalSession` | `crates/radian-host/src/session.rs:456-494`, `crates/radian-tui/src/main.rs` and `crates/agent-cli/src/lib.rs` both build a `new_multi_thread` runtime (ADR 0030 background) |
| `open_session(setup, bindings)` | Exists; the single session-establishment point | `crates/radian-host/src/session.rs:903-906` |
| `HostBindings` | Defined and passed by both hosts | `crates/radian-host/src/session.rs:388-413` |
| `InputSource::Subagent`, `::Scheduler`, `::Mcp` | All three exist in the enum | `crates/agent-core/src/message.rs:222-233` |
| Async inbox operations with explicit source | `followup`/`steer`/`inject` (async) + `cancel` | `crates/radian-runtime/src/inbox.rs:224/235/246/357` |
| Pending queue with an `origin` per entry | `PendingQueue`, `push(origin, request)`, `abort_session_all(origin)` | `crates/radian-host/src/pending.rs:146-151`, `:202`, `:323` |
| Approval hooks with `auto` semantics and source lowering | `PermissionHooks::decide`, `apply_policy` | `crates/radian-permissions/src/hooks.rs:112-268` |
| `git_commit` host tool (R10) | Implemented, with `RepoPin`, hooks/filters off, path checks | `crates/radian-host/src/git_commit.rs` (whole file) |
| Sandbox: explicit network switch and git access | `SandboxPolicy::network(bool)`, `GitAccess::{ReadOnly,WritableExceptExec}` | `crates/radian-sandbox/src/policy.rs:148-151`, `:39-58` |
| Background jobs + completion notification (ADR 0037) | Job registry, durable start/settle events, busy-`inject`/idle-`followup` notifier | `crates/radian-coding/src/jobs.rs`, `crates/radian-host/src/job_notify.rs` |
| `JournalTail` incremental reader (ADR 0023 Step 5, reader half) | Implemented; used only by its own tests | `crates/radian-session/src/tail.rs:71-160`; consumer is `crates/radian-session/tests/tail.rs` |
| Trust gate (ADR 0031) | Implemented: records, resolve, TUI modal, CLI path | `crates/radian-host/src/trust.rs` |
| Layered system prompt and role prompts (ADR 0036) | 0a/1/2/3/0b assembly + `default`/`task`/`lead` prompts | `crates/radian-host/src/prompt.rs`, `crates/radian-host/prompts/{default,task,lead}.md` |
| herdr reporting (ADR 0034) | Implemented | `crates/radian-host/src/herdr.rs` |
| One binary with `radian exec` (ADR 0032) | `agent-cli` is a library; `radian`/`rad` dispatch it; `serve`/`mcp` are reserved stubs | `crates/radian-tui/src/main.rs:251-303,343-351` |

### 1.2 Decision-by-decision gaps

`exists` = nothing to build for this decision beyond wiring; `partial` = a base exists, the orchestration
part does not; `missing` = nothing.

| Decision | What 0030 wants | Code today | Gap |
|---|---|---|---|
| **O1** primitives not a lead | design stance | — | none (a stance) |
| **O2** task agent = a second `open_session` | `SessionCreated.parent = {session_id, seq, relation:"task"}`; task-bound approvals/events/memory; first input `Subagent` | `parent: Option<ForkParent{session_id,seq}>` at `crates/radian-session/src/writer.rs:34-38`; `InputSource::Subagent` exists but is **never constructed anywhere** (grep: zero `.rs` uses) | **partial**: `relation` field, task `HostBindings`, `Subagent` input, and the task/brief wiring are missing |
| **O2** three-layer approval (rev 2, item 3) | hard-deny list → optional edge policy (static or external command) → `auto`; `PermissionResolved.policy` | `PermissionHooks` already stacks deny > ask > allow and `auto` only rewrites `ask` (`crates/radian-permissions/src/hooks.rs:259-268`); `ResourceMatcher::CommandPrefix` deny rules exist (`crates/radian-permissions/src/rule.rs:52-60,149-155`); no shipped profile uses them | **missing**: hard-deny list on task profiles, an external-policy `Approver`, and the `policy` field |
| **O3** one worktree per task agent | `git worktree add <ws>/.radian-worktrees/<id> -b radian/<id>`; `shared` isolation forces `safe`; explicit network off; host-side `git_commit` only to `radian/<id>`; end-of-task commit | no worktree code anywhere (grep `.radian-worktrees` in `crates/` = 0); `SandboxPolicy::network(false)` exists; `GitCommitTools::with_allowed_branch` exists but is **never called** (grep = 0); `git_commit` refuses `main`/`master` unconditionally (`crates/radian-host/src/git_commit.rs:333-341`) | **missing**: worktree create/remove, `shared` mode, network-off wiring, `allowed_branch` wiring, end-of-task commit |
| **O4** the lead tool set | `spawn_task`, `send_agent`, `wait_task`, `wait_any`, `task_status`, `steer_task`, `cancel_task`, `list_tasks`, `remove_task`, `git_commit`; `ToolOutput` results; exclusive/shared concurrency | only `git_commit` exists. `ToolProvider`/`ToolSession` and `ToolOutput` are there (`crates/agent-core/src/tool.rs:78-108`) | **missing**: all task tools, a task registry, the FIFO queue, the `wait` semantics |
| **O5** two lead journal events | `TaskSpawned`, `TaskFinished` (`critical=false`) | `RuntimeEvent` is `#[non_exhaustive]` (`crates/radian-session/src/runtime.rs:119`) and has neither variant (grep: only in the ADR) | **missing**: variants, wire, reader, fixtures |
| **O6** process control port | Unix socket `$RADIAN_HOME/run/<pid>.sock`, JSON lines, the four inbox verbs + `list_sessions`/`spawn_task`/`wait_task`/`task_status`/`subscribe`; source fixed by the port | no listener code; `radian serve`/`mcp` are stubs; `home.rs` has no `run/` dir helper (`crates/radian-host/src/home.rs:46-136`); `herdr.rs` has a socket *client* only | **missing**: the whole port, the `run/` layout, `serve`/`mcp` reconciliation |
| **O7** project customization + merge | `<ws>/.radian/orchestration.toml` + `prompts/*.md` + `agents/*.md`; behind the 0031 trust gate; snapshot at spawn; verify and policy commands run in the task sandbox; 4 hook points; integration worktree | no `[orchestration]` config section (`crates/radian-host/src/config/mod.rs:429-451`); trust's trigger only knows `.radian/skills` (`crates/radian-host/src/trust.rs:403,410`); no project resource is read | **missing**: the config section, trust-list extension, snapshot, sandboxed verify, integration worktree |
| **O8** limits | `max_tasks` (4), `heavy_slots` (2), stall detection, task memory off, task MCP off by default | `radian-coding` has `MAX_RUNNING_JOBS = 4` for shell jobs (`crates/radian-coding/src/jobs.rs:44`) — a different cap | **missing**: `max_tasks`, `heavy_slots`, stall timestamps |
| **O9** shutdown/crash | per-task cancel+close+abort; crash leaves resumable journals; no auto-restart | `LocalSession::stop/abort/join` exist (`crates/radian-host/src/session.rs:664-679`); `close_pending` aborts by origin (`:657-660`) | **missing**: the task-tree teardown |
| **O10** TUI v0 | source on the queue (exists), task count, notification lines, `/tasks` (+cancel/kill), in-TUI session switching | `App` is a single-session view (`crates/radian-tui/src/app.rs:1106-1294`); no `SessionView` type exists; no `/tasks` | **missing**: the `App` split, the multi-session switch, `/tasks` |
| **O11** agent definitions are files | frontmatter + prompt at `~/.radian/agents/<n>.md` / `<ws>/.radian/agents/<n>.md` | role prompts exist (`crates/radian-host/src/prompt.rs:11-16,28`); no frontmatter parser, no `SessionSetup` mapping from a definition | **missing**: the definition file parser and its mapping |
| **O12** named vs task agents | named = one long session per name, `send_agent`, `/send <n>`, session lookup | — | **missing**: named-agent registry |
| **O13** lead is `spawn: true` | capability-gated tools; teams; main is a named `main`; depth 2; process-wide `max_tasks`; control-port `as:` | — | **missing** |
| **R1** default `auto` + hard-deny list | see O2/O3 | `auto` exists; list does not | **missing** |
| **R2** runtime-filled facts + verify | HEAD/clean/`no_commit`/verify results after an end-of-task commit | `git_commit` produces a `Committed{commit,branch,files}` (`crates/radian-host/src/git_commit.rs:92-97`); no verify runner | **missing** |
| **R3** `wait_any` | — | — | **missing** |
| **R4** heavy command slots | — | — | **missing** |
| **R5** stall observability | two timestamps + `stalled` | `SessionProjection` tracks turn/usage/tool/last-tool (`crates/radian-host/src/projection.rs:92-118`) but no file-write or event timestamps | **missing** |
| **R6** integration worktree | `.radian-worktrees/integration`, branch `radian/integration` | — | **missing** |
| **R7** report template | — | already in `crates/radian-host/prompts/task.md` ("Finishing" section) | effectively done (prompt text) |
| **R8** `list_tasks` diff stat | — | — | **missing** |
| **R9** task questions/approvals route via the lead (rev 3) | `answer_task`, `deny_task_approval`, `escalate_approval`; task `ask_user` becomes a lead input | `QueuedPrompter`/`QueuedApprover` route to one queue (`crates/radian-host/src/pending.rs`, `session.rs:988-992,1052-1057`) but there is no lead hop | **missing** |
| **R10** `git_commit` standalone | host-side commit tool | **done** (`crates/radian-host/src/git_commit.rs`), except the task-branch wiring (`allowed_branch`) and the end-of-task commit | **partial** |

Two facts about today's code that the plan must respect:

- **`git_commit` is unconditional about `main`/`master`** (`crates/radian-host/src/git_commit.rs:333-341`).
  ADR 0038 decision 4 says that refusal should apply **only to task agents**; the interactive
  `general`/`lead` session should be able to commit on `main`. This is a code/ADR conflict to resolve
  in the slice that makes task sessions (see open question 8).
- **`git_commit.rs` cites "ADR 0030"** for `allowed_filters` (e.g.
  `crates/radian-host/src/git_commit.rs:14,38,171`), but ADR 0030's file has no revision 4: it ends at
  revision 3 (`docs/adr/0030-orchestration-primitives.md`, 393 lines). The `[git]` config
  (`crates/radian-host/src/config/mod.rs:229-244`) documents the rule that exists only in code.

---

## 2. Slices in dependency order

Sizes: **S** ≈ a day, **M** ≈ a few days, **L** ≈ a week or more, for one worker with the tests green.
Each slice is independently verifiable. "Mutation tests" are the ADR 0030 test list's cases that the
slice owns.

### Slice 1 — Journal/session schema groundwork

**Goal.** Land the additive journal schema that every later slice writes and reads, with no producer
yet.

**Decisions.** O2 (`parent.relation`), O5 (`TaskSpawned`/`TaskFinished`), O2 rev 2
(`PermissionResolved.policy`).

**Crates/files.**
- `crates/radian-session/src/writer.rs:34-38` — add `relation: String` to `ForkParent`. Default
  `"fork"` on read (`#[serde(default = ...)]`); task sessions write `"task"`. ADR 0022's fork keeps
  writing `"fork"` (or omits it).
- `crates/radian-session/src/runtime.rs` — add `TaskSpawned { agent_session_id, worktree: Option<String>, label: Option<String> }` and `TaskFinished { agent_session_id, outcome: TurnOutcome }`; wire `name()` (`:246-266`), `critical()` → `false` (`:270-290`), `RuntimeData` (`:441-459`), `runtime_data()` (`:485-623`). Add optional `policy: Option<String>` to `PermissionResolved` (`:177-183`) and `PermissionResolvedData` (`:376-386`).
- `crates/radian-session/src/reader.rs:32-61` — extend `KNOWN_EVENTS` (it is `[&str; 28]`; the length changes) and the event match (`:474-775`), plus `PermissionResolvedData` handling (`:651`).
- `crates/radian-permissions/src/audit.rs:26-34` and `hooks.rs:320-345` — thread the optional policy
  name through `radian_permissions::PermissionResolved` and `runtime_event`.
- `docs/schema/fixtures/*.jsonl` + `docs/schema/check-fixtures.sh` (`KNOWN_EVENTS` at `:13-22`, the
  `EXPECT` dict) — new `task-events.jsonl` (or extend `fork-child.jsonl` with `relation`), and the
  `policy` field on `PermissionResolved`.
- `docs/adr/0006-session-jsonl-v1.md` §5.9/§6 — the schema text (ADR 0006 §5.9 already reserves
  `policy`; confirm the `relation` note).

**New public API.** None in `agent-core`. `radian-session` gains one enum variant pair and one field;
`radian-permissions` gains one field. No `agent-core` change.

**Schema/fixtures.** Yes, as above. The reader must still accept old lines (ADR 0006 §7(a)).

**Testing.** Deterministic, fixture-driven:
- `crates/radian-session/tests/read_fixtures.rs` reads the new fixture and cross-checks `recover`.
- `crates/radian-session/tests/runtime_events.rs` round-trips the new variants.
- `crates/radian-session/tests/fork.rs` asserts a fork child's `parent.relation` defaults to `"fork"`
  and a task child writes `"task"`.
- **Mutations to catch:** dropping `relation` from the wire; reading an old `parent` without `relation`
  as an error instead of defaulting to `"fork"`; marking `PermissionResolved.policy` as critical;
  omitting `policy` when present.
- **Hand test:** none.
- Also run `cargo test -p agent-core --features journal-selfcheck` (the new variants must not break
  replay).

**Size.** S.

**Parallel with.** Everything. It is the strict prerequisite of Slice 4.

---

### Slice 2 — Read-only observer facade

**Goal.** A `SessionFacade`-shaped read-only view of a session built on `JournalTail`, exposing only
committed facts (ADR 0023 U5/U6, ADR 0030 slice ①), so `task_status`/`stalled`/the TUI observer have
one source of truth.

**Decisions.** ADR 0030 slice ①; feeds O4 (`task_status`), R5 (`stalled`), O6 (`subscribe`), O10/⓪/⑥.

**Crates/files.**
- `crates/radian-session/src/tail.rs` — the reader exists; add derived facts only if needed (last
  committed assistant text, last event ts, last successful write ts) or compute them in the facade.
- `crates/radian-host/src/session.rs:183-230` — `SessionFacade` today requires `projection()`,
  `pending()`, `command()`, `answer()`, `compact()`. The observer implements `Observe` only. Decide
  (open question 2) whether to implement this trait with empty/no-op answers or introduce a narrower
  `Observer` trait.
- A new module, e.g. `crates/radian-host/src/observer.rs`, wrapping `JournalTail` + a
  `SessionProjection` it updates on `poll()`.

**New public API.** A facade/observer constructor `open(path) -> Observer`. No `agent-core` change.

**Schema/fixtures.** None.

**Testing.** Deterministic, using session files written by `SessionJournal` in a temp dir:
- A pending (uncommitted) replacement is not shown (ADR 0019/0023 U6).
- Incremental polls equal a fresh open at every byte split (mirror `crates/radian-session/tests/tail.rs`).
- A torn tail is not read until complete; a file-identity change resets.
- **Mutations:** showing a pending segment; folding from scratch each poll; ignoring a reset.

**Size.** M.

**Parallel with.** Slice 1, Slice 3.

---

### Slice 3 — TUI `App` split (ADR 0030 slice ⓪)

**Goal.** Pure refactor: split `App` into global state + per-session view, no behavior change.

**Decisions.** ADR 0030 slice ⓪; ADR 0023 (composition boundary); prerequisite for Slice 11.

**Crates/files.** `crates/radian-tui/src/app.rs:1106-1294` and all callers. The structural tests
`crates/radian-tui/tests/purity.rs` and `crates/radian-tui/tests/structure.rs` constrain where facts
live and must keep passing.

**New public API.** None.

**Schema/fixtures.** None.

**Testing.** All existing unit and `pty` tests pass unchanged. **Mutation:** a behavior change here is
the whole risk — the guard is "no test changes".

**Size.** L. This is the highest-churn slice and the least urgent; it can wait until the orchestration
core (Slices 1, 2, 4, 5) is shaped, or run in parallel to absorb a worker.

**Parallel with.** Slices 1, 2, 4, 5.

---

### Slice 4 — Task-session lifecycle

**Goal.** `open_session` a second time, in-process, as a task agent: its own worktree, sandbox
network off, `memory_root = None`, `Subagent` inputs, `parent.relation = "task"`, a lead-journal
`TaskSpawned`; `git_commit` restricted to `radian/<id>`; an end-of-task commit; `remove_task`.

**Decisions.** O2 (session/bloodline/bindings), O3, O9, R10 (branch wiring + end-of-task commit),
R2 (the end-of-task commit half), O8 partially (`memory_root=None`).

**Crates/files.**
- New `crates/radian-host/src/task.rs` (or `orchestration/task.rs`): a `TaskRegistry` keyed by task
  id, holding each `LocalSession` + worktree path + branch + start seq.
- Worktree: `git worktree add <ws>/.radian-worktrees/<id> -b radian/<id>` and `git worktree remove`
  (host-side, outside the sandbox). Add `.radian-worktrees/` to the repo `.gitignore` and to the
  coding tools' ignore list (ADR 0030 O3, following ADR 0020 decision 4).
- `crates/radian-host/src/session.rs` — the task setup: `sandbox` with `network(false)`
  (`crates/radian-sandbox/src/policy.rs:148`), `GitAccess::ReadOnly` (default), `memory_root: None`,
  `role: Some("task")`, and a `HostBindings` whose approvals/questions route to the lead's queue.
- `crates/radian-host/src/git_commit.rs:191-196` — call `with_allowed_branch(Some("radian/<id>"))` for
  the task session. End-of-task commit: on turn end/cancel/failure, if the worktree is dirty, run the
  same commit path with the fixed message `radian: end of task <id> (<outcome>)`.
- `isolation = "shared"`: no worktree, root = lead workspace, force `safe`; no branch/verify/end
  commit.

**New public API.** `TaskRegistry::{spawn, remove, get}` in `radian-host`. No `agent-core` change.

**Schema/fixtures.** Uses Slice 1's `relation` and `TaskSpawned`/`TaskFinished`.

**Testing.** Deterministic (temp repos):
- A task session opens with a clean worktree on `radian/<id>`; its journal's `SessionCreated.parent.relation == "task"`; the lead's journal gets `TaskSpawned`.
- The task sandbox has network off (an outbound connect fails; the sandbox matrix's `network_*` group plus a task policy case).
- A shell `git commit` fails in the worktree; `git_commit` succeeds on `radian/<id>` and is refused when HEAD is not `radian/<id>`.
- After the task ends, the worktree is clean and HEAD is the end-of-task commit; with no changes, no commit is made; `shared` isolation writes no end commit.
- **Mutations:** allow network; forget `allowed_branch`; run the end commit on `shared`; leave the worktree dirty; use the lead's workspace instead of the worktree.
- **Hand test:** one real `git worktree list` after spawn/remove.
- **Concurrency note:** "two task sessions open at once" is a real risk but timing-dependent; test it
  with a deterministic driver (both sessions opened, then driven), not with sleeps.

**Size.** L.

**Depends on.** Slice 1.

**Parallel with.** Slices 2, 3, 5 (approval can be designed in parallel but lands here or after).

---

### Slice 5 — Three-layer approval, and R9 routing

**Goal.** Task approvals resolve hard-deny → optional edge policy → `auto`; task questions and
approvals that still need a human reach the lead, and the lead may refuse or escalate but not approve.

**Decisions.** O2 rev 2 item 3; R1; R9; ADR 0012 P1 direction.

**Crates/files.**
- `crates/radian-permissions/src/profile.rs:89-216` — a task `Profile` with the hard-deny
  `CommandPrefix` rules (`git push`, merge/checkout against the lead workspace or user branch,
  `git reset --hard`, `git checkout -- <path>`, `git restore`, `git stash`). The deny matcher is
  conservative over every shell segment (`crates/radian-permissions/src/rule.rs:149-155`).
- A new `Approver` implementation in `crates/radian-host/src/pending.rs` (or `task.rs`): on an `ask`
  that reaches it, run the optional policy command (stdin = the pending request JSON, stdout =
  `allow|deny|ask`) with the task's sandbox policy; `ask`/timeout/nonzero → route to the lead.
- R9: task `ask_user` → a lead input (`Subagent` source, `task-<id> asks: …`); task approval that
  still asks → a lead input. Lead tools `answer_task`, `deny_task_approval`, `escalate_approval`
  (implemented with Slice 6) and a lead-side timeout (default 10 min → treat as deny). `PermissionResolved.approver = Policy` + the `policy` name from Slice 1.

**New public API.** `TaskApprover`/policy runner; the R9 message shapes. No `agent-core` change.

**Schema/fixtures.** Uses Slice 1's `policy` field; a fixture with `approver:"policy"` + `policy`.

**Testing.** Deterministic:
- A hard-deny command is still denied when the policy command says `allow`.
- Policy `ask`/timeout/nonzero lands in the lead's queue, not `auto`-allowed.
- `approval: on-ask` in a definition sends every `ask` to the lead.
- The policy decision writes `PermissionResolved` with `approver:"policy"` and the policy name.
- A lead `deny_task_approval` narrows; `escalate_approval` puts the request in the human queue with
  the lead's summary; the lead timeout behaves as deny.
- **Mutations:** hard-deny bypassable by the policy; policy `ask` auto-allowed; lead able to approve;
  timeout stuck.

**Size.** M.

**Depends on.** Slice 4 (needs task sessions and the shared queue wiring).

**Parallel with.** Slices 5–8 once Slice 4's interfaces are stable. (ADRs 0037 and 0036 already show
the shape: a System message injected/woken by lane.)

---

### Slice 6 — Lead tools, task registry behavior, heavy slots, stall

**Goal.** The O4 tool surface a lead calls, over the registry from Slice 4.

**Decisions.** O4 (`spawn_task`, `send_agent`, `wait_task`, `wait_any`, `task_status`, `steer_task`,
`cancel_task`, `list_tasks`, `remove_task`, `git_commit`), R3, R4, R5, R8; O8 (`max_tasks`,
`heavy_slots`, `stall_after_secs`); O13 (`spawn: true` capability gating, depth 2, process-wide cap).

**Crates/files.**
- New `crates/radian-host/src/lead_tools.rs`: one `ToolProvider` (like `AskTools`,
  `crates/radian-coding/src/ask.rs:538-583`) with `Exclusive` concurrency for
  `spawn_task`/`remove_task`/`git_commit` and `Shared` for the rest (`crates/agent-core/src/tool.rs`,
  the `ToolSpec.concurrency` field as used at `crates/radian-host/src/git_commit.rs:81`).
- Mount only when the session has the `spawn` capability (an agent-definition field, Slice 7; the
  main session defaults to it, O13) — wired in `crates/radian-host/src/session.rs:1098-1105` beside
  `git_commit`.
- `wait_task`/`wait_any`: race the task's driver-completion signal against `cx.cancellation`; on
  timeout return `still_running`, never cancel the task. Result fields filled by the runtime:
  last-committed assistant text (truncated, `TASK_SUMMARY_LIMIT_CHARS` ≈ 4000), outcome, usage,
  worktree path, HEAD commit, `git status --porcelain`, `no_commit`, each verify command's exit code
  and bounded tail (R2; uses Slice 9's verify runner or a no-op until then).
- `task_status`/`list_tasks`: read from the Slice 2 observer; `list_tasks` adds a bounded
  `git diff --stat`.
- Heavy queue: a process-level `Semaphore` (`heavy_slots`, default 2) gating verify commands and
  shell commands matching `heavy` patterns (O8/R4).
- Stall: track the last event ts and the last successful write ts per task; emit a `stalled` event
  after `stall_after_secs` (default 600). Where "event ts" comes from is an open question (the
  observer's tail poll vs the in-process projection).

**New public API.** `LeadTools`/`TaskTools` provider; a process `Orchestrator` holding the registry,
`max_tasks` queue, and `heavy_slots`. No `agent-core` change.

**Schema/fixtures.** `TaskFinished` written when a task ends (Slice 1's variant).

**Testing.** Deterministic:
- Source rule: after a user turn calls `spawn_task`/`wait_task`, a tool needing a session grant is
  still evaluated as `User`; a task's same call is `Subagent` (ADR 0030 test list; exercised with a
  scripted approver).
- `spawn_task` past `max_tasks` returns `queued` and the id, and runs FIFO when a slot frees.
- `wait_any` returns the first finisher; the others are untouched.
- A heavy command blocks a second heavy command until a slot frees.
- Stall emits once after the threshold.
- Results are bounded and state truncation.
- **Mutations:** the wait cancels the task on timeout; the queue exceeds `max_tasks`; the source rule
  raises the lead's turn to something non-User; the result is unbounded; `list_tasks` runs an
  unbounded diff.

**Size.** L.

**Depends on.** Slices 2, 4, 5.

**Parallel with.** Slices 7, 8, 9 (they consume its interfaces).

---

### Slice 7 — Agent definitions and named agents

**Goal.** Parse `~/.radian/agents/<n>.md` and `<ws>/.radian/agents/<n>.md` (frontmatter + prompt body)
into a `SessionSetup` mapping; named agents (one long session per name), `send_agent`, teams, depth 2.

**Decisions.** O11 (fields → `SessionSetup`), O12 (named vs task), O13 (teams, `spawn` capability,
depth limit), plus ADR 0036 (the body is layer 1; the built-in `default`/`task`/`lead` are
overridable by the same files).

**Crates/files.**
- New `crates/radian-host/src/agents/mod.rs`: a frontmatter parser (`name`, `description`, `model`,
  `thinking`, `profile`, `tools`, `mcp`, `isolation`, `skills`, `spawn`).
- `crates/radian-host/src/prompt.rs:16,28` — resolve a definition name to its prompt body; keep the
  built-ins as the fallback.
- `crates/radian-host/src/session.rs` — map fields to `SessionSetup` (`model` via ADR 0026,
  `thinking` via D11, `profile`, `tools` allow-list ∩ the tier's allowed tools, `mcp`, `isolation`,
  `role`).
- Named registry + `send_agent` (lead tool, Slice 6) + a `/send <name>` command (Slice 11) + session
  lookup by name.

**New public API.** `AgentDef` parse; a named-agent registry. No `agent-core` change.

**Schema/fixtures.** None, unless a `SessionSetup` snapshot is journaled.

**Testing.** Deterministic: frontmatter parse (all fields, unknown field ignored); a task agent
ignores its own `spawn: true`; depth 2; the `tools` allow-list can only narrow (ADR 0030 problem 7 =
A); `--system` replaces layer 1 only (ADR 0036/2); a named agent reuses its session across
`send_agent` calls. **Mutations:** task agent can spawn; allow-list widens a tier; the body replaces
the wrong layer.

**Size.** M.

**Depends on.** Slices 4, 6.

---

### Slice 8 — Process control port

**Goal.** The O6 Unix socket, JSON lines, source-fixed verbs; reconcile with `radian serve`/`radian mcp`
(ADR 0032).

**Decisions.** O6; ADR 0032's reserved subcommands.

**Crates/files.**
- `crates/radian-host/src/home.rs:46-136` — add `run_dir_for(root)` (`$RADIAN_HOME/run`, 0700).
- New `crates/radian-host/src/control.rs`: a `UnixListener`, per-connection JSON-lines handler over
  the async `Inbox` (never `block_on`), verbs `followup`/`steer`/`inject`/`cancel`/`list_sessions`/
  `spawn_task`/`wait_task`/`task_status`/`subscribe`. Source defaults to `Scheduler`; a client may
  declare `Subagent`/`Mcp`; `User`/`PermissionDecision` are refused (ADR 0012).
- Startup cleanup of stale `*.sock`; removal on exit; permissions 0700/0600.
- `crates/radian-tui/src/main.rs:299-351` — decide how `radian serve`/`radian mcp` relate (open
  question 6): a separate process that owns the socket, or the existing TUI process opening it.
- `subscribe` needs an event fan-out; the existing per-session `EventSink` is single-sink (`radian-tui`'s
  `ProjectionSink`), so a multicast wrapper is new.

**New public API.** `ControlServer`. No `agent-core` change (O6 says the `Inbox` four ops already
exist).

**Schema/fixtures.** None.

**Testing.** Deterministic:
- A `followup` declaring `User` is refused; `cancel` always goes; answering an approval over the port
  is refused.
- A slow subscriber is disconnected and never blocks the runtime.
- Socket permissions and stale-file cleanup.
- **Mutations:** accept `User`; block the runtime on a slow client; keep a stale socket.
- **Hand test:** one scripted client spawning a task and reading `subscribe` output.

**Size.** M.

**Depends on.** Slices 2, 4, 6.

---

### Slice 9 — Project orchestration config, trust, snapshot, sandboxed verify

**Goal.** `<ws>/.radian/orchestration.toml` (`max_tasks`, defaults, `worktree_root`, `verify`,
`policy`, `heavy`, `heavy_slots`, `stall_after_secs`, `merge_needs_human`), read behind the ADR 0031
gate, snapshotted at spawn, with verify/policy executed in the task's sandbox.

**Decisions.** O7, O8 (`heavy`/`heavy_slots`/`stall_after_secs`), R2 (verify), ADR 0031 T1 (trust
list), freeze-list item 6.

**Crates/files.**
- `crates/radian-host/src/config/mod.rs:429-451` — add `[orchestration]` (user level) with ADR 0008
  schema/migration rules; add config fixtures.
- `crates/radian-host/src/trust.rs:403-413` — extend `has_project_resources` to the new project
  resources (`orchestration.toml`, `agents/*.md`, `prompts/*.md`), per ADR 0031 T1.
- Snapshot at `spawn_task`: resolve the config from the **lead's** workspace, freeze it, and never
  read the task's own worktree `.radian/` (ADR 0030 O7 rev 2 item 4). The task session never loads its
  own project resources.
- Verify runner: run each command with the task's `SandboxPolicy` (workspace root = worktree, network
  off, same deny-read list), through the heavy queue.
- Hook points as control-port events only: `task_spawned`, `task_finished`, `stalled`, `before_merge`.

**New public API.** `OrchestrationConfig`; a `Snapshot`. No `agent-core` change.

**Schema/fixtures.** Config fixtures + `check-config-fixtures.sh`; a startup line listing the verify
and policy commands.

**Testing.** Deterministic:
- A task's own `.radian/orchestration.toml` does not affect its verify/policy/skills.
- Verify runs in the task sandbox: writing outside the workspace fails; reading `~/.radian/credentials/`
  fails.
- Untrusted workspace: no project config is read; the startup line says so.
- **Mutations:** read the task's own `.radian/`; run verify unsandboxed; load without trust.

**Size.** M.

**Depends on.** Slice 4 (snapshot at spawn), Slice 6 (verify reporting), ADR 0031 (done).

---

### Slice 10 — Lead built-in loop, integration worktree, dogfood

**Goal.** The thin host loop that surfaces task completions to the lead model, the default `lead.md`
and brief template, the integration worktree, and one real dogfood run.

**Decisions.** O1 (loop is a thin host loop, replaceable via the control port), O7 merge (rev 1 R6 /
rev 2 item 5), R6, R7 (prompt, mostly present), O11's backlog model; ADR 0036's `lead`
prompt.

**Crates/files.**
- `crates/radian-host/src/prompt.rs` + `prompts/lead.md` — the default lead loop instruction.
- Integration worktree `.radian-worktrees/integration` on `radian/integration`; `git_commit` there
  accepts only `radian/integration`; `before_merge` event; verify reruns there; the lead's delivery
  ends at `radian/integration` (the user branch is only moved by a human command the lead reports).
- `merge_needs_human` (default true) governs merging a task branch into `radian/integration`.

**New public API.** The loop. No `agent-core` change.

**Schema/fixtures.** `TaskFinished` + `GitCommitted` records from earlier slices.

**Testing.** The ADR 0030 metric: one real task that splits into 2–3 slices, run once with a lead +
task agents, compared with a single agent on wall-clock, input tokens, manual steers. The integration
worktree tests: `git_commit` there accepts only `radian/integration`; no tool can move the user branch.
**Mutations:** accept another branch in the integration worktree; auto-merge into the user branch.

**Size.** M.

**Depends on.** Slices 6, 8, 9.

---

### Slice 11 — TUI session switching, `/tasks`, task notifications

**Goal.** v0 TUI: task count, `TaskSpawned`/`TaskFinished`/`stalled` as transcript lines, `/tasks`
(with `cancel`/`kill`), and in-TUI session switching (named interactive, task read-only follow).

**Decisions.** O10, O12 (`cancel`/`kill`), ADR 0023 U6 (observer semantics), ADR 0034 H5.

**Crates/files.** `crates/radian-tui/src/app.rs` (post-Slice-3 view model), `command.rs` (`/tasks`),
`render.rs` (lines), `events.rs` (the projection sink feeding the lead transcript), `main.rs`
(switch wiring). Task sessions get no input box; approvals/questions stay in the shared queue with
origin.

**New public API.** None.

**Schema/fixtures.** None.

**Testing.** `pty`: switching to a task agent shows no input box; the shared approval queue still
arrives with origin; `/tasks cancel` and `/tasks kill` behave as ADR 0030 O12 (kill aborts the task's
pending by origin, keeps the worktree). **Mutations:** a task agent gets an input box; kill does not
abort pending; the count is wrong.

**Size.** M.

**Depends on.** Slices 2, 3, 6.

---

### Dependency graph (summary)

```
S1 schema ──▶ S4 task lifecycle ──▶ S5 approval ──┐
S2 observer ──────────────────────┐               ├─▶ S6 lead tools ──▶ S7 agents
S3 tui split ─────────────────────┤               │        │
                                  │               │        ├─▶ S8 control port
                                  └───────────────┘        ├─▶ S9 config/trust
                                                           └─▶ S10 lead loop
S2 + S3 + S6 ────────────────────────────────────────────────▶ S11 tui switching
```

Can run in parallel from day one: S1, S2, S3. After S1: S4. S5/S6 pair. S8/S9 parallel. S10/S11 last.

---

## 3. The first slice, in enough detail to brief a worker

Proposed first slice: **Slice 1, the journal/session schema groundwork.** Reasons: it is the strict
prerequisite of the core task-session slice; it is purely additive; it is fully deterministic; and it
exercises the fixture + `journal-selfcheck` machinery every later slice reuses. Slices 2 and 3 have no
dependency on it and can run in parallel. (If the owner would rather start by answering the
observer-vs-projection question, start Slice 2 instead; see open question 2.)

### Brief

**Worktree/branch.** The worker's own git worktree under `.radian-worktrees/<task>` and branch
`<prefix>/<task>` off `origin/main`. Work only there; never touch the main checkout or another
worktree. Commit on the branch; do not push, merge, rebase, amend, reset, or stash.

**Read first.**
- `docs/adr/0030-orchestration-primitives.md` O2, O5, and revision 2 item 3; `docs/adr/0006-session-jsonl-v1.md` §5.9, §6, §7.
- `crates/radian-session/src/runtime.rs` (the `RuntimeEvent` enum, `name()`, `critical()`, `RuntimeData`, `runtime_data()`), `crates/radian-session/src/writer.rs` (`ForkParent`, `SessionCreatedData`), `crates/radian-session/src/reader.rs` (`KNOWN_EVENTS`, the event match), `crates/radian-permissions/src/audit.rs` and `hooks.rs` (the `PermissionResolved` mirror + `runtime_event`).
- `docs/schema/check-fixtures.sh` and `docs/schema/fixtures/README.md`.

**Steps.**
1. Add `relation: String` to `ForkParent` (`writer.rs:34-38`), read with `#[serde(default = "…fork")]` so an old `parent` line still parses as `"fork"` (ADR 0006 §7a). `SessionCreatedData::forked` keeps writing `"fork"`; a `task` constructor is added but only used by Slice 4.
2. Add `RuntimeEvent::TaskSpawned { agent_session_id, worktree: Option<String>, label: Option<String> }` and `TaskFinished { agent_session_id, outcome: TurnOutcome }`; wire them through `name()`, `critical()` (`false`), `RuntimeData`, `runtime_data()`, and the reader's `KNOWN_EVENTS`/match. Keep their `data` field order exactly as the ADR and use `skip_serializing_if` for the optionals.
3. Add `policy: Option<String>` to `RuntimeEvent::PermissionResolved`, `PermissionResolvedData`, and the `radian-permissions` mirror; thread it through `runtime_event()`.
4. Add fixtures: a task-session pair demonstrating `parent.relation:"task"`, `TaskSpawned`, `TaskFinished`; a `PermissionResolved` with `approver:"policy"` and `policy`. Update `KNOWN_EVENTS` and the `EXPECT` dict in `check-fixtures.sh`, and `docs/schema/fixtures/README.md`.
5. Update ADR 0006 §5.9/§6 text if the `relation` note is not already there (the `policy` field is reserved).

**Limits.** Do not change `agent-core`. Do not add dependencies. Minimum Rust 1.88. Follow the
surrounding style (the manual `RuntimeData` enum, the fixture script). This slice emits no new events
from a producer; only the schema and its readers exist.

**Acceptance commands** (from `AGENTS.md`):
```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked --no-fail-fast
cargo test -p agent-core --locked --features journal-selfcheck --no-fail-fast
cargo clippy -p agent-core --all-targets --features journal-selfcheck -- -D warnings
bash docs/schema/check-fixtures.sh
bash docs/schema/check-config-fixtures.sh
```

**Test rule.** Each new test must fail when the behavior is broken. Mutation checks to perform and
name in the report:
- drop `relation` from the wire → the fork/task fixture test goes red;
- read an old `parent` with no `relation` as an error instead of defaulting → the v1-forward test goes red;
- mark `PermissionResolved` (or either new event) as critical → the reader test goes red;
- serialise `policy` when `None` → the fixture equality test goes red.

**Write-as-you-go.** Commit each finished piece (the field; the two events; the `policy` field;
fixtures) before starting the next. Keep a short progress note in the worktree and update it after
each step.

**Report.** Files changed; the design in a few lines; each command and its result; each mutation and
the test that caught it; what is unverified; the final commit id. Say plainly when unsure.

**Hand test.** None.

---

## 4. Open questions for the owner

Each with a recommendation. Numbered for the reply.

1. **Which slice first: the schema, the observer, or the TUI split?** — Recommendation: **the schema
   (Slice 1)**. It is the smallest strict prerequisite and the most mechanical. Slices 2 and 3 can run
   in parallel. If the owner wants the biggest architectural risk retired first, start Slice 4
   (task-session lifecycle) after Slice 1; it is the slice that proves or disproves the whole layout.

2. **How should a lead observe a task agent — the on-disk `JournalTail` facade (ADR 0023 U6) or the
   in-process `SessionProjection`?** Task agents are in the same process as the lead, so the
   projection is already available and cheaper. ADR 0030 O4 says `task_status` reads "from the
   observer, committed facts only", which is the on-disk semantics. Recommendation: build the Slice 2
   observer as the single **committed-facts** source and have the in-process task projection feed it,
   keeping the same code usable by the control port and by a future out-of-process task. Decide this
   in Slice 2; it does not block Slice 1. **This is a genuine design fork.**

3. **Task completion: `ToolOutput` (`wait_task`) or the ADR 0037 inbox notification path?** ADR 0030
   R3 rejected async notification because a `System`/follow-up message would lower the lead's turn
   source to `Subagent`. ADR 0037's `JobNotifier` (`crates/radian-host/src/job_notify.rs`) already
   does busy-`inject`/idle-`followup` for background jobs. Recommendation: **keep `ToolOutput`** for
   task results (O4, freeze-list 3) and reuse only the durable-event *shape* and the orphan-recovery
   rule from 0037, not its wake path. If the owner ever wants lead-side wake, it needs a new decision
   (a `System` input does not carry `User` authority).

4. **Does the control port live in the existing process (the TUI/`exec` opens the socket) or in a
   separate `radian serve` process?** ADR 0030 O6 describes a per-process socket at
   `$RADIAN_HOME/run/<pid>.sock`; ADR 0032 reserves `radian serve`/`radian mcp` "for ADR 0030's
   control port". These can coexist (a `serve` mode that opens the socket and runs the loop), but it
   must be stated. Recommendation: **the running process opens the socket**, and `radian serve`
   becomes the headless way to run a lead session with the socket and no TUI; `radian mcp` stays
   reserved for a later MCP bridge. Decide before Slice 8.

5. **`/mode lead` (ADR 0038) vs a `spawn: true` named agent (ADR 0030 O13): one mechanism or two?**
   ADR 0038 switches the role prompt in the current session; ADR 0030 says a lead is a named agent
   with the lead tools, and the human's main session is a `spawn: true` named `main`. Recommendation:
   treat `spawn: true` as **capability** (the tools) and `/mode lead` as a **prompt/role** switch;
   a session needs both to act as a lead, and `/mode lead` may auto-grant the capability on the main
   session. State the rule in Slice 7 and update ADR 0038 slice ③ accordingly.

6. **Where do `parent.relation` and `PermissionResolved.policy` land relative to ADR 0006's schema
   version?** Both are optional fields on existing events, so no version bump (ADR 0006 §7a).
   Recommendation: **no bump**; just update the §5.9/§6 text and the fixtures. Confirm.

7. **Project resources and trust: extend ADR 0031's trigger now or after the first real resource?**
   Today the trigger only knows `.radian/skills` (`crates/radian-host/src/trust.rs:403-413`), and no
   project resource is read. Slice 9 adds `orchestration.toml`, `agents/*.md`, `prompts/*.md`.
   Recommendation: **extend the trigger in Slice 9**, together with the first reads, so an untrusted
   workspace reliably refuses to load them. Confirm the 0031 record needs no schema change (it does
   not; it is a path→bool map).

8. **`git_commit`'s `main`/`master` refusal and ADR 0038 decision 4.** Today `git_commit` refuses
   `main`/`master` unconditionally (`crates/radian-host/src/git_commit.rs:333-341`), but ADR 0038 says
   that refusal should apply only to task agents (the interactive `general`/`lead` session should be
   able to commit on `main`). Recommendation: **make the refusal conditional on the session kind in
   Slice 4** (task agents keep it; interactive sessions drop it), and note it in ADR 0038's slice ④.
   This is a live conflict between today's code and a decided ADR.

9. **The `git_commit.rs` "ADR 0030" citation and `[git] allowed_filters`.** The code cites
   a revision 4 that does not exist in `docs/adr/0030-orchestration-primitives.md` (which ends at
   revision 3), yet `allowed_filters` is implemented and configured. Recommendation: **write the
   revision-4 text into ADR 0030** (the filter `check_attr`/`allowed_filters` rule) as a small doc fix,
   so the ADR and code agree. decision only.

10. **`stall` event source.** R5 needs "last event" and "last successful write" timestamps. The
    observer (Slice 2) can produce both from the journal; the in-process projection only has some of
    it. Recommendation: **derive both in the observer** and let the runtime read them, so the answer
    to open question 2 governs this too. Confirm.

11. **Does a task agent's `ask_user` remain advertised at all (R9 turns it into a lead input), and does
    the human ever see a task agent directly?** ADR 0030 says no: task questions go to the lead,
    task approvals that still need a human escalate through the lead, and the human queue holds only
    the lead's own items plus escalations. Recommendation: **follow rev 3**; the Slice 5 tests pin
    "task agent never appears to the human". Confirm, because it changes the pending-queue projection.

12. **`max_tasks` and `heavy_slots` scope across teams (O13):** process-wide, FIFO, no per-team quota.
    Recommendation: **follow O13** (process-wide `max_tasks` default 4, `heavy_slots` default 2, FIFO,
    no quotas). Confirm; quotas can come later.

### Conflicts between ADR 0030 and later ADRs / today's code, stated precisely

- **0030 vs 0038 (decision 4):** `git_commit`'s `main`/`master` refusal is unconditional in
  `crates/radian-host/src/git_commit.rs:333-341`, but 0038 makes it task-agent-only. Resolve in
  Slice 4 / 0038 slice ④ (open question 8).
- **0030 O6 vs 0032:** 0030's control port is a per-process socket; 0032 reserves `radian serve`/`radian
  mcp` "for 0030's control port". No mechanism ties the two yet (open question 4).
- **0030 O13 vs 0038 decision 1:** "lead is a `spawn: true` named agent" and "`/mode lead` switches the
  role prompt" are two paths; the relationship is unstated (open question 5).
- **0030 O4/R3 vs 0037:** task results by `ToolOutput` and background-job completion by inbox
  notification are two shapes for "work finished"; 0030 chose `ToolOutput` deliberately, 0037 built
  the inbox path. They do not conflict, but task completion must not accidentally reuse the wake path
  (open question 3).
- **0030 code vs its own doc:** `git_commit.rs` and `[git] allowed_filters` cite "ADR 0030"
  that the ADR file does not contain (open question 9).
- **Stale code comments:** `crates/radian-session/src/runtime.rs:117-118` says `ContextCompacted`,
  `Rewound`, `SessionForked` and `PermissionResolved` "will be added when their producers exist", but
  all four are present and produced. A doc-comment fix, not a slice.
- **0030 background vs code:** the ADR's background cites line numbers from `main 47c5b37` for a
  two-binary layout (`agent-cli/src/main.rs`, `radian-tui/src/main.rs`); since then ADR 0032 landed
  (`agent-cli` is a library, `radian exec` exists). The background is stale on that point; nothing in
  the decisions depends on it.

### Unverified in code (do not build on these without checking)

- ADR 0030 background's claim that a task-agent's shell tool uses its own OS thread and does not
  occupy a runtime thread (`crates/radian-coding/src/shell.rs`); not re-read here.
- The memory single-writer `flock` "Locked" behavior across two sessions (`radian-memory/src/log.rs`);
  taken from the ADR, not verified.
- Whether two `open_session` calls can always run concurrently in one process without contending on a
  shared resource; the per-session state is separate (`crates/radian-host/src/session.rs:456-494`),
  but this is a concurrency claim and is left to a deterministic Slice 4 test.
- Whether `rmcp`'s server-side feature is available for `radian mcp` (ADR 0030 "before implementing").
- ADR 0026's provider descriptor grammar accepting `anthropic/claude-sonnet-5-5` strings directly
  (ADR 0030 "before implementing"); Slice 7 depends on it.
