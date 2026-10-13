# radian

`radian` is a minimal, headless, embeddable Rust agent runtime, plus the coding agent built on
top of it. The core (`agent-core`) runs a turn loop over an injected model, tools and hooks; the
`radian-*` crates add durable sessions, a permission layer, an OS sandbox, built-in coding
tools, durable memory and the host plumbing, and two hosts ship as binaries: the line-prompt CLI
`agent` and the full-screen TUI `radian-tui`. Everything the runtime does is journalled to a
per-session JSONL file, so a session can be resumed by either host.

The design of record is `docs/minimal-headless-rust-agent-runtime-design.md`. Decisions that
constrain the code are ADRs under `docs/adr/` (see the index at the end of this file). Read
`AGENTS.md` before you change code.

## Crates

| Crate | What it is | Detailed docs |
|---|---|---|
| `agent-core` | The runtime itself: turn loop (`Agent`/`AgentSession`/`ActiveRun`), messages, tools, hooks, steering, the journal seam, events, the context-budget guard. | `crates/agent-core/README.md`, ADR 0001–0007, 0010, 0014, 0015 |
| `agent-extensions` | MCP stdio servers exposed as `agent-core` tools, under stable `server__tool` aliases. | `crates/agent-extensions/README.md` |
| `agent-provider-contract` | Adapter-neutral contract suite: the semantics every `Model` implementation must satisfy, independent of wire format. | `docs/provider-contract.md` |
| `agent-provider-openai-compatible` | Model adapter for OpenAI-compatible Chat Completions (`/chat/completions`), streaming. | `crates/agent-provider-openai-compatible/README.md`, `docs/provider-contract.md` |
| `agent-provider-anthropic` | Model adapter for the Anthropic Messages API (`/v1/messages`), streaming, API-key auth, extended thinking. | `crates/agent-provider-anthropic/README.md`, `docs/providers/anthropic-wire-notes.md` |
| `radian-session` | Session JSONL v1 storage: the durable `TranscriptJournal` seam, the reader/replay path, the disposable SQLite index and the export renderers. | `crates/radian-session/README.md`, ADR 0006 |
| `radian-runtime` | The layer above the turn loop: a durable, bounded inbox (`followup`/`steer`/`inject`/`cancel`) and the per-session driver. | `crates/radian-runtime/README.md`, ADR 0011 |
| `radian-permissions` | The permission layer: `ApprovalPolicy` + `ToolRuleSet` as one `Hooks` implementation; decides allow / ask / deny. It does not enforce anything. | `crates/radian-permissions/README.md`, ADR 0012 |
| `radian-sandbox` | OS sandbox backends (macOS `sandbox-exec`, Linux `bubblewrap`) that build a command under a policy and **refuse to run** without a proven backend. | `docs/sandbox.md`, ADR 0012, ADR 0016 |
| `radian-coding` | Built-in coding tools — `read`, `write`, `patch`, `list`, `git_status`, `git_diff`, and `shell` only when the host supplies a sandbox — with workspace path confinement. | `crates/radian-coding/README.md` |
| `radian-memory` | The memory storage layer (ADR 0021 M1): Global/Project scope logs with replay, the source rules (an agent cannot touch a user record), Forget/Purge, the `projects.jsonl` registry and the Markdown import/export view. Host-free: the caller passes `~/.radian/memory/`. | `crates/radian-memory/README.md`, ADR 0021 |
| `radian-host` | Host plumbing shared by both binaries so they cannot drift: `--header` parsing, session ids, the `--mcp-config` file, model construction, the startup description, the "built-in tools + MCP" provider, the `~/.radian/` layout and diagnostics, the model catalog, and the memory layer (`MemoryService`, project identity, the agent memory tools, injection, and the shared `agent memory` / `/memory` commands). | `crates/radian-host/README.md` |
| `agent-cli` | The `agent` binary: a line-prompt chat client with optional built-in tools, MCP servers, session-management commands and the `agent memory` commands. | `crates/agent-cli/README.md` |
| `radian-tui` | The `radian-tui` binary: a full-screen TUI (ADR 0009: full screen only, no inline mode) over `radian-runtime`, with a slash-command layer (`/help`, `/memory …`) and its own transcript scrolling. | `crates/radian-tui/README.md`, ADR 0009 |

Two more binaries exist but are not part of the product surface: `agent-extensions`'s
`mcp-fixture` (a hand-written MCP server for its tests) and `radian-sandbox`'s `sandbox-escape`
(requires `--features testing`).

## Status (2026-10-13)

- **The `~/.radian/` layout (ADR 0020)** is implemented: `config/` and `cache/` are readable
  by the sandbox; `sessions/` and `memory/` are denied. `RADIAN_HOME` overrides the
  root. (`--session-dir` / `RADIAN_SESSION_DIR` still override only the sessions directory.)
- **Memory M1 (ADR 0021)** is implemented: the `agent memory` CLI commands, the TUI `/memory`
  slash command (same commands), the agent's `memory_search` / `memory_get` / `memory_save` /
  `memory_revise` tools (writes always ask), and deterministic injection with the
  `MemoryInjected` audit event.
- **Session fork (ADR 0022)** is implemented: `agent --fork <parent> [--at <seq>]
  [--session-id <child>]` and the TUI `/fork`. The fork point is a parent turn boundary; the
  child materializes the parent's effective history there (a committed compaction in effect, an
  uncommitted replacement dropped), is written before the parent records it, and can resume or
  fork again.
- **Automatic compaction (ADR 0018)** is implemented: the proactive budget path, the
  provider-overflow fallback (compact to half and retry once), and a manual `/compact` in both
  hosts. `--no-auto-compact` turns off the proactive path only; the overflow fallback still
  runs. The model catalog and the context-budget projection are its inputs.

## Build

The workspace MSRV is **Rust 1.88**. Debug build:

```sh
cargo build --workspace --locked
```

Release build:

```sh
cargo build --workspace --locked --release
```

The two binaries land in `target/debug/` (or `target/release/`) as `agent` and `radian-tui`.
The full verification suite an agent must run before calling a change done is in `AGENTS.md`.

## Run

Both hosts read the API key **only** from an environment variable — never from the command line
— and never print it. The default variable is `$OPENAI_API_KEY` for `openai-compatible` and
`$ANTHROPIC_API_KEY` for `anthropic`; `--api-key-env` names a different one. A session is one
`<session-dir>/<session-id>.jsonl` file; the session directory defaults to
`--session-dir`, else `$RADIAN_SESSION_DIR`, else `~/.radian/sessions` (`$RADIAN_HOME/sessions`
when `RADIAN_HOME` is set, ADR 0020). The same `~/.radian/` root also holds `config/`,
`cache/` and `memory/`; the sandbox denies read on `sessions/` and `memory/`.

### `agent` (line prompt)

```sh
export OPENAI_API_KEY=sk-...

# One turn, a new session named `demo`, provider and tier named explicitly.
./target/debug/agent \
  --provider openai-compatible \
  --model gpt-5 \
  --base-url https://api.openai.com/v1 \
  --profile safe \
  --workspace "$PWD" \
  --session-id demo \
  --session-dir /tmp/radian-sessions \
  -p "summarize Cargo.toml"
```

`--provider anthropic` uses the Messages API instead: the base URL defaults to
`https://api.anthropic.com`, the key comes from `$ANTHROPIC_API_KEY`, and `--max-tokens`
(default 16384) is sent because the API requires the field. `--thinking off|adaptive|enabled:<budget>`
controls extended thinking (default `off`).

Resume that saved session — `--resume` replaces `--session-id` (the two conflict):

```sh
./target/debug/agent \
  --provider openai-compatible \
  --model gpt-5 \
  --profile safe \
  --workspace "$PWD" \
  --session-dir /tmp/radian-sessions \
  --resume demo \
  -p "now read docs/"
```

Session-management commands need no model, no key and no network: `--list`, `--export <id>`
(`--format json|markdown`, `--out FILE`), and `--search <text>`. `--export` and `--search`
work on the sessions of the current workspace (`--workspace`, else the current directory).

### `radian-tui` (full screen)

Run this only in a real terminal: it enters the alternate screen and raw mode, and it takes
over the screen. `--model` is required even for `--list`.

```sh
export OPENAI_API_KEY=sk-...

./target/debug/radian-tui \
  --provider openai-compatible \
  --model gpt-5 \
  --profile workspace \
  --approval on-ask \
  --workspace "$PWD" \
  --session-dir /tmp/radian-sessions \
  --session-id demo
```

Resume a session written by either host:

```sh
./target/debug/radian-tui \
  --provider openai-compatible \
  --model gpt-5 \
  --profile workspace \
  --workspace "$PWD" \
  --session-dir /tmp/radian-sessions \
  --resume demo
```

`--list` prints this workspace's sessions and exits without entering the terminal. The TUI's
per-turn model-call fuse is `--max-model-calls` (default 1000, far above the core's 32).
Keys, the approval box, the status bar and the context-budget wording are in
`crates/radian-tui/README.md`.

## Permission tiers

`--profile` picks all three layers of ADR 0012 at once: the sandbox (the enforcement boundary),
the rule set (allow / ask / deny) and what the host mounts. The names the binaries accept are
`safe`, `workspace` and `full` (ADR 0016); the old `workspace-write` and `full-access` remain
accepted as aliases but are not listed in `--help`. `workspace` is the default and `full` is
never reached by forgetting a flag.

| Tier | Sandbox | Shell | MCP | Built-in files |
|---|---|---|---|---|
| `safe` | none needed — nothing executes | not offered | denied | `read`/`list`/`git_status`/`git_diff` allowed; `write`/`patch` denied |
| `workspace` | enforced: macOS seatbelt or Linux bubblewrap, probed at startup | offered, **inside** the sandbox | asks | `write`/`patch` allowed only when the resolved path stays under the workspace |
| `full` | none — chosen explicitly | offered, **not** sandboxed | allowed | `write`/`patch` as `workspace` |

- **A failed probe refuses to run.** If the backend is missing or its probe fails, the host
  exits with an error instead of falling back to an unsandboxed run. There is no degrade path
  (ADR 0012, decision 2). Try `RADIAN_SANDBOX_PROBE_FAIL=1` to exercise it; it can only
  cause a refusal.
- **Without `--workspace`** no write or execute tool exists at all: the built-in tools are only
  mounted with a workspace, and `shell` additionally needs a sandbox.
- **Under `workspace`**, writes are confined to the workspace, repository metadata (`.git`) is
  read-only, sensitive directories (`~/.ssh`, `~/.aws`, `~/.gnupg`, browser profiles, …) are
  not readable by default, and the network is **on** (outbound and local-loopback
  connect/bind/listen) with the pty devices available, so a project's own tests can run
  (ADR 0016). This is **not** a claim of credential isolation: the macOS backend leaves
  `mach-lookup` open, so the IPC boundary is not tightened. MCP server processes are started
  outside the sandbox in this build, and the startup notice says so.
- **`--yes` is not "allow everything".** It swaps the approver for one that answers "allow
  once" without showing the question; `deny` rules still deny, `allow` rules still allow, the
  sandbox is unchanged, and every decision is still audited. `radian-tui` also has
  `--approval on-ask|always|never` (default `on-ask`).
- **Audit**: every decision writes one `PermissionResolved` event to the session file
  (`approver` = `policy` for a rule or grant, `user` for an answered prompt).
- Approval shows the tool and its **resolved** resource; `y` allows once, `a` allows for the
  session (in memory only, gone after a resume), anything else denies. `--allow-shell-command
  PREFIX` allows one *simple* command prefix without asking; a compound command always asks.
  Write these at the subcommand level, never at a bare program name.

Common exit codes: `agent` uses 0 (turn completed), 1 (turn failed), 2 (usage/setup error,
including a failed sandbox probe) and 130 (forced exit after a second Ctrl-C). `radian-tui` uses 0 (clean quit), 1 (startup refusal or a
driver error), 101 (the app panicked), and 128 + the signal number on SIGTERM/SIGHUP.

## Docs

| What | Where |
|---|---|
| Design of record | `docs/minimal-headless-rust-agent-runtime-design.md` |
| Decisions (read before changing code) | `docs/adr/` — 0001 transcript journal · 0002 tool execution markers and recovery · 0003 interrupted streaming prefix · 0004 hot config slots · 0005 host message identity and transcript replacement · 0006 session JSONL v1 · 0007 usage · 0008 config schema and migration · 0009 fullscreen-only first release · 0010 durable vs live events · 0011 durable inbox semantics · 0012 permission sources · 0013 no telemetry by default · 0014 reasoning content · 0015 context budget guard · 0016 tier names, network and devices · 0017 model and hook errors end the turn · 0018 model catalog and auto compaction · 0019 transcript replacement commit · 0020 the `~/.radian/` layout · 0021 memory |
| Sandbox backends: what they really enforce | `docs/sandbox.md` (spikes: `docs/spikes/macos-sandbox.md`, `docs/spikes/linux-sandbox.md`) |
| Provider contract and its escape hatches | `docs/provider-contract.md`, wire notes in `docs/providers/anthropic-wire-notes.md` |
| Session format fixtures and their checkers | `docs/schema/` (`check-fixtures.sh`, `check-config-fixtures.sh`, `fixtures/`, `config/`) |
| Dogfooding findings (radian run on radian) | `docs/findings/2026-10-12-radian-tui-dogfooding.md`, `docs/findings/2026-10-12-opencode-zen-dogfooding.md` |
| Performance baseline and its reproduction commands | `docs/perf-baseline.md` |
| Spike reports (sandbox, TUI, OAuth) | `docs/spikes/` |
| Agent instructions (verify suite, delegation) | `AGENTS.md` |
