<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/readme/banner-dark.png">
    <img src=".github/readme/banner-light.png" alt="radian" width="480">
  </picture>
</p>

<p align="center"><b>English</b> | <a href="README.zh-CN.md">简体中文</a></p>

<p align="center">
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-5b3fd0" alt="license: MIT / Apache-2.0"></a>
  <img src="https://img.shields.io/badge/rust-1.99.1%2B-5b3fd0" alt="rust 1.99.1+">
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-5b3fd0" alt="platform: macOS | Linux">
  <img src="https://img.shields.io/badge/status-pre--release-8f8d9c" alt="status: pre-release">
</p>

# radian

radian is a coding agent in one Rust binary, and the agent runtime it is built on.

- `radian` opens a full-screen terminal UI. `rad` is an alias.
- `radian exec` runs headless. `radian exec -p "<task>"` runs one task and exits. `--output jsonl` prints each step as one JSON line, and the exit code tells how the turn ended.
- Each session is one JSONL journal file. Both modes can resume a session, fork it at a turn boundary, or watch it read-only with `radian attach`.
- The sandbox is off by default. `--profile workspace` turns it on (macOS seatbelt or Linux bubblewrap). A project can save the setting for itself, and the setting can only narrow access.
- Providers: OpenAI-compatible Chat Completions and the Anthropic Messages API. Tools: built-in file, search, shell and job tools, MCP stdio servers, skills, memory, and a one-shot `subagent`.
- Inside [herdr](https://github.com/ogulcancelik/herdr), radian reports its state (working, idle, blocked) by itself.

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/readme/terminal-dark.png">
    <img src=".github/readme/terminal-light.png" alt="the radian TUI at 120 columns: a thinking block, a shell call, a read, a patch with its diff, job notices, the input and the footer" width="820">
  </picture>
  <br><sub>radian at 120 columns, rendered by the TUI prototype script (<code>research/2026-10-16-render-prototype/</code>).</sub>
</p>

## Install

Install the current release from Homebrew, from a release tarball, or from
source with Cargo.

```sh
brew install hisea/tap/radian
```

The tap links only `radian`: homebrew-core already has a `rad` formula, so
this one does not link its `rad` alias. The macOS packages are signed and
notarized.

Or download `radian-<version>-<target>.tar.gz` from the
[releases page](https://github.com/hisea/radian/releases), check it against
its `.sha256`, and unpack the `radian` (and `rad`) binary inside:

```sh
shasum -a 256 -c radian-<version>-<target>.tar.gz.sha256
tar -xzf radian-<version>-<target>.tar.gz
./radian --version
```

Or build from source (this installs both `radian` and `rad`):

```sh
cargo install --git https://github.com/hisea/radian radian-tui
```

On Linux the `workspace` tier runs the sandbox through `bubblewrap`;
install it first (for example `sudo apt install bubblewrap` on Debian
and Ubuntu).

New users start with [`docs/user/quickstart.md`](docs/user/quickstart.md). The design is in [`ARCHITECTURE.md`](ARCHITECTURE.md), and the decisions that constrain the code are in [`docs/adr/`](docs/adr/). Read [`AGENTS.md`](AGENTS.md) before you change code. How to contribute is in [CONTRIBUTING.md](CONTRIBUTING.md). To report a security problem, see [SECURITY.md](SECURITY.md).

## Crates

| Crate | What it is | Detailed docs |
|---|---|---|
| `radian-core` | The runtime itself: turn loop (`Agent`/`AgentSession`/`ActiveRun`), messages, tools, hooks, steering, the journal seam, events, context budget estimation. | `crates/radian-core/README.md` |
| `agent-extensions` | MCP stdio servers exposed as `radian-core` tools, under stable `server__tool` aliases. | `crates/agent-extensions/README.md` |
| `agent-provider-contract` | Adapter-neutral contract suite: the semantics every `Model` implementation must satisfy, independent of wire format. | `crates/agent-provider-contract/README.md` |
| `agent-provider-openai-compatible` | Model adapter for OpenAI-compatible Chat Completions (`/chat/completions`), streaming. | `crates/agent-provider-openai-compatible/README.md`, `crates/agent-provider-contract/README.md` |
| `agent-provider-anthropic` | Model adapter for the Anthropic Messages API (`/v1/messages`), streaming, API-key auth, extended thinking. | `crates/agent-provider-anthropic/README.md`, `crates/agent-provider-anthropic/WIRE.md` |
| `radian-session` | Session JSONL v1 storage: the durable `TranscriptJournal` seam, the reader/replay path, the disposable SQLite index and the export renderers. | `crates/radian-session/README.md` |
| `radian-runtime` | The layer above the turn loop: a durable, bounded inbox (`followup`/`steer`/`inject`/`cancel`) and the per-session driver. | `crates/radian-runtime/README.md` |
| `radian-permissions` | The permission layer: `ApprovalPolicy` + `ToolRuleSet` as one `Hooks` implementation; decides allow / ask / deny. It does not enforce anything. | `crates/radian-permissions/README.md` |
| `radian-sandbox` | OS sandbox backends (macOS `sandbox-exec`, Linux `bubblewrap`) that build a command under a policy and **refuse to run** without a proven backend. | `crates/radian-sandbox/LIMITS.md` |
| `radian-coding` | Built-in coding tools — `read` (files and directories), `write`, `patch` (single or atomic multi-edit), `glob`, `grep`, when the host supplies a workspace; `shell` and the `job` tool additionally require a sandbox. The model runs git through `shell` (hardened via `GIT_CONFIG_*` in untrusted projects). A truncated foreground result registers under a normal job id; retention additionally requires a session-private output directory. Workspace path confinement applies throughout. | `crates/radian-coding/README.md` |
| `radian-memory` | The memory storage layer: Global/Project scope logs with replay, the source rules (an agent cannot touch a user record), Forget/Purge, the `projects.jsonl` registry and the Markdown import/export view. Host-free: the caller passes `~/.radian/memory/`. | `crates/radian-memory/README.md` |
| `radian-host` | Host plumbing shared by the TUI and the headless command so they cannot drift: `--header` parsing, session ids, the `--mcp-config` file, model construction, the startup description, the "built-in tools + MCP" provider, the `~/.radian/` layout and diagnostics, the model catalog, and the memory layer (`MemoryService`, project identity, the agent memory tools, injection, and the shared `radian memory` / `/memory` commands). | `crates/radian-host/README.md` |
| `radian-exec` | The library behind `radian exec`: the line-prompt chat client with optional built-in tools, MCP servers, session-management commands and the `radian memory` commands. | `crates/radian-exec/README.md` |
| `radian-tui` | The `radian` binary: a full-screen TUI (full screen only, no inline mode) over `radian-runtime`, with a slash-command layer (`/help`, `/memory …`, `/access`, `/jobs`) and its own transcript scrolling. | `crates/radian-tui/README.md` |

Two more binaries exist but are not part of the product surface: `agent-extensions`'s
`mcp-fixture` (a hand-written MCP server for its tests) and `radian-sandbox`'s `sandbox-escape`
(requires `--features testing`).

## Status

- **The `~/.radian/` layout** is implemented: `config/` and `cache/` are readable
  by the sandbox; `sessions/` and `memory/` are denied. `RADIAN_HOME` overrides the
  root. (`--session-dir` / `RADIAN_SESSION_DIR` still override only the sessions directory.)
- **Memory** is implemented: the `radian memory` CLI commands, the TUI `/memory`
  slash command (same commands), the agent's `memory_search` tool (search by keyword, or read
  records by id; mounted only past the injection budget), and deterministic injection
  with the `MemoryInjected` audit event.
- **The configuration file** is implemented read-only: `--config`/`RADIAN_CONFIG`/
  `~/.radian/config/config.toml`, with `model.*`, `permissions.profile` and `compaction.auto`
  wired as defaults (the command line wins). It never contains a secret value.
- **Pi configuration migration (Phase 5)** is implemented: `radian config import-pi --from
  <pi-agent-dir>` drafts a Radian configuration (stdout) plus a mapping report (stderr). It
  reads only the directory you name, never `auth.json`, never a secret value, and writes
  nothing without `--write` (which refuses an existing file). Radian does **not** promise
  compatibility with Pi's file format: see [`docs/user/pi-migration.md`](docs/user/pi-migration.md).
- **Session fork** is implemented: `radian exec --fork <parent> [--at <seq>]
  [--session-id <child>]` and the TUI `/fork`. The fork point is a parent turn boundary; the
  child materializes the parent's effective history there (a committed compaction in effect, an
  uncommitted replacement dropped), is written before the parent records it, and can resume or
  fork again.
- **Automatic compaction** is implemented: the proactive budget path, the
  provider-overflow fallback (compact to half and retry once), and a manual `/compact` in both
  hosts. `--no-auto-compact` turns off the proactive path only; the overflow fallback still
  runs. The model catalog and the context-budget projection are its inputs.

## Build

The workspace MSRV is **Rust 1.99.1**. Debug build:

```sh
cargo build --workspace --locked
```

Release build:

```sh
cargo build --workspace --locked --release
```

The binary lands in `target/debug/` (or `target/release/`) as `radian` (with a `rad` alias);
`radian exec` is its headless command (the `agent` binary is gone).
The full verification suite an agent must run before calling a change done is in `AGENTS.md`.

## Run

Both hosts read the API key **only** from an environment variable — never from the command line
— and never print it. The default variable is `$OPENAI_API_KEY` for `openai-compatible` and
`$ANTHROPIC_API_KEY` for `anthropic`; `--api-key-env` names a different one. A session is one
`<session-dir>/<session-id>.jsonl` file; the session directory defaults to
`--session-dir`, else `$RADIAN_SESSION_DIR`, else `~/.radian/sessions` (`$RADIAN_HOME/sessions`
when `RADIAN_HOME` is set). The same `~/.radian/` root also holds `config/`,
`cache/` and `memory/`; the sandbox denies read on `sessions/` and `memory/`.

### `radian exec` (line prompt)

```sh
export OPENAI_API_KEY=sk-...

# One turn, a new session named `demo`, provider and tier named explicitly.
./target/debug/radian exec \
  --provider openai-compatible \
  --model gpt-5 \
  --base-url https://api.openai.com/v1 \
  --profile workspace \
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
  --profile workspace \
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
# The key comes only from ~/.radian/credentials/auth.json, e.g.
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

## Configuration file

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
profile = "workspace"        # workspace | full

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
- The file is **sandbox-readable**, which is exactly why it holds references, never
  keys.

## Permission tiers

`--profile` picks all three layers at once: the sandbox (the enforcement boundary),
the rule set (allow / ask / deny) and what the host mounts. The names the binaries accept are
`workspace` and `full`; the old `workspace-write` and `full-access` remain
accepted as aliases but are not listed in `--help`. **`full` is the default**: the
neighbouring harnesses run unsandboxed by default, and a sandbox is one flag away
(`--profile workspace`). A project can turn it on for itself in
`<workspace>/.radian/config.toml` (`[permissions] profile = "workspace"`); that
declaration can only **narrow**, never widen.

| Tier | Sandbox | Shell | MCP | Built-in files |
|---|---|---|---|---|
| `workspace` | enforced: macOS seatbelt or Linux bubblewrap, probed at startup | offered, **inside** the sandbox | asks | `write`/`patch` allowed only when the resolved path stays under the workspace |
| `full` | none — the default | offered, **not** sandboxed, no prompts | allowed | `write`/`patch` as `workspace` |

- **A failed probe refuses to run.** If the backend is missing or its probe fails, the host
  exits with an error instead of falling back to an unsandboxed run. There is no degrade path. Try `RADIAN_SANDBOX_PROBE_FAIL=1` to exercise it; it can only
  cause a refusal.
- **Without `--workspace`** no write or execute tool exists at all: the built-in tools are only
  mounted with a workspace, and `shell` additionally needs a sandbox.
- **Under `workspace`**, writes are confined to the workspace, `.git` stays writable except the
  entries git can execute (`config`, `config.worktree`, `hooks/`, submodule and worktree
  equivalents), sensitive directories (`~/.ssh`, `~/.aws`, `~/.gnupg`, browser profiles, …) are
  not readable by default, and the network is on (outbound and local-loopback
  connect/bind/listen) with the pty devices available, so a project's own tests can run. This is **not** a claim of credential isolation: the macOS backend leaves
  `mach-lookup` open, so the IPC boundary is not tightened. MCP server processes are started
  outside the sandbox in this build, and the startup notice says so.
- **`--yes` is not "allow everything".** It swaps the approver for one that answers "allow
  once" without showing the question; `deny` rules still deny, `allow` rules still allow, the
  sandbox is unchanged, and every decision is still audited. `radian` also has
  `--approval on-ask|always|never` (default `on-ask`).
- **`full` trusts your environment.** It asks no built-in prompts and does not defend against
  prompt injection from an unfamiliar repository — a malicious skill, config or context file
  runs unimpeded. For a boundary use `workspace`, a container, or a dedicated OS user.
- **Audit**: every decision writes one `PermissionResolved` event to the session file
  (`approver` = `policy` for a rule or grant, `user` for an answered prompt).
- Approval shows the tool and its **resolved** resource; `y` allows once, `a` allows for the
  session (in memory only, gone after a resume), anything else denies. `--allow-shell-command
  PREFIX` allows one *simple* command prefix without asking; a compound command is allowed
  only when **every** segment is allowed (a user rule or the built-in read-only table),
  otherwise it still asks.
  Write these at the subcommand level, never at a bare program name.

Common exit codes (unified, plus the turn-outcome codes for `radian exec`): 0 success (the turn completed), 1 this turn/run failed, 2 a usage, setup
or startup refusal (clap's own errors, a bad config, an unknown keybinding, a refused startup,
a failed sandbox probe), 3 the budget ran out (`BudgetExceeded`, deadline exceeded), 4 the answer
was unfinished (`Incomplete`, `Refused`), 101 the app panicked, 143 on SIGTERM (128 + 15), and
130 for a requested cancellation or a forced exit after a second Ctrl-C. `radian exec` and the TUI share the codes.
`radian exec --list | head` (and every print-and-exit mode) still exits 0
silently on a broken pipe.

## Docs

Start with `docs/user/` for the user manual; `docs/adr/` holds the decisions; `ARCHITECTURE.md` is the design of record; `research/` is the lab notebook.

| What | Where |
|---|---|
| Design of record | `ARCHITECTURE.md` |
| Decisions (read before changing code) | `docs/adr/` |
| Sandbox backends: what they really enforce | `crates/radian-sandbox/LIMITS.md` (spikes: `spikes/macos-sandbox/README.md`, `spikes/linux-sandbox/README.md`) |
| Provider contract and its escape hatches | `crates/agent-provider-contract/README.md`, wire notes in `crates/agent-provider-anthropic/WIRE.md` |
| Session format fixtures and their checkers | `docs/schema/` (`check-fixtures.sh`, `check-config-fixtures.sh`, `fixtures/`, `config/`) |
| Dogfooding findings (radian run on radian) | `research/2026-10-12-radian-tui-dogfooding.md`, `research/2026-10-12-opencode-zen-dogfooding.md` |
| Performance baseline and its reproduction commands | `benchmarks/perf-baseline.md` |
| Spike reports (sandbox, TUI, OAuth) | `spikes/` |
| Agent instructions (verify suite) | `AGENTS.md` |

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. Third-party code and its attribution are listed in [NOTICE](NOTICE).
