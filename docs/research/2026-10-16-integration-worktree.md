# The integration worktree and the merge path (ADR 0030 slice 10, step 1: design note)

- Date: 2026-10-16
- Branch/HEAD the facts were read on: `oc/orch-s10`, from `origin/main` (`d608272`). Worktree
  `<worktree>/orch-s10`.
- Purpose: design where the lead's integration worktree lives and how a task branch reaches
  `radian/integration`, before any of it is written. This is a document; it changes no code and no
  ADR.
- Audience: the owner (for the decisions at the end) and whoever builds the slice.

## How to read this

- `path:line` marks a fact **read in the source on this branch**. `(ADR NNNN)` means the fact
  comes from an ADR, not from code. `(unverified)` marks a claim this note did not check.
- Decision ids `O1`–`O13`, `R1`–`R10` are ADR 0030's; file names are ADR 0030's.
- Slice numbers are the implementation plan's
  (`docs/research/2026-10-15-adr-0030-implementation-plan.md`), not ADR 0030's table.

### A premise in the step's brief that is **not** true of the code today

The step brief says code from **slice 9** exists ("verify from slice 9"; "the `before_merge`
control-port event (slice 9 defined it)"). It does not. Verified on this branch:

- There is no `orchestration.toml` reader, no `OrchestrationConfig`, no `merge_needs_human`, and no
  verify runner anywhere in `crates/` (grep for `orchestration.toml`, `OrchestrationConfig`,
  `run_verify` = 0 hits; config has no `[orchestration]` section).
- The verify runner is explicitly still missing: it "will call `HeavySlots::acquire` … that runner
  does not exist yet" (`crates/radian-coding/src/heavy.rs:19-21`), and `lead_tools.rs` lists it as
  "what is not here (part b2): `send_agent`, `steer_task` and the verify runner"
  (`crates/radian-host/src/lead_tools.rs:32`).
- `before_merge` appears only in ADRs and the plan, never in code
  (`docs/adr/0030-orchestration-primitives.md:97`;
  `docs/research/2026-10-15-adr-0030-implementation-plan.md:437`).
- Slice 9's own plan entry confirms it is unbuilt: "Slice 9 — Project orchestration config, trust,
  snapshot, sandboxed verify" (`…implementation-plan.md:418-453`).

So this note designs **against slice 9 as a dependency** (its `verify` runner and its
`before_merge` hook-point event), not against code that exists. Where a design item depends on
slice 9, it says so. The `merge_needs_human` and `verify` names below are slice 9's config keys
(`…implementation-plan.md:420-422`), not today's.

---

## What is true today (verified)

These are the facts the design must fit. Every line was read on this branch.

| Fact | Where |
|---|---|
| A task agent's worktree root is `~/.radian/worktrees/<repo-key>`, **outside** the workspace; `<repo-key>` is the repo name plus a hash of the canonical common dir | `crates/radian-host/src/task.rs:7-9`, `:1722-1727`, `:1731-1761` |
| Each level of that root is opened with `openat(O_NOFOLLOW\|O_DIRECTORY)` and created 0700; a symlink at any level refuses the spawn | `crates/radian-host/src/task.rs:1838-1880`, `:2001-2034` |
| The task worktree is created host-side with `git worktree add -b radian/<id> <path>` (no start point) and the branch is `radian/<id>` | `crates/radian-host/src/task.rs:1142-1150`, `:1805-1836` |
| Git always runs through the one host helper, which carries `core.fsmonitor=false`, `core.hooksPath=/dev/null`, `core.attributesFile=/dev/null` and empties every configured filter command | `crates/radian-coding/src/host_git.rs:35-64`; enforced by `crates/radian-coding/tests/host_git.rs:51-88` |
| The task sandbox is a `workspace` policy, network **off**, `.git` read-only, carrying the lead's deny-read list | `crates/radian-host/src/task.rs:1670-1688` |
| A session's `git_commit` is pinned at session open (`RepoPin`: git-dir, common-dir, work-tree, and "did the workspace have a `.git`") and every git call re-resolves and refuses a swap | `crates/radian-host/src/session.rs:1438-1451`, `crates/radian-host/src/git_commit.rs:113-175` |
| `git_commit` refuses a detached HEAD always, `main`/`master` only when `refuse_main`, a configured protected branch always, and any branch but `allowed_branch` when one is set | `crates/radian-host/src/git_commit.rs:399-436` |
| The interactive host session is `task_agent=false`, `allowed_branch=None`, `git_commit_requires_branch=false` → its `git_commit` mounts with `refuse_main=false` and can commit on the branch the user has checked out | `crates/radian-host/src/startup.rs:313-331`, `crates/radian-host/src/session.rs:1438-1451`, `crates/radian-host/src/git_commit.rs:418-419` |
| A task session is `task_agent=true`, `allowed_branch=Some("radian/<id>")`, `git_commit_requires_branch=true` → it commits only to its own branch | `crates/radian-host/src/task.rs:18-19`, `:1538-1543` |
| `git_commit` stages with plumbing that runs no filter (`hash-object --no-filters` + `update-index`) and commits with `git commit --no-verify` | `crates/radian-host/src/git_commit.rs:19-23`, `:533-542`, `:1024-1033` |
| The end-of-task commit is a host-side `git_commit::commit(worktree, allowed_branch, …, refuse_main=true, require_branch=true)` with the fixed message `radian: end of task <id> (<outcome>)`, awaitable | `crates/radian-host/src/task.rs:2120-2151`, `:2249-2326` |
| The lead tool surface is 12 names (`spawn_task`, `send_agent`, `wait_task`, `wait_any`, `wait_agent`, `answer_task`, `deny_task_approval`, `escalate_approval`, `task_status`, `list_tasks`, `cancel_task`, `remove_task`); **no integration/merge tool** | `crates/radian-host/src/lead_tools.rs:90-106` |
| `spawn_task`/`remove_task` are `Exclusive`; the rest `Shared` | `crates/radian-host/src/lead_tools.rs:24`, `:199`, `:372` |
| The lean lead's `git_commit` tool spec is `ToolConcurrency::Exclusive` | `crates/radian-host/src/git_commit.rs:95` |
| The control port exposes `followup`/`steer`/`inject`/`cancel`/`list_sessions`/`spawn_task`/`wait_task`/`task_status`/`subscribe`, never accepts `User`, and cannot answer an approval | `crates/radian-host/src/control.rs:7-15`, `:328-345`, `:831-835` |
| A task's `ask` reaches its own `TaskApprover` (hard-deny → policy → auto/pending), only because the host sets `set_task_approver(true)` for a task session; every other session keeps "a non-human turn's `ask` is refused" | ADR 0030 item 1 (`docs/adr/0030-orchestration-primitives.md:451-453`); `crates/radian-permissions/src/hooks.rs:124`, `crates/radian-host/src/session.rs:1397` |
| The process heavy gate exists (`DEFAULT_HEAVY_SLOTS=2`, patterns `cargo build`/`test`/`clippy`); verify will draw on it | `crates/radian-coding/src/heavy.rs:19-43` |

The lead's sandbox **can read** `~/.radian/worktrees/...`: that directory is not in
`DEFAULT_DENY_READ` (`crates/radian-sandbox/src/policy.rs:105-117`) and the host deny-read list
built for the lead adds only the sessions, memory, credentials, trust and access directories
(`crates/radian-tui/src/cli.rs:1170-1190`; `crates/radian-host/src/task_host.rs:83-87`). It cannot
**write** there: the lead's writable root is the workspace, and the worktrees root is outside it
(ADR 0039 decision 4, `docs/adr/0039-host-file-access-hardening.md:26`).

---

## 1. Where the integration worktree lives, and when it is created

**Recommendation (B): outside the workspace, next to the task worktrees.**

```
<radian-home>/worktrees/<repo-key>/integration     on branch radian/integration
```

created host-side with the same `vet_worktree_root` per-level `O_NOFOLLOW` check the task root
uses (`crates/radian-host/src/task.rs:1838-1880`), `<repo-key>` from the same `repo_key` helper
(`…/task.rs:1731-1761`).

Why B:

- It is the ADR 0039 decision 4 root (`docs/adr/0039-host-file-access-hardening.md:26`): 0700,
  symlink-checked per level, **not writable by any model**. The host is about to run `git merge`
  and `git commit` there; ADR 0039's whole thesis is that the host must not operate by path inside
  a model-writable location (`docs/adr/0039-host-file-access-hardening.md:8`). Putting the
  integration worktree in the workspace puts the host's merge back inside the model's writable
  root.
- The lead can still see the result: reading `~/.radian/worktrees/...` is allowed (not denied;
  see the facts table), and the primary read is by git (`radian/integration`, `git_diff`), not by
  path.
- It supersedes the two places that name an in-workspace location:
  ADR 0030 O7 "`<workspace>/.radian-worktrees/integration`"
  (`docs/adr/0030-orchestration-primitives.md:98`) and ADR 0039 decision 4's sentence that
  "`.radian-worktrees/` is still used for … integration worktrees"
  (`docs/adr/0039-host-file-access-hardening.md:26`). Those need an ADR edit.

**Alternative (A): keep it in the workspace** at `<workspace>/.radian-worktrees/integration` (ADR
0030 O7 literal; ADR 0039 decision 4 keeps the directory for manual/integration worktrees). The
lead's sandbox can write the working tree there (root = the workspace) but not `.git` (read-only,
ADR 0038 decision 5), so every merge and commit would still be host-side. Drawbacks: the host
operates in a model-writable tree; the lead's shell can corrupt the integration checkout; the two
existing skip lists already special-case the directory (`crates/radian-coding/src/search.rs:22-26`,
`crates/radian-host/src/skills/mod.rs:27-30`), which show it is treated as not-for-reading. If A is
chosen, the note's other items are unchanged except the path.

**Base and lifecycle.**

- Create **lazily on the first `integrate_task`**, not at lead start: a lead that never integrates
  leaves nothing behind, and the base is the most recent fact.
- Base = `git rev-parse HEAD` in the **lead's workspace at that moment** (`git_head`,
  `crates/radian-host/src/task.rs:2078-2091`), i.e. the branch point the first task worktree was
  also created from (`TaskRecord::start_commit`, `crates/radian-host/src/task.rs:243-246`). It is
  **not** re-pointed afterwards.
- If `<root>/integration` or the branch `radian/integration` already exists, the tool **refuses
  and reports it**: ADR 0030 O7 allows per-lead names
  ("按 lead 名可再分", `docs/adr/0030-orchestration-primitives.md:98`), so a second concurrent lead
  should use `integration-<lead>` / `radian/integration-<lead>` rather than share or reset one.
  Reset/reuse would move a branch a previous run may already have handed to the human; refuse is
  the ADR 0020 "never discard user data" posture (`docs/adr/0030-orchestration-primitives.md:50`).
- The worktree is **kept** when the lead exits (ADR 0030 O3/O12 keep worktrees; no auto-delete).

**Separate open question: the task branch point.** Today
`git worktree add -b radian/<id> <path>` has no start point
(`crates/radian-host/src/task.rs:1825-1827`), so a task branches from the lead's HEAD, **not** from
`radian/integration`. For a lead running slices in sequence, task N+1 will not see task N's merged
work. Either accept that (each brief must say "based on `radian/integration`", and merging becomes
order-dependent), or give `spawn_task` a `base` and default it to `radian/integration` once the
integration worktree exists. This note does not decide it; it records that it changes integration
semantics.

---

## 2. How a task branch gets merged into `radian/integration`

**Recommendation: one host-side lead tool, `integrate_task { id }`.**

- It is a lead tool (`radian-host/src/lead_tools.rs`, `LEAD_TOOL_NAMES`), `Exclusive` like
  `spawn_task`/`remove_task` (`crates/radian-host/src/lead_tools.rs:24`), mounted from the start
  and gated by mode like every other lead tool (ADR 0030 item 5,
  `docs/adr/0030-orchestration-primitives.md:412`; `crates/radian-host/src/session.rs:1461-1470`).
- It resolves the task from the registry and takes `TaskRecord::branch`
  (`crates/radian-host/src/task.rs:229`); a `shared` task has `branch = None` and is refused (it was
  never on a branch, ADR 0030 O3).
- It ensures the integration worktree (§1), then in it runs
  `git merge --no-ff radian/<id>` built from `radian_coding::host_git::host_git_command(integration)`
  (`crates/radian-coding/src/host_git.rs:59-64`). That helper is what makes "no hooks, no filters"
  true for the merge: `core.hooksPath=/dev/null` disables `pre-merge-commit`/`commit-msg`/
  `post-merge`, and every filter command is emptied so a smudge/clean driver cannot run while the
  merge updates the working tree (`crates/radian-coding/src/host_git.rs:23-54`; ADR 0039 decision 3,
  `docs/adr/0039-host-file-access-hardening.md:24`).
- **The commit of the merge result.** `git merge --no-ff` creates the merge commit itself. To keep
  the ADR 0030 O7 sentence "commit the merge result with `git_commit`"
  (`docs/adr/0030-orchestration-primitives.md:98`) and the ADR test "`git_commit` in the integration
  worktree accepts only `radian/integration`" (`docs/adr/0030-orchestration-primitives.md:248`)
  literally true, the tool can instead run `git merge --no-ff --no-commit radian/<id>` and then
  commit through the tested path `git_commit::commit(integration, pin, msg, &[], Some("radian/integration"),
  protected, filters, true, true)` — the same function the end-of-task commit uses
  (`crates/radian-host/src/git_commit.rs:379-556`, `crates/radian-host/src/task.rs:2139-2149`).
  The `allowed_branch` check is what accepts only `radian/integration`
  (`crates/radian-host/src/git_commit.rs:429-436`). decision 4 picks between
  "merge commits itself" and "`--no-commit` + the `git_commit` path"; the latter is the one that
  makes the ADR's wording and its named test exact.
- **Alternative rejected:** give the lead's own session a `git_commit` that operates on the
  integration worktree. A session's `git_commit` is pinned at `open_session` to that session's
  workspace (`crates/radian-host/src/session.rs:1438-1451`) and the RepoPin is re-resolved on every
  call (`crates/radian-host/src/git_commit.rs:147-155`); re-pointing it at runtime would mean
  rebuilding the tool set, which ADR 0030 item 5 forbids (tools never change mid-session,
  `docs/adr/0030-orchestration-primitives.md:412`). So the merge is a host function behind one tool,
  not a re-purposed session tool.

**What `merge_needs_human = true` (default) means concretely.**

- It is the **permission** of `integrate_task`: with `true` the call resolves to `ask`, so it lands
  in the **human's** pending queue with the lead's session as the origin; with `false` it is
  `allow` (ADR 0030 O7: "`merge_needs_human = false` only opens task→`radian/integration`, never the
  user branch", `docs/adr/0030-orchestration-primitives.md:98`).
- Source rules: ADR 0012's `ask` is answered only by a human, and the lead's turn source is what
  decides whether an `ask` can be raised at all. A human-initiated lead turn is `User`, so the ask is
  answerable (ADR 0030 item 1, `docs/adr/0030-orchestration-primitives.md:431`). A lead
  driven over the control port (`Scheduler`/`Subagent`) cannot obtain an approval — a non-human
  turn's `ask` is refused (`crates/radian-permissions/src/hooks.rs`, cited by ADR 0030
  item 3 and revision 6; `docs/adr/0030-orchestration-primitives.md:410`, `:426-427`) — so an
  external lead with `merge_needs_human = true` gets a refusal, and must either set it `false` or
  have the human run the merge. The control port deliberately has no "answer an approval" verb
  (`crates/radian-host/src/control.rs:13-15`).
- There is no "always ask" rule kind distinct from `merge_needs_human`; the config value is the
  switch (ADR 0030 item 5 records exactly this correction,
  `docs/adr/0030-orchestration-primitives.md:351`).

---

## 3. What happens on a merge conflict

**Recommendation: abort and report.**

- On a non-zero merge exit with conflicts, run `git merge --abort` in the integration worktree,
  then return to the lead, as the tool result: the conflicted paths
  (`git diff --name-only --diff-filter=U`), `git status --porcelain`, and git's stderr tail. No
  verify runs; no integration commit.
- Why abort rather than leave it: a half-finished merge in a **shared, single** integration
  worktree blocks every later `integrate_task` and leaves `MERGE_HEAD` behind; and the lead cannot
  write there to resolve it (its shell is sandboxed to the workspace and `.git` is read-only, §1).
- What the lead can then do: report the conflict to the human, or spawn a **resolver task** that
  starts from `radian/integration` and merges `radian/<id>` itself. That resolver needs the
  branch-point change of §1 (a `base` on `spawn_task`); without it a resolver task branches from the
  lead's HEAD and cannot reproduce the conflict. decision 6.

**Other options, listed:**

- **Leave it in progress** and let a resolver work directly in the integration worktree. Rejected:
  the resolver would have to be a new kind of session rooted at a host-owned, model-unwritable
  directory, and the worktree is shared across merges.
- **Auto-resolve** (favor one side). Rejected: silently choosing a side is exactly the class of
  decision ADR 0030 keeps for the human (ADR 0030 "accepted costs": "the user branch always needs
  one human action", `docs/adr/0030-orchestration-primitives.md:215`).
- **Reset the task's branch onto the integration branch** (`git rebase`). Rejected: rewrite of
  history; the hard-deny list is built to keep the agent away from it
  (`docs/adr/0030-orchestration-primitives.md:41`).

---

## 4. Integration verify

- After a **successful** merge, rerun the snapshot's `verify` commands (slice 9;
  `…implementation-plan.md:435-436`) **in a sandbox rooted at the integration worktree**, built by
  the same `task_sandbox_policy` (`crates/radian-host/src/task.rs:1670-1676`): `workspace` tier,
  network off, `.git` read-only, the lead's deny-read list. This is the only way "verify reruns
  there" (`…implementation-plan.md:469`) matches task verify's guarantees — verify must not run on
  the host, because it is project-supplied executable configuration behind the ADR 0031 trust gate
  (ADR 0030 O7, `docs/adr/0030-orchestration-primitives.md:96`).
- Commands run through the process heavy gate (`crates/radian-coding/src/heavy.rs:19-43`), and each
  command's exit code and a bounded output tail are returned to the lead (ADR 0030 O4 R2,
  `docs/adr/0030-orchestration-primitives.md:77`).
- A failed integration verify is **not** auto-reverted. The committed merge stays on
  `radian/integration`, and the failure is a fact in the tool result; the lead decides (report, or
  a follow-up task). Auto-revert would move `radian/integration` behind a commit the human may have
  already seen.
- Dependency: slice 9's runner does not exist (`crates/radian-coding/src/heavy.rs:19-21`); until it
  does, the integration verify step is a no-op that says so in its result.

---

## 5. The `before_merge` control-port event

- `before_merge` is one of ADR 0030 O7's four hook points — `task_spawned`, `task_finished`,
  `stalled`, `before_merge` — and exists **only** as a control-port event; wiring it to a shell hook
  is a separate, unwritten ADR (`docs/adr/0030-orchestration-primitives.md:97`). Slice 9 owns the
  four (`…implementation-plan.md:437`); it is not in code today.
- **When it fires (recommended):** inside `integrate_task`, after the permission decision
  (`ask` answered, or `allow`) and after the integration worktree is ready, immediately **before**
  the `git merge` runs — so a subscriber can observe the exact pre-merge state, and a future hook
  can veto by other means. It fires once per merge **attempt**, i.e. also on an attempt that then
  conflicts.
- **Shape:** a new `#[non_exhaustive]` `RuntimeEvent` variant (ADR 0006 §7b forward-compat,
  `docs/adr/0030-orchestration-primitives.md:19`), `critical = false` (audit, not recovery, like
  `TaskSpawned`/`TaskFinished`, ADR 0030 O5). Proposed payload: `task_id`,
  `branch` (`radian/<id>`), `integration_branch` (`radian/integration`),
  `integration_head` (the pre-merge HEAD). It is written to the **lead's** journal and reaches
  control-port subscribers through the existing `RuntimeFanout`
  (`crates/radian-host/src/control.rs:169-178`) and its `runtime_event_json`
  (`crates/radian-host/src/control.rs:622-639`). Payload and the payload/`schema` fixtures are an
  decision (decision 7).

---

## 6. What "the thin host loop" is after revision 5 item 3

**It is not host code. It is the default `lead.md`, plus the model's own `wait_*` calls.**

- ADR 0030 O1 described a "thin built-in loop" that the host runs
  (`docs/adr/0030-orchestration-primitives.md:32`). Revision 5 item 3 removes the only mechanism
  such a loop could have used: a completion is surfaced **only** by `wait_task`/`wait_any`, never by
  `followup` and never by `inject`, because either would lower the lead's in-flight turn's source and
  refuse its later `ask`s (`docs/adr/0030-orchestration-primitives.md:410`). Revision 6 extends the
  same rule to task questions and still-ask approvals: they arrive as `wait_*` returns, not as inbox
  input (`docs/adr/0030-orchestration-primitives.md:429-435`). The code already matches: the wait
  tools surface `question`/`approval`/`finished` from the wait
  (`crates/radian-host/src/lead_tools.rs:16-21`).
- So after revision 5/6 the "loop" is: the default `lead.md` telling the model
  **spawn → `wait_any` → review → `integrate_task` → report**, and the model issuing those tool
  calls inside its own `User` turn. There is no host-side subscriber that pushes task results into
  the lead, and adding one would violate revision 5 item 3. Slice 10 therefore adds **no loop code**;
  its loop deliverable is prompt wording — extend `crates/radian-host/prompts/lead.md` (its
  "Integration" section already states "Your delivery ends at the integration branch … report the
  command", `crates/radian-host/prompts/lead.md:22-23`) to name `integrate_task` and the merge
  report, and keep the brief template's `task.md` report format (R7,
  `crates/radian-host/prompts/task.md:30-33`).

---

## 7. How the lead reports the final human command, and why no tool moves the user branch

- The report is a line in the lead's final message: `git merge --ff-only radian/integration` (or
  `git merge radian/integration`). The lead never runs it (ADR 0030 O7,
  `docs/adr/0030-orchestration-primitives.md:98`; freeze-list 11, `:194`), and `lead.md` already says
  so (`crates/radian-host/prompts/lead.md:23`).
- **Structural guarantees** (what the tests in §8 must pin):
  1. The integration worktree has `radian/integration` checked out, and a git worktree can only
     update the branch checked out **in it**; the user's branch is checked out in the user's own
     worktree.
  2. `git push . radian/integration` from the integration worktree (the one way a worktree could try
     to update the user's checkout) is rejected by `receive.denyCurrentBranch` (ADR-cited,
     `docs/adr/0030-orchestration-primitives.md:98`; not re-verified here). *(unverified: the git
     default on this machine.)*
  3. No host tool switches branches or pushes: `git_commit` "only adds and commits: no push, no
     branch, no amend, no rebase" (`crates/radian-host/src/git_commit.rs:34`), and the merge tool
     only ever merges the task branch into `radian/integration`.
  4. An integration commit is refused unless HEAD is exactly `radian/integration`:
     `git_commit.rs:429-436`, with `require_branch=true`.
  5. A task agent can never commit off `radian/<id>` (`crates/radian-host/src/task.rs:1538-1543`,
     `crates/radian-host/src/git_commit.rs:407-436`), and the task hard-deny list blocks
     `git push`, merges into the lead's worktree/user branch, and `git checkout`
     (`docs/adr/0030-orchestration-primitives.md:41`).
- **One verified caveat the tests must respect.** The lead is the human's own session in the TUI,
  and that session's `git_commit` mounts **unrestricted** (`task_agent=false`, `allowed_branch=None`,
  `git_commit_requires_branch=false` → `refuse_main=false`,
  `crates/radian-host/src/startup.rs:313-331`; `crates/radian-host/src/session.rs:1438-1446`;
  `crates/radian-host/src/git_commit.rs:418-419`). So the literal ADR claim "the lead has no tool
  that can update the user branch" (`docs/adr/0030-orchestration-primitives.md:248`) is **false for a
  model-issued `git_commit` on the user's checked-out branch**, even in `lead` mode: that is
  ADR 0038 decision 4's deliberate "the interactive session may commit on `main`" behaviour
  (`crates/radian-host/src/git_commit.rs:30-33`, `:233-240`). The guarantee that *is* structural is
  narrower: **no orchestration/integration tool can apply the integration result to the user
  branch**; the user branch is moved only by the human. decision 8: leave the guarantee worded
  that way, or mode-scope `git_commit` so `lead` mode refuses a commit on the user's checked-out
  branch (a behaviour change to ADR 0038 decision 4).

---

## 8. Tests and mutations each part needs

ADR 0030's own list has the two named cases; the rest follow from the parts above. Each test must
fail when the behaviour is broken (mutation), and the mutation is named.

| Part | Test | Mutation (must fail) |
|---|---|---|
| Location/vetting (§1) | Integration root is `<radian-home>/worktrees/<repo-key>/integration`, 0700; a symlink at any level refuses creation (mirror `crates/radian-host/tests/task_root.rs:199-238`) | accept a symlinked level; put the root back in the workspace |
| Base/lifecycle (§1) | First `integrate_task` creates the worktree at the lead's HEAD; a second lead with the branch already present refuses (or uses `integration-<lead>`) | reset an existing integration branch silently |
| Merge path (§2) | `git_commit`/`integrate` in the integration worktree accepts only `radian/integration`; a task branch merges with `--no-ff` and no hook/filter runs (a `pre-merge-commit` that leaves a marker never fires; a `filter.<n>.clean` marker never fires) | accept another branch in the integration worktree; run a raw `git` command instead of the host helper (covered by `crates/radian-coding/tests/host_git.rs`) |
| `merge_needs_human` (§2) | with the default `true`, `integrate_task` produces a human approval with the lead as origin; with `false` it does not; a non-`User` lead turn gets a refusal, not an auto-allow | auto-merge without the ask; allow a non-`User` turn to bypass it |
| Conflict (§3) | a conflicting merge returns the conflicted paths and leaves no `MERGE_HEAD` and a clean worktree | leave the merge in progress |
| Integration verify (§4) | verify runs in the integration sandbox: a write outside the worktree fails; reading `~/.radian/credentials/` fails; a failing command is reported with its exit code and tail | run verify unsandboxed; skip verify; auto-revert the merge |
| `before_merge` (§5) | the event is written once, before the merge, with the task/branch/head payload, and is visible on the control-port `subscribe` stream (a fixture, `docs/schema`) | fire after the merge; not fire; wrong payload |
| User-branch guarantee (§7) | no tool updates the user branch; the tool result tells the lead the human command; a `git push .` attempt is refused | auto-merge into the user branch; run the reported command |
| Loop (§6) | a task's completion/question arrives as a `wait_*` return and **not** in the lead's inbox; the lead's turn source stays `User` and its own `ask` still reaches the human queue (ADR 0030 acceptance, `docs/adr/0030-orchestration-primitives.md:439-441`) | inject a completion into the lead's inbox; lower the lead's turn source on a task event |
| Metric (§, slice 10) | one real task split into 2–3 slices, run once with a lead + task agents, compared with a single agent on wall-clock, input tokens, manual steers (`…implementation-plan.md:477-479`) | — (a measurement, not a mutation) |

Contiguity: these build on slice 9's runner and config, so they land after it unless the note's
no-op verify fallback (§4) is used first.

---

## Who must decide

1. **Integration worktree location.** B: `<radian-home>/worktrees/<repo-key>/integration` (this
   note's recommendation; needs the ADR 0039 decision 4 sentence and ADR 0030 O7 changed) vs A:
   `<workspace>/.radian-worktrees/integration` (ADR 0030 O7 as written). Whichever, an ADR edit is
   needed.
2. **Lifecycle when `radian/integration` already exists.** Refuse (recommended) vs reuse/ff vs
   reset; and whether per-lead names (`radian/integration-<lead>`) are the default.
3. **The task branch point.** Keep branching task worktrees from the lead's HEAD (today,
   `crates/radian-host/src/task.rs:1825-1827`) or add a `base` to `spawn_task` and default it to
   `radian/integration` once it exists. This changes what "a slice" sees.
4. **Merge mechanism.** One tool that lets `git merge --no-ff` create the commit vs `--no-commit`
   then the `git_commit` path restricted to `radian/integration` (the latter makes ADR 0030 O7's
   wording and the ADR test literal). Also the tool's name (`integrate_task` proposed).
5. **`merge_needs_human` wiring.** A permission `ask` on `integrate_task` (recommended) and what an
   external, non-`User` lead does when it is `true` (refused vs a documented "set it false").
6. **Conflict policy.** Abort and report (recommended) vs leave in progress vs spawn a resolver;
   and, if a resolver, the `base` of §1 so it can start from `radian/integration`.
7. **`before_merge` payload and firing point.** The fields (§5), `critical = false`, and whether it
   fires on a conflicting attempt too.
8. **The user-branch guarantee's wording.** Leave ADR 0030's "no tool can move the user branch"
   narrowed to "no orchestration tool", or mode-scope `git_commit` so `lead` mode refuses a commit
   on the user's checked-out branch (changes ADR 0038 decision 4).
9. **Does `integrate_task` count against `max_tasks`/heavy slots?** (This note says no: it is a
   short host call, and the heavy gate is for verify/shell, `crates/radian-coding/src/heavy.rs:6-8`.)
10. **Is `integrate_task` also a control-port verb** (so an external lead can integrate), with the
    `merge_needs_human`/no-approval-over-the-port consequence of §2?

## Not verified here / not in the code yet

- Slice 9 in full: `[orchestration]` config, `merge_needs_human`, the verify runner, and the
  `before_merge` hook-point event (`…implementation-plan.md:418-437`; `crates/radian-coding/src/heavy.rs:19-21`;
  `crates/radian-host/src/lead_tools.rs:32`).
- git's `receive.denyCurrentBranch` default on this machine (ADR-cited from
  `docs/adr/0030-orchestration-primitives.md:98`, not re-tested).
- The exact `git merge` behaviour under the host helper's overrides (the overrides are verified in
  `crates/radian-coding/src/host_git.rs:35-64`; a merge actually exercised under them was not).
- Any concurrency claim about two leads integrating at once (no such code exists).
