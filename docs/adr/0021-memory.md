# ADR 0021: Memory

Status: accepted

## Invariants

1. **Core never knows Memory.** Core never depends on memory types, scopes, or backends. With Memory off, Core, compaction, and session resume behave fully unchanged.
2. **Memory never changes session facts.** Memory never modifies session journals, compaction records, or their recovery semantics. A memory failure must never corrupt a session or block a dialogue turn.
3. **The host judges sources, and sources never rewrite.** A memory record source (user, import; old logs also carry agent-tool-written records) is fixed by the host at write time. The code never trusts a model-claimed source. An original source never changes after write (confirmation records separately).
4. **Memory is low-trust data.** Memory injected into the model is labeled data, never instructions. It never bypasses tool permissions, sandboxes, tiers, or user instructions.
5. **The sandbox cannot read memory**: under the `workspace` tier the memory store is unreadable. Only Radian injection, `memory_search`, and user commands reach it. The sandbox denylist enforces it (ADR 0020). The escape matrix verifies it.
6. **User records win.** The agent never modifies, archives, deletes, or overwrites user-created records.

## Decisions

### D1 Scope

- Only **Global** (user scope) and **Project** (project scope) exist. No Session scope exists.
- A scope is a tagged value (`{"global"}`, `{"project": "<ProjectId>"}`).
- Configuration toggles each scope on or off, plus read order (default Project then Global) and default write scope (default Project).
- When the same key holds Active records in both scopes, **Project overrides Global**.
- Scope priority decides only relevance and override, never permissions. Project configuration never grants access to other projects.

### D2 Project identity

- Identity key: **canonical path of the git common dir** (multiple worktrees of one repository share one project). Outside a git repository, the canonical workspace path. The code never judges by directory name.
- The host maintains a registry of "identity key to stable `ProjectId`" at `~/.radian/memory/projects.jsonl`. Appended writes, single writer.
- **Auto-create a project only when writing memory and the directory is a git workspace**: only writes (`/memory add`, `radian memory add`, and other user commands) create. Reads and injection never create. A directory outside git (`~`, `/tmp`) **never** creates. Writes return an error stating it is not a project workspace. Users register manually with `radian memory project new`. Creation shows one notice line.
- With a possible old project (for example a moved repository), creation still proceeds. The notice points at `radian memory relink <ProjectId>` and `radian memory merge`.
- User commands: `relink`, `split`, `merge`. A moved or renamed repository never silently joins its records to a new identity.

### D3 Persistence

- Each scope keeps one append-written JSONL log as the only fact source: `~/.radian/memory/global/log.jsonl`, `~/.radian/memory/projects/<ProjectId>/log.jsonl`.
- Reuses JSONL write and read paths, single-writer lock, torn-tail truncation, unknown-event critical handling, and 0600/0700 modes. Memory has **its own event schema and its own replay**, uncoupled from session state machines.
- Events: `MemoryCreated`, `MemoryRevised`, `MemoryArchived`, `MemoryRestored`, `MemoryConfirmed`, `MemoryForgotten`, `StorePurged`. Current state equals replay result.

| Operation | Effect | Data still present |
|---|---|---|
| **Archive** | Skipped from default retrieval and injection; `restore` possible | Records and full history stay |
| **Forget** (`delete`) | Logical delete: vanishes from every view, search, injection, and export; no rebuild ever revives it | History content stays in the log |
| **Purge** | User-triggered physical cleanup: rewrites the log, drops all content of Forgotten records | Gone from copies this service manages; no guarantee over user-made backups or content already written into session files |

Purge: exclusive lock, then write temp file per current state, then fsync, then atomic rename over the old log, then fsync the directory. The new log ends with one `StorePurged { removed, at }`. On mid-way crash, old or new log stays intact. No partly valid file ever remains. Only the user triggers Purge. Archive and Forget must never claim physical deletion.

- Markdown is a view: `export` produces editable Markdown (YAML front matter with `id` and `revision`). `import` creates `MemoryRevised` per `id` plus `revision` (revision mismatch means conflict, never overwrite; malformed format means reject with position). Direct Markdown edits take no effect.
- No cross-scope atomicity promised.

### D4 Data model

```text
MemoryRecord {
    id: MemoryId,                 // stable across revisions, exports, imports, and Purge
    scope: Global | Project(ProjectId),
    kind: Preference | Decision | Fact | Lesson,
    key: Option<String>,
    content: String,
    source: User | Agent | Import,
    provenance: { session_id?, turn_id?, file?, commit? },
    verification: Unverified | Confirmed { by: User, at },
    status: Active | Archived,
    created_at, updated_at,
    revision: u64,
}
```

- **Active and Verified are two independent dimensions**: a record can be `Active` and `Unverified`. A user `confirm` never rewrites `source` to `User`: `source` keeps its value, a separate `MemoryConfirmed` is written, and `verification` becomes `Confirmed`. A user-written record is natively `Confirmed`.
- `source: Agent` appears only on old-log records: no write path produces it anymore (D5). Reads, injection, and `confirm` of these records behave as usual.
- Deterministic conflicts on same-scope same-key (no last-write-wins): one scope holds at most one Active record per non-empty key. A `create` against an existing Active key fails with `KeyConflict`, returning the existing record id and revision. Content changes use `revise` only, with `expected_revision`. `revise`, `archive`, `restore`, and `confirm` all carry `expected_revision`. A mismatch returns `RevisionConflict`. Multiple writers serialize on the single-writer lock, then revision rules judge.
- No TTL, `review_at`, `NeedsReview`, conflict candidates, or `Superseded`. Adding them adds only fields and events. The base storage format stays.

### D5 Write sources and priority

1. **The model has no write path.** No model-callable write tool exists. Writes come only from user commands (`source: User`, natively `Confirmed`) and imports (`source: Import`).
2. When the user asks to "remember something", the model routes by content (ADR 0036 role rules). Project facts and project-only processes go to the workspace `AGENTS.md` (one line, with `patch`; a workspace file, reviewed by git). Reusable cross-project practices become user-level skills (`~/.agents/skills/<name>/SKILL.md`, ADR 0025; user-level files outside the repository; the model reports path and content to the user after writing). Both take effect next session. Cross-project personal preferences stay with the user: the user runs `/memory add` directly. The model only suggests that command, never writes for the user. File tools cannot write user-level skills in any tier: the `full` tier writes with `shell`. A sandboxed session (`workspace`) never attempts the write. It hands the path and full text to the user for self-save.
3. Old-log `source: Agent` records read and inject as usual: `verification: Unverified`. Injection places them in a separate labeled section apart from user and confirmed records. Users can `confirm` (original source stays).
4. With a user record on the same-scope same-key, the user record wins. Agent records never inject-replace it.
5. User commands default to Project write scope. Content with this project path, branch, or repository structure never stores globally.

### D6 Injection

```toml
[memory.injection]
enabled = true
max_tokens = 2000      # default
global_budget = 500    # soft quota
project_budget = 1500  # soft quota
overflow = "ranked"
additions = true
addition_budget = 500
```

- Global 500 and Project 1500 are soft quotas: unused quota from one side lends to the other. The total never exceeds `max_tokens`.
- Only Active records enter. Archived and Forgotten never join. Unconfirmed agent records form their own labeled section.
- **Order** (deterministic, no embeddings or LLM): scope (Project first), then source and confirmation state (user and confirmed first), then key and keyword match score against the **session first user message**, then `updated_at` newest first, then `id` lexicographic. Past budget, selection takes whole records in order. It never truncates.
- **Fixed section**: generated once at the first model call, from memory state and the first user message then. Every later call in the same process reuses it **verbatim, byte for byte**, even after memory changes. Memory changes therefore reach the system prompt only in the **next session (or resume)**. Resume regenerates with the same query text per memory then.
- **Later-turn additions**: each new user turn (when an `InputSource::User` message in inbox commits), sorted by that message text. Candidates are Active records hitting this turn text, absent from the fixed section and from current-transcript addition messages. Selection takes whole records per D6 rules, under `addition_budget` (default 500). On non-empty selection, the host places **one** message on the `inject` lane after the user message commits and before the first model call of the turn: `Message::user`, `meta.source = System`, `meta.origin = {"kind":"memory_addition","records":[{id, revision, scope}]}`. The body is a `<memory>…</memory>` block (same low-trust notice and renderer). On empty selection it sends nothing.
- **The dedup ledger is the transcript**: ids in System messages with `origin.kind = "memory_addition"`. After compaction folds an addition message into the summary, its id leaves the ledger. A later hit can append it again.
- Addition messages are never `User`-sourced. They are low-trust data. Compaction treats them as ordinary System messages: they never join the user-verbatim list, but they join summary input.
- **`MemoryInjected`**: the set equals the fixed section plus current-transcript addition messages. On set change the code writes one `critical: false` session runtime event `{ records: [{ id, revision, scope }] }`. It records only id, revision, and scope. It never copies content. Resume reassembles per memory then. Audit sees which ids and revisions injected then. It never rebuilds the full historical prompt from this event alone.
- Parts outside the transcript (the fixed section) skip compaction handling. They join the ADR 0018 current input `T`. `memory_search` (when mounted, D7) ignores injection budgets, but its tool results join context as usual.
- With `additions = false`, only the fixed section exists.

### D7 Tools and commands

- The agent tool set holds only `memory_search` (keyword search, or read by id). **It mounts only when Active records at session start exceed `[memory.injection] max_tokens` in estimated tokens**: what fits stays fully in the fixed section, and search has no use. Estimation uses the injection ruler (ADR 0015 conservative estimate), taking Project and Global Active records per D1 read order. The mount decision runs once at session start and never changes in-session (ADR 0036: tool definitions stay fixed in-session). Resume judges again.
- No `memory_save`, `memory_revise`, or `memory_get` exist: these names never register and never advertise. Calls to them are unknown tools.
- Permissions: `memory_search` is `allow` in both tiers. No memory tool needs `ask`.
- User commands (CLI and TUI): `list`, `search`, `show`, `add`, `edit`, `archive`, `restore`, `forget`, `purge`, `confirm`, `export`, `import`, `relink`, `split`, `merge`, `project`. `forget`, `purge`, `split`, and `merge` need confirmation.

### D8 Service contract

- One host-internal `MemoryService`: Search / Get / List, Create / Revise, Archive / Restore, Forget, Confirm, Purge. All changes pass through it with revision compare-and-swap.
- Any backend must satisfy: stable ids, scope isolation, revision checks, atomic writes, unfakeable sources, Forget and Purge semantics, import and export support.
- v1 ships only the JSONL backend. No plugin trait is public yet. Configuration selects backends, scopes, conflict policies, lifecycle policies, retrieval and extraction policies. **It never disables permission isolation, sandbox denylist, or source-authenticity checks through configuration.**

### D9 Relations with compaction and session journals

- Memory and compaction stay fully independent (ADR 0018 Memory boundary). No automatic extraction exists: only explicit writes.
- Any automatic extraction reads only **committed** session facts (ADR 0019: uncommitted replacements and candidate-only compactions never read). It uses `MemoryInjected` to separate "injected memory echoed by the model" from "new independent evidence".
- The sandbox denylist enforces memory unreadability through the ADR 0020 `memory/` entry. With Memory off, Core, compaction, and session-restore test results stay unchanged.

## Verification

`radian-host::memory` unit tests and `radian-exec` end-to-end tests cover: scope isolation, worktree sharing and repository moves, auto-registration on write only and never on read, non-git directories rejected without global fallback, user-record protection, same-key conflicts, revision checks, Archive, Forget, and Purge, Markdown round trips, ordering and soft quotas, byte-identical fixed section, addition-message position and dedup, `MemoryInjected` with id, revision, and scope only, resume per current memory, no effect on Core with Memory off, no memory write tool in model requests, and conditional `memory_search` mount (unadvertised within budget, advertised past budget, fixed within one session). The `radian-sandbox` `memory_unreadable` and `skills_readable` tests cover isolation. Fixtures: `memory-injected.jsonl`, `docs/schema/config/memory-injection.toml`.
