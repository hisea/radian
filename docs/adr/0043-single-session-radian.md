# ADR 0043: Single-Session radian

Status: accepted

## Background and goals

radian is a coding agent of "one general interactive session": TUI is the main interface, `radian exec` is headless/line mode. It does no in-process orchestration (no named agents, task agents, leads, worktree pools), only one one-shot `subagent` tool. No sandbox by default. Sandbox opens with one flag, or saves per project, and only narrows. It is a first-class self-reporting agent to herdr at the same time (ADR 0034).

## Decisions

### 1. One session kind, no in-process orchestration

- Only one general interactive session exists. `radian` opens TUI, `radian exec` runs the headless build of the same host. No named/task/lead session kinds, `/mode`, `/tasks`, control-port task verbs, project-level `orchestration.toml`/`agents/`/`prompts/`/`progress/`, task worktrees, or `git_commit` exist.
- `control.rs` stays, but drops task verbs: `followup`, `steer`, `inject`, `cancel`, `list_sessions` (one row for this session only), `subscribe`. Its source rules: default `Scheduler`, clients declare `Subagent` or `Mcp`. **`User` and `PermissionDecision` are refused**. **It takes no approval answers** (approvals belong to humans).
- Project dir `<workspace>/.radian/` keeps only `config.toml` (ADR 0008 narrow config, with §2 tier claims). Project skills live in `.agents/skills/` (ADR 0025; from workspace up to repo root). `.radian-sandbox/` is a sandbox temp dir beside `.radian/` in the workspace, never a project resource. The trust gate (ADR 0031) lists `.agents/skills/` with `config.toml`. The former loads only when trusted. `config.toml` only triggers the trust question. Its narrow-only `permissions.profile` narrowing always applies.

### 2. `subagent`: one blocking one-shot child session

**One tool `subagent { agent?, task }`** blocks. Child sessions return their last assistant text on finish. Text caps at 4 000 chars. Over-cap truncates with a mark. Results add child session id (viewed through `radian attach`) with usage. Child sessions start blank, carry no parent history, `task` is their first input.

- **Two built-in kinds** exist. `explore` is read-only (`read glob grep`). User-level `[model.smol]` `provider`/`model` routes it cheaper models. Unset follows parent. Half-set warns and ignores. Project-level config never reads it. `general` owns all parent-session tools. It always follows parent current route. No user or project custom kinds exist. No `~/.radian/agents/` or `<workspace>/.radian/agents/` exists.
- **Parallel**: many `subagent` calls in one reply run parallel. The tool is `ToolConcurrency::Shared`, Core batch concurrency is the parallelism, no extra cap set. Parallel `general` children can each run `cargo`/tests. They own no extra process-level concurrency cap. Stacked load is a known cost.
- **Depth 1**: child sessions own no `subagent` tool.
- **Tiers and network inherit the parent session, narrow only**. `explore` read-only tool set is one narrowing. Parent `full` means unbounded, child `full` means unbounded the same. Children stay "never wider than parent".
- **It is a `radian exec` child process**: each subagent owns its journal. `SessionCreated.parent = { session_id, seq, relation: "subagent" }`. The parent-session host starts it, waits for it, takes its stdout conclusions. Parent-turn cancel kills children.
- **Headless children own no approver**: auto parents (`--yes`) mean auto children. Else child `ask` all resolves as denied. `radian exec` with no terminal returns `Deny` for ask through `TerminalApprover` (fail closed). Conclusions state what was denied. `explore` is read-only and never meets this.
- **Current limits**: `subagent` owns no background mode (`background: true`), worktree isolation, structured results, custom kinds, or solo timeouts. Only parent-turn timeouts and cancel govern it. Parameters extend on the same tool only, never add a second tool.

### 3. No sandbox by default; sandbox saves per project, narrows only

- `radian` (TUI) with `radian exec` run **`full`** with no `--profile`. Startup lines write `full · no sandbox · no prompts`. Fact rows write `no sandbox: commands can read and write anywhere you can, including the network · no prompts`.
- **The sandbox stays** (`radian-sandbox`, probe-failure means deny, deny-read lists, socket allowlists, ADR 0016): `--profile workspace` with `/access` opens it explicitly. No new powers add to it. Four use scenes exist. Scenes are strange repos, unattended runs (auto, scripts), third-party skills, many projects on one box. Deny-read lists separate other projects sessions, memory, credentials. Shells in the sandbox never prompt per command. `workspace`-tier shells `allow` inside the sandbox. Crossings need approval. No-human scenes resolve as denied.
- **Saves per project, both sides take narrower**: user choices for a project save to `~/.radian/access/access.json` (ADR 0038). First entry to a new repo adds one trust-gate popup option "use the sandbox for this project". Project `.radian/config.toml` can write `profile = "workspace"`, meaning "this project wants the sandbox". It **only narrows**, writing `full` is a config error, warns and ignores. That narrowing applies trusted or not.
- Startup lines follow fixed text. `full` writes `full · no sandbox · no prompts`. Summary rows write `full · no sandbox · no prompts · …`. Footers write `full · no prompts`. `workspace` writes `sandbox on: commands stay in the workspace`.

### 4. Tool set

The built-in tool **max catalog** is 12:

`read`, `write`, `patch`, `glob`, `grep`, `shell`, `job`, `memory_search`, `ask_user`, `skill`, `subagent`, `goal`.

**Each request carries only schemas the session really mounts**, never the whole catalog unconditionally.

- Coding tools mount only with a workspace/tool provider. `shell` with `job` also need a sandbox. Foreground-truncation keeps (ADR 0037 decision 3a) also need caller-provided session-private output dirs. With none, `job` still mounts, only with no foreground entries.
- `memory_search` mounts only with memory **and** Active records past inject budget (ADR 0021 D7). No memory write tools exist. `ask_user` mounts only with an answerable human prompter. `skill` mounts only with discovered skills. `subagent` never mounts in child sessions (depth 1). `goal` always mounts (host level: no goal answers `no_active_goal`, ADR 0047).
- MCP tools are extra per-session-config mounted tools. MCP tool text results take the same cap as `read` default budget before entering context (64 KiB). Over-cap parts drop. Ends gain one truncation-mark row naming source size. Truncation runs where the MCP provider maps server responses to tool results, journals record post-truncation results, replays agree. Under-cap results stay byte-identical. The cap takes no config key.

Three concrete sets stay pinned by test: brand-new interactive TUI workspace sessions (memory inside budget) advertise 11 besides `memory_search`. Brand-new headless `radian exec` with no workspace advertises only `skill`, `subagent`, `goal`. Memory past inject budget adds `memory_search` to both sessions.

- Old names `list`, `git_status`, `job_output`, `job_list`, `job_kill`, `shell_output`, with `git_diff` stay directly deleted, never advertised, never dispatched. Old-name calls receive unknown-tool errors. Tools only rename or delete directly, never set transition aliases.
- `job` (`id` plus `op: read | wait | kill`) carries background-job reads and writes (ADR 0037 decision 4). Foreground-truncation output shares one id space with background jobs (ADR 0037 decision 3a).
- `read` reading directories carries old `list` functions (`limit` caps entries). Negative `read` `offset` counts from file end (`-N` reads from Nth-last row).

### 5. Permission, policy, and safety layers

- The list keeps ADR 0012 allow/ask/deny rules with two tiers. The effective source of a turn only lowers when the turn mixes non-human input (ADR 0011 L2). The list also keeps sandbox probe-failure deny, deny-read lists, Unix socket allowlists, project-level configs never widen, `.git` read-only entries (ADR 0038 decision 3), fd-level file tools (ADR 0039 decisions 1–3), credential redaction (ADR 0026), trust gate (ADR 0031), control-port `User` and approval refusal, unrebindable approval keys, background-job caps (ADR 0037).
- Under `auto`, ask rules never prompt, even in a session with a person present.
- The sandbox is an **option**, never the default. Default `full` lets models read and write everything users can, network included.

### 6. herdr

radian decisions as a first-class herdr agent live in **ADR 0034**: state, session identity, metadata (`model`/`context`/`tier`/`pending`), `blocked` labels, six `HERDR_*` passes, plugin, `working` within 1 second of input. The `--kind radian` herdr PR stays pending. Until it merges, start through plugins, drive through `pane send-text` + `send-keys enter`.

### 7. `radian attach <session> [--follow]`

`crates/radian-tui/src/attach.rs`: reuses `radian_host::JournalObserver` to read journals, `App::push_message` into the same transcript render. **Read-only**, never takes session write locks. `--follow` tails newly appended rows, half rows wait for next time. Keys scroll and quit only, all else swallowed, producing no input events. Ids build paths only after `validate_session_id` checks. Streaming deltas stay invisible (ADR 0010). **No "waiting approval" rows**: they need durable `PermissionRequested`, which never exists now. Uses: read-only views of other radian sessions (herdr "screen read" counterparts in radian). ADR 0034 plugin `attach` panes use it.

## Constraints and Costs

- No in-process orchestration: parallel work uses many `subagent` calls in one reply. Cross-session keeps need own journals each, viewed through `attach`.
- Default `full`: safety rests on user-explicit sandbox choices, approval rules, and project trust. Strange repos with unattended runs open `workspace`.
- Parallel `general` children can stack load. No extra process-level concurrency cap exists.
- Control-port with herdr reports are local IPC, never remote protocols.
- `attach` is read-only. It never answers approvals for watched sessions, and never sees streaming output.

## Verification

Full Verify suites (`cargo fmt`, `clippy`, `cargo test --workspace --no-fail-fast`, `journal-selfcheck`, both schema scripts). `explore` child tool tables hold only three read-only tools, and `write` is denied. `general` children own no `subagent`. Parent `workspace` means child `SessionCreated.profile = workspace`, parent `full` means child `full`. Three `subagent` calls in one reply start parallel (three child processes co-exist). Over-cap conclusions truncate with marks. Parent cancel exits children. Non-auto parents deny child asks with conclusions stating it. No-flag starts show `no sandbox` in startup rows. `--profile workspace` shows `sandbox`. Project-config `profile = "full"` ignores with warnings. User-record `full` plus project-claim `workspace` yields effective `workspace`. Old-name `list`, `git_status`, `job_output`, `job_list`, `job_kill`, `shell_output`, `git_diff` calls receive unknown-tool errors and never appear in request schemas. Model requests own no memory write tools, and `memory_search` appears only past memory inject budget. `control` refuses `User` with approval answers. `attach` is read-only, `--follow` runs normal. Mutations: depth checks dropped, inherit changed to fixed `full`, tools changed to `Exclusive`, cancel never propagates, project configs widen, both sides take wider.
