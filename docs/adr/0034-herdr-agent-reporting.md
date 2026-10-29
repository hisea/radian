# ADR 0034: Reporting Agent State to herdr

Status: accepted

## Decisions

### H1 Reports only in TUI, only to herdr

- Conditions: `HERDR_ENV == "1"`, and both `HERDR_PANE_ID` and `HERDR_SOCKET_PATH` are non-empty, and reporting is not turned off. One missing piece disables the whole feature with no notice.
- Only the fullscreen TUI reports. `radian exec` and `-p` never report: they own no interface that herdr displays, and `-p` exits after one turn.
- The startup line gains one item `herdr: reporting`, so the user knows it runs.

### H2 State derives from projection and pending queue

| herdr status | Condition | `message` |
|---|---|---|
| `blocked` | Pending queue is non-empty; or the startup trust modal waits for an answer; or idle with no jobs plus one of five: wake budget is spent, a gated-down notice waits; `/goal` budget is topped out; a completion notice failed to enqueue (still fails after retry), a wake that should settle is lost; the built-in intent-stop validator hits after two straight continuations a third time; the hook idle-continuation cap hits after 8 hook continuations whose turns did no work | Only the category: `approval: <tool name>`, `question`, `trust`, `wake budget`, `goal budget`, `job notice lost`, `intent stop`, `hook stop`. **No command text, paths, or question text. A lost notice carries no job id, command, or error text. An intent stop carries no previous message text. A hook stop carries no hook stderr, additional context, or previous message text** |
| `working` | Queue is empty, `TurnState::Running`; or no turn runs, but background jobs run | None |
| `idle` | Queue is empty, `TurnState` is `Idle` or `Cancelled`; the first report after session open | None |

- Pending and trust `blocked` beats `working`. Four idle-block `blocked` states hold only when idle with no jobs. Running background jobs report `working · jobs`. A new turn returns to plain `working` with no label (normal state resumes at next turn start). Label priority: `job notice lost` beats `intent stop`. `intent stop` beats `hook stop`. `hook stop` beats `goal budget`. `goal budget` beats `wake budget`. approval, question, and trust beat all idle blocks.
- State is a pure function `(TurnState, pending_len, trust_modal, idle_block, running_jobs)`. It computes after `update`. Only a changed result queues a report. `update` itself does no I/O. `running_jobs` counts running background jobs in this session (footer `jobs` item reads the same start/settle pair, no second source). A running turn reports plain `working` with no label. `idle_block` bits jointly carry five stops: spent wake budget, `/goal` budget stop, lost completion notices, intent-stop stops, hook-stop stops. TUI ors them together for the pure function. `herdr_blocked` priority picks the label. All four follow the same jobs rule: idle with running jobs reports `working · jobs`, never `blocked`.
- **`working` reports within 1 second of input**: `TurnStarted` reports on entering projection. It never waits for the first model token (the 5-second gate of `herdr agent prompt --wait`).
- No `done` state exists: herdr itself shows "back from working to idle, user has not seen yet" as done.

### H3 Report fields

- `source = "radian"`, `agent = "radian"`.
- `seq`: takes `SystemTime` nanoseconds, and guarantees growth past the last value in this process (`max(now_ns, last + 1)`). Cross-process monotonicity rests on timestamps.
- **Session identity reports once separately**: `pane.report_agent_session`. `agent_session_id` is the radian session id. `agent_session_path` is the journal absolute path. It sends at startup and at session switch. Session fields in status reports stay.
- **Resume command**: `resume_argv = ["radian", "--resume", <id>, ...]`. Parameters match the normal resume command, but arrive as an array with no shell quoting. The first word is always `radian` (bare name on PATH). These mean **no resume command**: apostrophes or control characters in any parameter, over 64 parameters, over 8 KiB. Status still reports. The startup line notes `herdr: reporting (no resume: <reason>)`. The resume command never holds keys, header values, or `--trust`.
- **Display metadata**: `pane.report_metadata`, `source = "radian"`. `tokens` reports only: `model` (current model short name), `context` (context used percent, such as `42%`), `tier` (`workspace|full`), `pending` (pending count). Costs and token totals are **never reported**. `tasks` is never reported, because no in-process child-session count source exists. `title` is never reported. `display_agent` reports `radian`. Changed values only report. They merge with status reports into one "keep latest" slot. `ttl_ms` stays unset.
- **`state_labels`** gives `blocked` a sharper word: one H2 category. Values are `approval` / `question` / `trust` / `wake budget` / `goal budget` / `job notice lost` / `intent stop` / `hook stop`. It gives `working` one `jobs`. No turn runs but background jobs run, so it writes `jobs` (herdr shows `working · jobs`). A running turn means plain `working` with no label. `jobs` is only a label value. It reports no job count, command, or duration. It never reports `done` or `unknown`.
- Reports carry only H2/H3 fields. They never carry transcript, usage, costs, paths outside the workspace, or any command or question text.

### H4 Transport never slows radian

- It writes the `HERDR_SOCKET_PATH` Unix socket directly, one JSON line (the herdr socket API). It never spawns a `herdr` child process.
- One dedicated OS thread holds one latest-value slot per method (State/Session/Metadata). The UI thread drops a new value in the slot and returns. It never waits. The thread takes values one at a time, connects, writes, waits for the reply, timeout 500 ms. A failure retries once (1 500 ms). A second failure drops that copy and waits for the next change. Backlogged old states get overwritten. Each method keeps its own latest. Else queued metadata in the same turn pushes out the startup status report. herdr needs a status report holding the pane before it takes `resume_argv`.
- This thread stays outside the tokio runtime and never uses `block_on`. A stuck thread or a gone herdr never changes radian behavior.
- The herdr reply only proves delivery. It never parses and never depends on reply content. `resume_not_accepted` and `invalid_resume_argv` enter debug logs only (when open), never disturb the user.

### H5 Release

Normal exit (paths restoring the terminal such as `/quit`, Ctrl-D, double Ctrl-C) sends one synchronous `pane.release_agent`, timeout 500 ms. A failure is ignored. Panic and kill paths never release. The herdr fallback owns them (pane returns to shell prompt, then clears itself after one or two seconds). The panic path adds no I/O. A session switch inside one process reports the new session identity with no release.

### H6 Switches

Config `[integrations] herdr = true | false`, default `true`. Env var `RADIAN_HERDR=0` turns it off once. It takes effect only in panes where herdr sets `HERDR_ENV=1`.

### H7 Relation with privacy

This ADR never touches the network: the target is a local Unix socket, outside ADR 0013 outbound-allowlist scope. But it hands session id, journal path, and state to another local process. **It sends only H2/H3 fields** (state, session identity, resume command, `model`/`context`/`tier`/`pending` metadata, and `state_labels`). Any extra field needs a new decision.

### H8 herdr socket in the sandbox

This ADR never changes the sandbox. Sandbox commands reach same-user Unix sockets outside the workspace only under ADR 0016 Unix socket rules. On macOS `workspace` denies all outbound Unix sockets by default. The user writes the herdr socket into user-level `allow_unix_sockets` to connect. Linux sets no limit. The radian process itself (TUI, herdr reporting, MCP processes) stays outside the sandbox, unaffected.

### H9 `HERDR_*` env vars and user-level `allow_env`

Six `HERDR_*` names (`HERDR_ENV`, `HERDR_PANE_ID`, `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`, `HERDR_SOCKET_PATH`, `HERDR_BIN_PATH`) stay in shell env allowlists at all tiers. They are pane coordinates and paths, not credentials. `workspace` shells still cannot reach that socket (unless allowlisted), so passing the vars widens no boundary. `full` owns no boundary at all. User-level config adds `[sandbox] allow_env = ["NAME", ...]` (names only, never values; user level only). A project-level occurrence is a config error. Child sessions inherit the parent `allow_env`.

### H10 herdr plugin

The repo ships one herdr plugin `integrations/herdr/` (manifest and scripts, versioned with the repo). `start`/`start-worktree` actions start `radian` in the current pane or a new worktree. An optional `worktree.created` event starts radian automatically. The `sessions` pane runs `radian --list`. The `attach` pane runs `radian attach <id> --follow` (session id arrives through `herdr plugin pane open --env RADIAN_ATTACH_SESSION=<id>`). Plugin scripts call only `HERDR_BIN_PATH` and `radian`. They never read radian session files and never touch credentials. `min_herdr_version = "0.9.5"`.

### H11 Known limits

- herdr 0.9.5 treats no self-reported unknown kind as a named agent. `herdr agent list` shows `null` for the radian entry `agent_session`. `herdr agent prompt <pane> --wait` returns `agent_not_ready: not an active named agent`. Manual reporting through the herdr CLI itself fails the same way, unrelated to what radian sends. Starting radian from a shell pane in herdr and waiting for readiness needs herdr-side kind and detection rules (`--kind radian`). That PR stays pending. Until it merges, start radian through plugin actions. `agent prompt` uses `pane send-text` + `send-keys enter`.
- The herdr protocol belongs to herdr and can change. It uses only three documented methods and fields. It never parses replies. The worst protocol change leaves herdr blind to radian, and radian itself stays unaffected.
- The resume command auto-runs on herdr restart. It is only `radian --resume <id>`. It still passes the trust gate.

## Verification

`radian-host::herdr` unit tests: fake Unix socket receives expected JSON, `seq` strictly grows, apostrophe-bearing parameters drop the resume command while status still reports, the send thread gives up after timeout with no listener reply while callers never block, backlog sends only the latest, each method keeps its own latest, metadata reports only changed keys with no usage/costs/paths/commands/question text, `state_labels` is correct. State-function and label unit tests: idle with job count above 0 reports `working` with `jobs` label (including spent wake budget, topped-out `/goal` budget, lost completion notices, intent-stop stops, and hook-stop stops). A running turn with job count above 0 is plain `working` with no label. Non-empty pending with job count above 0 reports `blocked`. Idle with no jobs reports `blocked` per idle block with its own label (`wake budget` / `goal budget` / `job notice lost` / `intent stop` / `hook stop`). Two co-held idle blocks report the priority label (lost notice beats intent-stop beats hook-stop beats goal budget beats wake budget). Zero-job behavior stays unchanged. Pty: set `HERDR_ENV` at a fake socket, run one mock turn, receive `idle`, `working`, `idle`, `session`, `metadata`, `release` in order. A triggered approval receives `blocked` with `approval: shell`. A model-started background command ends the turn with `working` and the `working: jobs` label. Command end receives `working` (notice-opened turn) then `idle`. No `HERDR_ENV` or `RADIAN_HERDR=0` receives nothing. Input reports `working` within 1 second. `integrations/herdr` manifest required fields stay pinned test by test. `check-config-fixtures.sh` syncs `integrations-herdr.toml` with `sandbox-allow-env.toml`.
