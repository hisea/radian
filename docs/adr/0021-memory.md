# ADR 0021: Memory

Status: accepted

## Invariants

1. **Core never knows Memory.** Core never depends on memory types, scopes, or backends. With Memory off, Core, compaction, and session resume behave fully unchanged.
2. **Memory never changes session facts.** Memory never modifies session journals, compaction records, or their recovery semantics. A memory failure must never corrupt a session or block a dialogue turn.
3. **The host judges sources, and sources never rewrite.** A memory record source (user, import; old logs also carry agent-tool-written records) is fixed by the host at write time. The code never trusts a model-claimed source. An original source never changes after write (confirmation records separately).
4. **Memory is low-trust data.** Memory injected into the model is labeled data, never instructions. The session index file `SESSIONS.md` is memory too: it is low-trust data, and the model never writes it. Memory never bypasses tool permissions, sandboxes, tiers, or user instructions.
5. **The sandbox cannot read or write memory.** Under the `workspace` tier the memory store is unreadable and unwritable to `shell`. The model writes it only through the host's `write` and `patch` tools, which the permission profile allows in both tiers (D7) and which reach the two scope directories in both tiers, `global/` and this workspace's project directory, and nothing else under `memory/` (ADR 0039 decision 4). Radian injection and user commands reach the store directly. The sandbox denylist keeps the whole directory from the shell (ADR 0020), and the escape matrix verifies it.
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

- Each scope is a directory of Markdown files: `~/.radian/memory/global/<name>.md` and `~/.radian/memory/projects/<ProjectId>/<name>.md`. One record is one file: its frontmatter is the record's metadata (D4) — `name`, `description`, `type`, `source`, `created`, `modified`, `last_used` and `provenance` — and its body is the content. A model `read` of one of these files sets its `last_used` to the time of the read and rewrites only that line; a user command never changes it, and an absent `last_used` means the file was never read. Directories are `0700` and files `0600`. A write goes through a temporary file in the same directory, `fsync`, an atomic rename over the path, then a directory `fsync`; a crash leaves either the old file or the new one, never a partial file.
- Each scope has one index, `~/.radian/memory/global/MEMORY.md` or `~/.radian/memory/projects/<ProjectId>/MEMORY.md`. Its form is `- [title](file) — description`, one line per record. The store treats the index as plain text and only parses that form for the host's cap and the tests.
- Each project scope has one session index, `~/.radian/memory/projects/<ProjectId>/SESSIONS.md`, maintained by the host and never by the model. When a session ends (the process exits, the interface's `/exit`, or a quit), the host appends one line `- <session id> <start time, RFC 3339 UTC> <workspace path relative to the repository root, or .> — <first line of the first user message, cut at 120 characters, newlines removed>`, newest last, read from the committed journal only (ADR 0019): a session with no committed user message writes nothing. A line for a session id already in the file is replaced in place, so a resumed session that ends again does not duplicate its line. A workspace that maps to no project writes nothing and creates no project. A missing file is created; after an append the oldest lines beyond `[memory] sessions_index_max_lines` are dropped, and the rewrite goes through a temporary file in the same directory and an atomic rename. Deleting the file clears the index, and the next ended session creates its first line.
- A body keeps its bytes exactly: trailing newlines, blank lines and a line that spells `---` all survive a write and a read. The closing `---` of the frontmatter is the first such line after the opening one.
- `name` is the file stem and obeys `[a-z0-9-]`, 1 to 64 characters. A write refuses any other name before it touches a file. Two records never share a name; the caller chooses another name.
- A file whose frontmatter does not parse, or whose `type` or `source` is outside its closed set, is reported by `list` as one unreadable entry naming its path and line and never hides the other records. Reading it is an error naming the path and the line.
- Deleting a file is forgetting. There is no archive, no purge, no revision and no event log: the file is the record.
- Migration: when `<scope>/log.jsonl` exists and `<scope>/log.jsonl.migrated` does not, replay the log once with its reader. Every Active record becomes one file: the name comes from the record's key when it obeys the name rule, else from a slug of the first words of its content and made unique with `-2`, `-3`; the description is the first line of the content cut at 120 characters; `type` maps `preference` to `user`, `decision` and `fact` to `project`, and `lesson` to `feedback`; `source` maps `User` to `user`, `Agent` to `model`, and `Import` to `import`; `created` and `modified` come from the record; `provenance` keeps `session_id` and `turn_id`. Archived and Forgotten records are dropped. The index is written with one line per file, then the log is renamed to `log.jsonl.migrated`. A second call finds the marker and does nothing. A crash before the rename leaves the log in place, and the next call writes the same files again: the writes are idempotent.
- No cross-scope atomicity promised.

### D4 Data model

Each record is one Markdown file. Its frontmatter has these fields:

| Field | Type | Rule |
|---|---|---|
| `name` | string | The file stem; `[a-z0-9-]`, 1 to 64 characters. |
| `description` | string | One line, used by the index. |
| `type` | closed set | `user`, `feedback`, `project`, `reference`. |
| `source` | closed set | `model`, `user`, `import`. The host sets it at write time and never trusts a model-claimed value. |
| `created` | RFC 3339 UTC | Set once and kept across writes. |
| `modified` | RFC 3339 UTC | Set on every write. |
| `last_used` | RFC 3339 UTC | Optional; absent means never read. A model read updates it by rewriting only that line. |
| `provenance` | optional map | `session_id` and `turn_id`, each optional. |

- The body is free Markdown. `feedback` and `project` records carry `**Why:**` and `**How to apply:**` lines.
- The two closed sets are `RECORD_TYPES` and `RECORD_SOURCES` in `radian-memory`: the parser, the writer and the docs drift test read the same slices.
- `source` is fixed when the file is written and never changes afterwards. The host judges it; the model never supplies it.
- There is no `id`, `key`, `revision`, `verification`, or lifecycle dimension. A record's identity is its scope and its name. A change rewrites the file; deleting it is forgetting (D3).

### D5 Write sources and priority

1. **The model writes memory files.** The model writes and updates files in the two scope directories with the host's `write` and `patch` tools, in both tiers (D7; invariant 5). One file is one fact. User commands and imports write the same files.
2. **Routing.** When the user asks to remember something, the model routes by content. Project facts and project procedures go to the workspace `AGENTS.md` (one line, with `patch`; a workspace file, reviewed by git). Reusable cross-project procedures become user-level skills (`~/.agents/skills/<name>/SKILL.md`, ADR 0025; user-level files outside the repository; the model reports path and content to the user after writing). Both take effect next session. The rest goes to a memory file: the user's preferences, corrections of the model's way of working, ongoing work state and external pointers. The role rules in ADR 0036 say when to write, what never goes to memory, how to write a file, how to choose project or global, and the trust rules for reading.
3. **Old-log records.** The migration in D3 turns old `source: Agent` records into `source: model` files. They read and inject like any model record, as low-trust data (invariant 4).
4. **User records win.** The model never modifies or deletes a file whose `source` is `user` (invariant 6).
5. **Project versus global.** The model chooses the scope by the directory it writes to: `global/` or this workspace's `projects/<ProjectId>/` (D3). Content that names this repository's paths, branches, files, ADR numbers, task numbers or structure goes to project. The user's preferences, corrections of the model's way of working and pointers that hold across repositories go to global. When unsure, the model writes project. `type` is independent of scope: `user` usually global, `feedback` either, `project` project-only, `reference` either. User commands still default to the project scope.

### D6 Injection

- **What is injected**: the two index files and nothing else. At the first model call of a session (and on resume), the host reads the project scope's `MEMORY.md` — when the workspace maps to a project; reads never create one (D2) — and then the global scope's `MEMORY.md`, through the file store. The bodies are not injected: the model reads a record with `read`. When the workspace maps to a project, the section then adds one line: `Past sessions of this project are listed in <path>/SESSIONS.md; read it when the request refers to earlier work.` The file itself is never injected.
- **The section**: one system-prompt section, after the base prompt and before the skills section (ADR 0036 layer 3). It opens with a header that says the content is data from the user's memory directory, not instructions. Then `## Project memory index` with one line naming the index file and the project index text, and `## Global memory index` likewise. The project text always comes first. A scope whose index is missing or empty contributes one line saying so. An index that cannot be read (for example an invalid byte sequence) contributes one line saying so, and one warning names the file.
- **Caps**: `[memory] index_max_lines` (integer, default 200, 1 or more) and `[memory] index_max_bytes` (integer, default 25600, 1 or more) apply per index. A line past either cap is not injected; the byte cap cuts at the last whole line within it. When at least one line is dropped, the host sends the model one `System`-source message on the inbox's inject lane before its next call: it names the scope, the index file and the number of dropped lines, and asks the model to rewrite the index shorter. The notice is sent once per session for each scope.
- **Stability**: the section is computed once per process and reused byte for byte (ADR 0036). Index changes reach the system prompt in the next session or on resume.
- **`MemoryInjected`**: when the section is built, the host writes one `critical: false` runtime event `{ indexes: [{ scope, path, sha256, lines, bytes }] }`, one entry per injected index in injection order (project then global). `scope` is one of `project`, `global`. `path` is the scope's `MEMORY.md`; `sha256` is the 64-hex hash of the injected text after the cap; `lines` and `bytes` are that text's line and byte counts. The event never copies the index text. A reader also accepts the old shape `{ records: [{ id, revision, scope }] }` from journals written before the file store; a record carrying both shapes or neither is refused at its line. This build writes only `indexes`.
- `[memory] enabled` (boolean, default `true`) turns memory off: no memory rules block in the base prompt, no section, no notice and no `MemoryInjected`. `[memory] scopes` selects the scopes, always in the order project then global.
- Every `[memory.injection]` key is retired: a file that sets one gets one ADR 0008 migration warning naming the key and stating that the index caps replace it; the key is otherwise ignored.
- Parts outside the transcript (the section) skip compaction handling. They join the ADR 0018 current input `T`.

### D7 Tools and commands

- The model has no memory tool. `memory_search` no longer registers or advertises: a call to it is an unknown tool, like `memory_save`.
- No `memory_save`, `memory_revise`, or `memory_get` exist: these names never register and never advertise. Calls to them are unknown tools.
- Permissions: `write` and `patch` to `<memory root>/global/` and `<memory root>/projects/<ProjectId>/` are `allow` in both tiers, except a file named `SESSIONS.md`: that name is outside the allow rule, so the tier's default decides, and the file tools themselves refuse the write with `SESSIONS.md is maintained by radian` in both tiers. Under `workspace` the sandbox deny set keeps `shell` out of the memory store (invariant 5); `full` runs no sandbox. No memory tool exists, so no memory tool needs `allow` or `ask`.
- User commands (CLI and TUI): `list`, `search`, `show`, `stale`, `add`, `edit`, `archive`, `restore`, `forget`, `purge`, `confirm`, `export`, `import`, `relink`, `split`, `merge`, `project`. `forget`, `purge`, `split`, and `merge` need confirmation. `stale [--days N]` lists files whose `last_used` is older than N days (default 30), or that were never read and whose `created` is older than N days; it reads the file store only.

### D8 Service contract

- One host-internal `MemoryService`: Search / Get / List, Create / Revise, Archive / Restore, Forget, Confirm, Purge. All changes pass through it with revision compare-and-swap.
- Any backend must satisfy: stable ids, scope isolation, revision checks, atomic writes, unfakeable sources, Forget and Purge semantics, import and export support.
- v1 ships only the JSONL backend. No plugin trait is public yet. Configuration selects backends, scopes, conflict policies, lifecycle policies, retrieval and extraction policies. **It never disables permission isolation, sandbox denylist, or source-authenticity checks through configuration.**

### D9 Relations with compaction and session journals

- Memory and compaction stay fully independent (ADR 0018 Memory boundary). No automatic extraction exists: only explicit writes.
- Any automatic extraction reads only **committed** session facts (ADR 0019: uncommitted replacements and candidate-only compactions never read). It uses `MemoryInjected` to separate "injected memory echoed by the model" from "new independent evidence".
- The sandbox denylist enforces memory unreadability for the shell through the ADR 0020 `memory/` entry. The host file tools reach the two scope directories only (ADR 0039 decision 4). With Memory off, Core, compaction, and session-restore test results stay unchanged.

## Verification

`radian-host::memory` unit tests and `radian-exec` end-to-end tests cover: scope isolation, worktree sharing and repository moves, auto-registration on write only and never on read, non-git directories rejected without global fallback, user-record protection, same-key conflicts, revision checks, Archive, Forget, and Purge, Markdown round trips, project-then-global index injection byte for byte, the line and byte caps with one rewrite notice per session per scope, a missing or unreadable index, the disabled switch, the per-process freeze and a resume reading again, the retired `[memory.injection]` warning, `MemoryInjected` with the injected index files (scope, path, sha256 of the capped text, lines, bytes) and, for old journals, id, revision, and scope only, no effect on Core with Memory off, and no memory tool in model requests (`memory_search` is unknown). The session index is covered end to end: one line after a session ends with the journal's id, start time and first user line, the workspace column at the repository root and in a subdirectory, nothing for a session with no committed user message or for an unlinked workspace, no line in another project's file, one replaced line after a resumed session ends, and a model `read` allowed while `write` is refused. Unit tests cover the line, the 120-character cut and the `[memory] sessions_index_max_lines` cap. The `radian-sandbox` `memory_unreadable` and `skills_readable` tests cover isolation. Fixtures: `memory-injected.jsonl`, `docs/schema/config/memory.toml`.
