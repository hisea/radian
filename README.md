# radian

`radian` is a minimal, headless, embeddable Rust agent runtime, plus the coding agent built on
top of it. The core (`agent-core`) runs a turn loop over an injected model, tools and hooks; the
`radian-*` crates add durable sessions, a permission layer, an OS sandbox, built-in coding
tools, durable memory and the host plumbing, and two hosts ship as binaries: the line-prompt CLI
`radian` (its headless command is `radian exec`) and the full-screen TUI `radian`. Everything
the runtime does is journalled to a
per-session JSONL file, so a session can be resumed by either host.

The design of record is `docs/minimal-headless-rust-agent-runtime-design.md`. Decisions that
constrain the code are ADRs under `docs/adr/` (see the index at the end of this file). Read
`AGENTS.md` before you change code.

## Crates

| Crate | What it is | Detailed docs |
|---|---|---|
| `agent-core` | The runtime itself: turn loop (`Agent`/`AgentSession`/`ActiveRun`), messages, tools, hooks, steering, the journal seam, events, context budget estimation. | `crates/agent-core/README.md`, ADR 0001–0007, 0010, 0014, 0015 |
| `agent-extensions` | MCP stdio servers exposed as `agent-core` tools, under stable `server__tool` aliases. | `crates/agent-extensions/README.md` |
| `agent-provider-contract` | Adapter-neutral contract suite: the semantics every `Model` implementation must satisfy, independent of wire format. | `docs/provider-contract.md` |
| `agent-provider-openai-compatible` | Model adapter for OpenAI-compatible Chat Completions (`/chat/completions`), streaming. | `crates/agent-provider-openai-compatible/README.md`, `docs/provider-contract.md` |
| `agent-provider-anthropic` | Model adapter for the Anthropic Messages API (`/v1/messages`), streaming, API-key auth, extended thinking. | `crates/agent-provider-anthropic/README.md`, `docs/providers/anthropic-wire-notes.md` |
| `radian-session` | Session JSONL v1 storage: the durable `TranscriptJournal` seam, the reader/replay path, the disposable SQLite index and the export renderers. | `crates/radian-session/README.md`, ADR 0006 |
| `radian-runtime` | The layer above the turn loop: a durable, bounded inbox (`followup`/`steer`/`inject`/`cancel`) and the per-session driver. | `crates/radian-runtime/README.md`, ADR 0011 |
| `radian-permissions` | The permission layer: `ApprovalPolicy` + `ToolRuleSet` as one `Hooks` implementation; decides allow / ask / deny. It does not enforce anything. | `crates/radian-permissions/README.md`, ADR 0012 |
| `radian-sandbox` | OS sandbox backends (macOS `sandbox-exec`, Linux `bubblewrap`) that build a command under a policy and **refuse to run** without a proven backend. | `docs/sandbox.md`, ADR 0012, ADR 0016 |
| `radian-coding` | Built-in coding tools — `read`, `write`, `patch` (single or atomic multi-edit), `list`, `glob`, `grep`, `git_diff` (`stat: true` gives the status and a `--stat` summary), when the host supplies a workspace; `shell` and the `job` tool additionally require a sandbox. A truncated foreground result registers under a normal job id; retention additionally requires a session-private output directory. Workspace path confinement applies throughout. | `crates/radian-coding/README.md` |
| `radian-memory` | The memory storage layer (ADR 0021 M1): Global/Project scope logs with replay, the source rules (an agent cannot touch a user record), Forget/Purge, the `projects.jsonl` registry and the Markdown import/export view. Host-free: the caller passes `~/.radian/memory/`. | `crates/radian-memory/README.md`, ADR 0021 |
| `radian-host` | Host plumbing shared by the TUI and the headless command so they cannot drift: `--header` parsing, session ids, the `--mcp-config` file, model construction, the startup description, the "built-in tools + MCP" provider, the `~/.radian/` layout and diagnostics, the model catalog, and the memory layer (`MemoryService`, project identity, the agent memory tools, injection, and the shared `radian memory` / `/memory` commands). | `crates/radian-host/README.md` |
| `agent-cli` | The library behind `radian exec`: the line-prompt chat client with optional built-in tools, MCP servers, session-management commands and the `radian memory` commands. | `crates/agent-cli/README.md` |
| `radian-tui` | The `radian` binary: a full-screen TUI (ADR 0009: full screen only, no inline mode) over `radian-runtime`, with a slash-command layer (`/help`, `/memory …`, `/access`, `/jobs`) and its own transcript scrolling. | `crates/radian-tui/README.md`, ADR 0009 |

Two more binaries exist but are not part of the product surface: `agent-extensions`'s
`mcp-fixture` (a hand-written MCP server for its tests) and `radian-sandbox`'s `sandbox-escape`
(requires `--features testing`).

## Status (2026-10-13)

- **The `~/.radian/` layout (ADR 0020)** is implemented: `config/` and `cache/` are readable
  by the sandbox; `sessions/` and `memory/` are denied. `RADIAN_HOME` overrides the
  root. (`--session-dir` / `RADIAN_SESSION_DIR` still override only the sessions directory.)
- **Memory M1 (ADR 0021)** is implemented: the `radian memory` CLI commands, the TUI `/memory`
  slash command (same commands), the agent's `memory_search` tool (search by keyword, or read
  records by id; mounted only past the injection budget), and deterministic injection
  with the `MemoryInjected` audit event.
- **The configuration file (ADR 0008)** is implemented read-only: `--config`/`RADIAN_CONFIG`/
  `~/.radian/config/config.toml`, with `model.*`, `permissions.profile` and `compaction.auto`
  wired as defaults (the command line wins). It never contains a secret value.
- **Pi configuration migration (Phase 5)** is implemented: `radian config import-pi --from
  <pi-agent-dir>` drafts a Radian configuration (stdout) plus a mapping report (stderr). It
  reads only the directory you name, never `auth.json`, never a secret value, and writes
  nothing without `--write` (which refuses an existing file). Radian does **not** promise
  compatibility with Pi's file format: see [`docs/pi-migration.md`](docs/pi-migration.md).
- **Session fork (ADR 0022)** is implemented: `radian exec --fork <parent> [--at <seq>]
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

The binary lands in `target/debug/` (or `target/release/`) as `radian` (with a `rad` alias);
`radian exec` is its headless command (ADR 0032: the `agent` binary is gone).
The full verification suite an agent must run before calling a change done is in `AGENTS.md`.

## Run

Both hosts read the API key **only** from an environment variable — never from the command line
— and never print it. The default variable is `$OPENAI_API_KEY` for `openai-compatible` and
`$ANTHROPIC_API_KEY` for `anthropic`; `--api-key-env` names a different one. A session is one
`<session-dir>/<session-id>.jsonl` file; the session directory defaults to
`--session-dir`, else `$RADIAN_SESSION_DIR`, else `~/.radian/sessions` (`$RADIAN_HOME/sessions`
when `RADIAN_HOME` is set, ADR 0020). The same `~/.radian/` root also holds `config/`,
`cache/` and `memory/`; the sandbox denies read on `sessions/` and `memory/`.

### `radian exec` (line prompt)

```sh
export OPENAI_API_KEY=sk-...

# One turn, a new session named `demo`, provider and tier named explicitly.
./target/debug/radian exec \
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
./target/debug/radian exec \
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

### `radian` (full screen)

Run this only in a real terminal: it enters the alternate screen and raw mode, and it takes
over the screen. `--model` is required even for `--list`.

```sh
# The key comes only from ~/.radian/credentials/auth.json (ADR 0026 D2), e.g.
#   {"openai-compatible": {"type": "api_key", "key": "$OPENAI_API_KEY"}}

./target/debug/radian \
  --provider openai-compatible \
  --model gpt-5 \
  --profile workspace \
  --approval on-ask \
  --workspace "$PWD" \
  --session-dir /tmp/radian-sessions \
  --session-id demo
```

Resume a session written by either host (the common flags have short forms; `-m`, `-w` and
`-r` below are `--model`, `--workspace` and `--resume`):

```sh
./target/debug/radian \
  --provider openai-compatible \
  -m gpt-5 \
  --profile workspace \
  -w "$PWD" \
  --session-dir /tmp/radian-sessions \
  -r demo
```

`--list` prints this workspace's sessions and exits without entering the terminal. The
per-turn model-call fuse is `--max-model-calls` (default 1000, far above the core's 32, for
both the TUI and `radian exec`). Keys, the approval box, the status bar and the
context-budget wording are in `crates/radian-tui/README.md`.

The common flags in full: `-m`/`--model`, `-w`/`--workspace`, `-r`/`--resume`, `-l`/`--list`
and `-y`/`--yes` (`-V` is `--version`).

## Configuration file (ADR 0008)

Both hosts read `~/.radian/config/config.toml` (`$RADIAN_HOME/config/config.toml`) unless
`--config FILE` or `RADIAN_CONFIG` names another path. A missing file is an empty configuration
and is never created. The file is **read-only** in this build: there is no `config migrate` and
nothing is ever written back.

```toml
schema_version = 1

[model]
provider = "openai-compatible"
base_url = "https://api.openai.com/v1"
model = "gpt-5"
context_window = 200000
api_key = { from = "env", name = "OPENAI_API_KEY" }   # a reference; never a value

[model.headers]
x-tenant = "team"

[permissions]
profile = "workspace"        # safe | workspace | full

[compaction]
auto = true

[tui]
theme = "dark"               # dark | light | mono; NO_COLOR forces mono
```

- **The command line always wins**; the file only supplies defaults.
- **A secret value is never accepted.** `api_key`/`token`/`password`/… must be a `SecretRef`
  (`{ from = "env", name = "…" }`; this version supports `env` only), and a secret-looking
  string anywhere in the file is refused. A refusal names the key path and never the value.
- `schema_version` is required and an integer. A **newer** version is read read-only with a
  warning; an **older** one is refused (there is no migration).
- An unknown `permissions` value is refused (fail-closed); an unknown `tui.*` value and any
  unknown field is ignored with a warning. Startup prints `[config] <path|none>` and the
  ignored keys (never values).
- The file is **sandbox-readable** (ADR 0020), which is exactly why it holds references, never
  keys.

## Permission tiers

`--profile` picks all three layers of ADR 0012 at once: the sandbox (the enforcement boundary),
the rule set (allow / ask / deny) and what the host mounts. The names the binaries accept are
`safe`, `workspace` and `full` (ADR 0016); the old `workspace-write` and `full-access` remain
accepted as aliases but are not listed in `--help`. **`full` is the default** (ADR 0043): the
neighbouring harnesses run unsandboxed by default, and a sandbox is one flag away
(`--profile workspace`). A project can turn it on for itself in
`<workspace>/.radian/config.toml` (`[permissions] profile = "workspace" | "safe"`); that
declaration can only **narrow**, never widen.

| Tier | Sandbox | Shell | MCP | Built-in files |
|---|---|---|---|---|
| `safe` | none needed — nothing executes | not offered | denied | `read`/`list`/`glob`/`grep`/`git_diff` allowed; `write`/`patch` denied |
| `workspace` | enforced: macOS seatbelt or Linux bubblewrap, probed at startup | offered, **inside** the sandbox | asks | `write`/`patch` allowed only when the resolved path stays under the workspace |
| `full` | none — the default (ADR 0043) | offered, **not** sandboxed | allowed | `write`/`patch` as `workspace` |

- **A failed probe refuses to run.** If the backend is missing or its probe fails, the host
  exits with an error instead of falling back to an unsandboxed run. There is no degrade path
  (ADR 0012, decision 2). Try `RADIAN_SANDBOX_PROBE_FAIL=1` to exercise it; it can only
  cause a refusal.
- **Without `--workspace`** no write or execute tool exists at all: the built-in tools are only
  mounted with a workspace, and `shell` additionally needs a sandbox.
- **Under `workspace`**, writes are confined to the workspace, repository metadata (`.git`) is
  read-only, sensitive directories (`~/.ssh`, `~/.aws`, `~/.gnupg`, browser profiles, …) are
  not readable by default, and the network is on (outbound and local-loopback
  connect/bind/listen) with the pty devices available, so a project's own tests can run
  (ADR 0016). This is **not** a claim of credential isolation: the macOS backend leaves
  `mach-lookup` open, so the IPC boundary is not tightened. MCP server processes are started
  outside the sandbox in this build, and the startup notice says so.
- **`--yes` is not "allow everything".** It swaps the approver for one that answers "allow
  once" without showing the question; `deny` rules still deny, `allow` rules still allow, the
  sandbox is unchanged, and every decision is still audited. `radian` also has
  `--approval on-ask|always|never` (default `on-ask`).
- **Audit**: every decision writes one `PermissionResolved` event to the session file
  (`approver` = `policy` for a rule or grant, `user` for an answered prompt).
- Approval shows the tool and its **resolved** resource; `y` allows once, `a` allows for the
  session (in memory only, gone after a resume), anything else denies. `--allow-shell-command
  PREFIX` allows one *simple* command prefix without asking; a compound command always asks.
  Write these at the subcommand level, never at a bare program name.

Common exit codes (ADR 0032 A3, unified): 0 success, 1 this turn/run failed, 2 a usage, setup
or startup refusal (clap's own errors, a bad config, an unknown keybinding, a refused startup,
a failed sandbox probe), 101 the app panicked, 128 + the signal number on SIGTERM/SIGHUP, and
130 for a forced exit after a second Ctrl-C. `radian exec` and the TUI share the codes;
`radian`'s TUI used to return 1 for a startup refusal and now returns 2, which is the one
behavior change. `radian exec --list | head` (and every print-and-exit mode) still exits 0
silently on a broken pipe.

## Docs

| What | Where |
|---|---|
| Design of record | `docs/minimal-headless-rust-agent-runtime-design.md` |
| Decisions (read before changing code) | `docs/adr/` — 0001 journal ordering · 0002 tool markers and recovery · 0003 interrupted prefix · 0004 hot config slots · 0005 message identity and transcript replacement · 0006 session JSONL v1 · 0007 usage · 0008 config schema and migration · 0009 fullscreen-only TUI · 0010 durable vs live events · 0011 inbox semantics · 0012 permission sources · 0013 no telemetry · 0014 reasoning · 0015 context budget estimation · 0016 tiers, network and devices · 0017 turn errors and retries · 0018 catalog and auto compaction · 0019 replacement commit · 0020 the `~/.radian/` layout · 0021 memory · 0022 fork · 0023 TUI architecture · 0024 `ask_user` · 0025 skills · 0026 providers and model switching · 0027 footer and chrome · 0028 transcript content · 0029 theme files · 0031 project trust · 0032 one binary · 0033 `!`/`!!` · 0034 herdr · 0035 visual language · 0036 system prompt · 0037 background commands · 0038 access and `.git` writes · 0039 host file access · 0040 group commit · 0042 Mermaid · 0043 single-session radian |
| Sandbox backends: what they really enforce | `docs/sandbox.md` (spikes: `docs/spikes/macos-sandbox.md`, `docs/spikes/linux-sandbox.md`) |
| Provider contract and its escape hatches | `docs/provider-contract.md`, wire notes in `docs/providers/anthropic-wire-notes.md` |
| Session format fixtures and their checkers | `docs/schema/` (`check-fixtures.sh`, `check-config-fixtures.sh`, `fixtures/`, `config/`) |
| Dogfooding findings (radian run on radian) | `docs/findings/2026-10-12-radian-tui-dogfooding.md`, `docs/findings/2026-10-12-opencode-zen-dogfooding.md` |
| Performance baseline and its reproduction commands | `docs/perf-baseline.md` |
| Spike reports (sandbox, TUI, OAuth) | `docs/spikes/` |
| Agent instructions (verify suite, delegation) | `AGENTS.md` |

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Third-party code and its attribution are listed in [NOTICE](NOTICE).
