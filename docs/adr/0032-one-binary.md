# ADR 0032: One Binary (`radian` and `radian exec`)

Status: accepted

## Decisions

### A1 One binary, mode by subcommand and flags

One binary `radian` exists. `rad` stays an alias. No argument opens the fullscreen TUI. `radian exec` is headless/line mode:

```text
radian                        fullscreen TUI (default)
radian exec                   interactive line mode
radian exec "…" / -p "…"      one-shot: run one turn plus the chain it wakes (with background jobs wait for completion, see ADR 0044 §2a), stdout carries only model text
radian exec -p "…" --image PATH  the same, with each image attached to the prompt's user message in order (repeatable)
radian exec <other flags and subcommands>
radian acp                    Agent Client Protocol over stdio: one session, prompts and approvals over the protocol (ADR 0049)
radian arc                    the arc TUI: plans and tasks across worker processes (ADR 0048)
radian arc <subcommand>       one arc CLI operation (ADR 0048)
radian login | logout | memory | config import-pi | trust | theme | footer | docs   management commands at top level
```

- The default serves only "a human opens a terminal". Line mode needs explicit `exec` (or `-p`/subcommand).
- No `agent` binary exists. Docs, scripts, tests, and AGENTS.md use `radian exec …`. The `rad` alias stays.
- No change to `radian-core` exists. Only host and `radian-host` change. Core interfaces stay unchanged.

### A2 Where each function lives

- **Kept**: fullscreen TUI (default), `exec` line mode and `-p`, `--image <PATH>` (repeatable, with `-p`) whose path the host reads itself, `--list`/`--search`/`--export`/`--fork` and output options, TUI `/…` commands, TUI `--pick` and test hooks, and the `acp` host (ADR 0049), which shares `radian exec`'s conversation flags.
- **Top level**: `memory`, `config import-pi`, `login`/`logout`, `trust`, `theme import`, `footer export`, `docs [topic]`. They touch no model. They open no session.
- **`arc`**: `radian arc` opens the arc TUI and `radian arc <subcommand>` runs the arc CLI (ADR 0048). arc opens no session in its own process. It starts each worker as a separate `radian exec` process. Shared names match TUI `/login`, `/memory`, and similar.
- **`exec` and TUI share** `radian_host::open_session`, config read, `~/.radian/` layout, tool set, auth, and session data layer.
- **Not merged**: line-mode terminal approval (blocking line read) and TUI pending queue plus modal (non-blocking UI queue) are two necessary shapes (ADR 0023 U7). They share only rules and policy (already in `radian-permissions`).

### A3 Exit codes, stdout/stderr

| Mode | stdout | stderr | Exit code |
|---|---|---|---|
| Fullscreen TUI | The terminal itself (alternate screen) | Startup and diagnostic lines, panic recovery | 0 clean, 1 driver error, 101 panic, 128+signal |
| `exec` line mode and `-p` | **Only model text** (`-p` whole block; line mode streams; `--output jsonl` see ADR 0044 §1) | Status, reasoning, errors | Exit code by outcome, see ADR 0044 §3 (2 usage/setup, 130 second Ctrl-C unchanged) |
| `--list`/`--search`/`--export`/`memory`/`config`/`trust`/`theme`/`footer`/`docs` | The result itself | Reports and hints | 0; usage 2; real error 1; stdout broken pipe 0, silent |

- Usage and setup errors exit 2 in every mode. This includes TUI startup refusal and a `--image` path that is not a PNG/JPEG/GIF/WebP image, is over 5 MiB, or has a side over 8000 px: the message names the path and the reason. Clap usage errors already exit 2. 0 means normal. 1 means turn failure. 101 means panic. 128+signal stays unchanged.
- The `-p` stdout contract stays unchanged: only model text. `radian exec -p … > out.txt` stays stable. Status lines use stderr.
- `--export --format md|json`, `--search`, and `--list` line formats stay unchanged. Entry points move. Formats stay unchanged.

### A4 Tests

Tests still drive the **real binary**: `radian-exec` end-to-end tests drive `radian exec`. TUI pty tests already drive `radian`. Old `agent` behavior cases stay covered (especially default line mode, streaming output, per-step approval, exit-code-2 cases). No test-only binary exists for compatibility.

## Constraints and Costs

- One binary carries larger dependencies: TUI and line-mode code ship together. One path, one test driver, and one dedup point pay back the cost.
- `--help` grows longer. `help_heading` groups it (TUI/exec/session/model/mcp/permissions).
- One behavior fact: TUI startup refusal exits 2. README and `--help` record it.

## Verification

`radian-exec` end-to-end tests cover `radian exec` modes, exit codes, the stdout/stderr contract, and broken pipe. `radian-tui` pty tests cover no-argument TUI entry, `exec --line` behavior, and management commands. `cargo test --workspace` runs.
