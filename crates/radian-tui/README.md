# radian-tui

Phase 4 / A: the full-screen TUI shell on `radian-runtime`. **Full screen only** (ADR 0009):
the alternate screen is used, so scrolling, selection and copy are ours to build rather than
the terminal's. This slice is the skeleton: one real conversation, a transcript view that
scrolls itself, streaming text and reasoning, and clean terminal recovery.

> **Do not run this in a shared terminal by hand** unless you accept that it takes over the
> screen. The tests never do: they drive the binary through `portable-pty` and read a `vt100`
> screen model, or use `ratatui`'s `TestBackend`.

## Providers

`--provider` picks the wire protocol; the default is unchanged.

- `openai-compatible` (default): `/chat/completions`; `--base-url` defaults to
  `https://api.openai.com/v1`, the key comes from `$OPENAI_API_KEY` unless `--api-key-env`
  names another variable.
- `anthropic`: the Messages API (`/v1/messages`); `--base-url` defaults to
  `https://api.anthropic.com`, the key comes from `$ANTHROPIC_API_KEY` unless `--api-key-env`
  names another variable. `--max-tokens` (default 16384) is required by the Messages API, and
  `--thinking off|adaptive|enabled:<budget>` enables extended thinking (default `off`).

The key is read only from the environment variable and is redacted from the panic log along
with every header value; it is never written to the session file. The config snapshot names
the provider and the model only.

## Layout and keys

```
┌──────────────────────────────────────────────────────────────┐
│ transcript: user >, assistant, ~ reasoning, -> tool, <- result│
├──────────────────────────────────────────────────────────────┤
│ message  (type here)                                          │
└──────────────────────────────────────────────────────────────┘
 model │ idle/running/cancelled │ tier │ usage in/out │ mcp servers/tools │ context │ scroll N/M
```

| Key | Action |
|---|---|
| Enter | send. Idle → `Inbox::followup`; while a turn runs → `Inbox::steer` |
| Ctrl-J (or Shift-Enter / Alt-Enter) | insert a newline in the input |
| Ctrl-C | cancel the running turn; when idle, quit |
| Esc | same as Ctrl-C |
| PageUp / PageDown / Up / Down | scroll the transcript; Home/End jump |
| bracketed paste | the whole multi-line paste goes into the input box |

Shift-Enter needs a terminal with the enhanced keyboard protocol; Ctrl-J works everywhere.

## Slash commands (`/`)

A submitted line whose first non-space character is `/` is a **command**: it never reaches the
model, the transcript or the session journal. `//text` is the escape — one `/` is dropped and
the rest is sent as an ordinary message.

| Command | What it does |
|---|---|
| `/help` | list the commands |
| `/compact [note]` | summarize the history now and keep the recent part (ADR 0018) |
| `/memory <command> …` | the same durable-memory commands as `agent memory` (ADR 0021 D7): `list`, `search`, `show`, `add`, `edit`, `archive`, `restore`, `forget`, `purge`, `confirm`, `export`, `import`, `project` / `project new`, `relink`, `split`, `merge` |

- Arguments are split with shell quoting, so `/memory add "two words"` is one argument.
  Unbalanced quotes are an error notice.
- The output is shown as transcript notices (scrollable); `--json` is shown verbatim.
- `forget`, `purge`, `split` and `merge` ask first: a notice shows the prompt, `y` runs it and
  `n`/`Esc` cancels. While the prompt is up, keys do not type into the input box.
- A **read** command runs while a turn is running and does not interrupt it; a **write** takes
  effect at the next model request (ADR 0021 D6).
- `/compact [note]` asks the runtime to summarize the history now (ADR 0018); the driver runs
  it between turns, so a running turn finishes first, and the result is a notice. Ctrl-C/Esc
  cancels an in-flight compaction without committing.
- An unknown command is an error notice, never a message to the model.
- The grammar and the execution are `radian_host::memory::commands`, shared byte-for-byte with
  the CLI; the TUI only adds the framework and the notices.

## What the screen shows

- **Streaming text** (`ModelTextDelta`) appears as it arrives.
- **Reasoning** (`ModelReasoningDelta`, ADR 0014) is rendered dim + italic with a `~ ` prefix,
  separate from assistant text. It is never sent back to a provider (R4).
- A committed assistant message **replaces** the live delta view, so a dropped delta cannot
  leave a duplicate.
- An **interrupted** assistant prefix (ADR 0003) shows a yellow `[interrupted]` marker and
  italic body text.
- Tool calls (`->`) and tool results (`<-`) each get a line.

## Scroll-follow rules

The transcript owns its scrolling:

1. While the view is pinned to the bottom, new content **follows**.
2. Scrolling up sets `follow = false`. New content then **never** drags the view back: the
   viewport stays on the same lines.
3. Returning to the bottom (End, or scrolling down to it) sets `follow = true` again.

## Tests

| # | Test | File |
|---|---|---|
| 1 | reasoning vs assistant distinguishable + interrupted marker + tool display | `tests/render.rs` |
| 2 | scroll follow / pause / resume | `tests/scroll.rs` |
| 3 | pty end-to-end conversation, clean exit, terminal restored, trailing newline | `tests/pty.rs` |
| 4 | panic path restores the terminal | `tests/pty.rs` |
| 5 | multi-line bracketed paste | `tests/pty.rs` |
| 6 | reasoning not sent back to the provider (ADR 0014 R4) | `tests/pty.rs` |
| 7 | panic path leaves no orphan child (child in its own process group) | `tests/pty.rs` |
| 8 | `--resume` renders the stored history; `--list` prints the workspace sessions | `tests/pty.rs`, `tests/list.rs` |
| 9 | approval: allow-once runs the tool, deny is fed back, always skips the next prompt, Ctrl-C cancels, the resolved path is shown, `PermissionResolved` is durable | `tests/pty.rs`, `tests/approval.rs` |
| 10 | driver task panic: orderly exit, terminal restored, failure reported | `tests/pty.rs` |
| 11 | a failed sandbox probe refuses instead of running unsandboxed | `tests/pty.rs` |
| 12 | shell approval shows the whole command; a capped result keeps the truncation marker visible; a compound command always asks | `tests/pty.rs` |
| 13 | an over-budget request is still sent and the turn completes; the status bar shows `T / B` and the honest wording; no window says the budget is off | `tests/pty.rs`, `tests/budget.rs` |
| 14 | `--header` `{session_id}` expands for new and resumed sessions; `authorization` and a malformed header are refused without echoing the value | `tests/pty.rs` |
| 15 | `--mcp-config` mounts the fixture; its tool runs through the approval dialog; the status bar shows the connected servers/tools; the startup notice names the MCP sandbox gap | `tests/pty.rs` |
| 16 | the status bar shows the tier, reported usage (or `none`) and the MCP counts; a 60-column terminal drops the long segments and marks it | `tests/status.rs` |
| 17 | `--version` prints `<program> <CARGO_PKG_VERSION>` and exits 0 without entering the terminal | `tests/version.rs` |
| 18 | an `@path` reference attaches the file to the request and the transcript folds it to `[file: P, N lines]`; a binary attachment is not sent and the input is kept | `tests/pty.rs`, `tests/render.rs`, `src/mention.rs` |
| — | Enter=followup idle / steer running; Ctrl-C cancel-then-quit | `tests/input.rs` |

## Must be hand-tested in a real terminal

pty automation uses a `vt100` screen model, which answers queries instantly and does not
implement a real emulator's glyph metrics. **It cannot judge these** (ADR 0009's lesson):

- scroll feel: does PageUp/PageDown move a comfortable amount? does follow/pause feel right?
- reasoning readability: is dim+italic `~` actually distinguishable, or too faint? does it
  deserve collapsible "thinking cards" (Phase 5)?
- resize: drag the window while a turn streams — no residue, status line keeps working?
- CJK and emoji width: alignment of a pasted Chinese line and an emoji.
- paste: a large multi-line paste into the box.
- **the approval box**: is the resolved path and the argument preview readable at your
  terminal width? does a long argument (a large patch, say) make the box unusable? do the
  `y`/`a`/`n` keys feel right, and is it obvious which one you are about to press? pty
  automation asserts the text and the keys, not the readability or the feel (ADR 0009).
- **the shell approval box with a long command**: a real command line is often longer than a
  file path. Does the `command:` line stay readable at your width, or does the wrapped preview
  become a wall of text? The pty test only asserts that the whole command is present.
- **the budget status segment**: with a small `--context-window`, is the `T / B` projection
  and the reported-vs-estimated wording readable at your width, and does the over-budget
  highlight stand out without looking like an error?
- **mouse selection/copy**: this app does **not** enable mouse capture, so selection is the
  **terminal emulator's own** (drag to select, Cmd-C on macOS, or the emulator's copy key).
  The caveat ADR 0009 notes applies: a selection that spans the input box and the transcript
  follows the emulator's rules, not ours. Verify it selects what you expect.
- **the MCP approval box**: with `--mcp-config`, an MCP tool call prompts like any other. Is
  the tool name (`server__tool`) and the argument preview clear at your width, and is it
  obvious the server runs outside the sandbox (the startup notice says so)?
- **`/help` and a `/memory` write**: type `/help` (a notice, no model request), then
  `/memory forget <id>` — is the `[y/N]` prompt obvious and does `n` feel like a cancel? pty
  automation asserts the notices and the keys, not the readability.
- **the status bar at your usual width**: is the tier / usage / mcp / context order readable,
  and does the truncation (`…`) drop what you would have dropped? The pty and `TestBackend`
  tests assert the text and the 60-column rule, not the feel.

Suggested 3-line manual pass (in a throwaway terminal, not your working one):

```sh
cargo run -p radian-tui -- --model <model> --base-url <...> --session-dir /tmp/radian-tui-manual
# 1. send a message; while it streams, PageUp and keep sending: the view must not jump back.
# 2. End; a new reply should follow. Paste a 5-line block and check it lands in the box.
# 3. drag-select some transcript text and copy it; then Ctrl-C twice and confirm the prompt
#    is clean (no `%`) and `stty -a` still shows `icanon`.
```

### Owner hand test, 2026-10-12: passed

The owner ran the slice A and slice B checks above in a real terminal on macOS, against the
opencode Zen endpoint (`deepseek-v4.1-flash`) with `--approval always`. The terminal emulator
was not recorded. **All items passed**:

- streaming; reasoning readable apart from the answer; PageUp during a stream stays put;
- the approval box (resolved path, allow once, allow for the session, no prompt after a
  session grant); Ctrl-C cancels a running turn; resize; CJK; multi-line paste; mouse copy;
- Ctrl-C twice exits with a clean prompt;
- `--resume` shows the history, the model remembers it, and a session grant does **not**
  carry over the resume (the box asks again).

The owner has usability feedback on the UI that is not recorded yet; it comes later.

Gap found while preparing the test: the TUI's `--header` took a literal value and had no
`{session_id}` placeholder, unlike `agent-cli`. **Fixed in slice D**: both hosts now use
`radian-host`'s parser, so `{session_id}` expands (for `--session-id` and `--resume`),
`authorization` is refused, names are validated, and a malformed argument never echoes its
value.

## SIGTERM and SIGHUP leave like a quit (2026-10-13, dogfooding D7)

A SIGTERM used to kill the app mid-frame: alternate screen and raw mode stayed on and nothing
said why. `signals::listen` now records SIGTERM/SIGHUP; the UI loop sees it on its next tick
and takes the ordinary quit path, so the terminal is restored and `main` prints
`received SIGTERM; session <id> saved; resume with --resume <id>` and exits with 128 + the
signal (143, 129). Tested by `pty_sigterm_and_sighup_restore_the_terminal_and_say_why`; not
starting the listener makes it fail.

The panic hook no longer chains to Rust's default hook, which printed the raw panic text to
stderr: stderr now gets the same redacted report as the panic log
(`pty_a_panic_writes_a_redacted_panic_log` asserts neither the header value nor the key reaches
the terminal). The cost: `RUST_BACKTRACE` is not honoured.

## The panic path unwinds, with a bounded runtime shutdown (2026-10-12)

Slice A's panic hook restored the terminal and then called `process::exit(101)`, which skips
every `Drop`. That was safe only while the TUI owned no child processes. Slice B wires the
built-in file tools, whose `git_status`/`git_diff` spawn `git`, so the precondition no longer
holds.

`main` now keeps the runtime outside `run`, catches the panic with `catch_unwind`, shuts the
runtime down with `shutdown_timeout(2s)` — dropping the in-flight tool futures and, with them,
the `kill_on_drop` children they own — and only then exits 101. The hook restores the terminal
and returns; it no longer exits.

Why this rather than "kill the registered process groups inside the hook": the TUI does not
know the pids (they are created inside `radian-coding`), and registering them would mean
changing that crate's internals. Unwinding plus a bounded shutdown reuses the `kill_on_drop`
contract the tools already rely on, and `pty_panic_path_does_not_orphan_a_child` exercises it:
a child in its own process group is running when the panic fires, and the test asserts both
the pid and the group are gone.

Durable data was already safe (every append is fsynced, ADR 0001 D3; a torn tail is truncated
on read, ADR 0006 §3.2) and stays so.

## A panic in the driver task leaves in order (2026-10-12)

The driver runs as a **spawned** task. A panic inside it is caught by the runtime, not by
`main`'s `catch_unwind`, so the process keeps running — and the old panic hook restored the
terminal anyway, leaving the UI drawing in cooked mode over a normal screen and sending input
to a closed inbox.

Two changes fix that:

- The panic hook restores the terminal **only when the panicking thread is `main`** — the
  process-going-down case. A spawned task's panic is not fatal, so the UI keeps the terminal.
  The hook still never exits; `main` catches the unwind, shuts the runtime down with a bound
  (dropping in-flight tool futures and their `kill_on_drop` children) and exits 101. The main
  thread's unwind drops `TtyGuard` regardless, so the hook's restore is only about making the
  panic visible on the normal screen.
- The UI loop watches the driver task (`JoinHandle::is_finished`) and leaves as soon as it
  ends, whatever the reason: it draws a final notice, restores the terminal, and `run` joins
  the task. A `JoinError` (the panic case) becomes exit 1 with a message; the panic hook
  records the panic text, which is printed **after** the alternate screen is gone, so it is
  readable instead of scrolling away inside the alternate screen.

`pty_a_driver_panic_exits_orderly_and_restores_the_terminal` asserts the orderly exit (code 1,
not 101), the restored terminal (`?1049l`, `?2004l`, `?25h`, `icanon`) and that the failure is
reported on the normal screen. Durable data was already safe and stays so.

## Exit reasons and the panic log (2026-10-13, dogfooding D4)

Whatever ended the app, the normal screen now says why, once, after the terminal is restored
(the alternate screen is gone by then, so it is readable and stays in scrollback):

| How it ends | Exit | Reason line (stderr, after restore) |
|---|---|---|
| clean quit (Ctrl-C twice / driver ends by request) | 0 | `session <id> saved; resume with `--resume <id>`` |
| startup/config refusal (bad workspace, failed probe, bad MCP file, bad header, ...) | 1 | `radian-tui: <the error>` (terminal never entered) |
| driver task ends with a `RunError` | 1 | `radian-tui: driver: <error>` |
| driver task panics | 1 | `radian-tui: the driver task ended unexpectedly: <join error>: <redacted panic>` |
| the app itself panics (`main` thread) | 101 | `radian-tui: the app panicked: <redacted panic> (panic log: <path>)` |
| `--list` | 0 | none: the listing is the output |

Every line is a single `eprintln!` and therefore ends in `\n`; `TtyGuard::restore` also writes
`\r\n` after leaving the alternate screen, so the shell never prints its "no newline" `%`.

A panic is also appended to the **diagnostics directory** (ADR 0013 decision 4; the path is
ADR 0020), named `<session-id>-<unix-millis>.panic.log` (a `radian_session::now_utc()`
timestamp, the thread name, the panic text), with header values and the API key replaced by
`<redacted>`. It is `~/.radian/cache/diagnostics` (`$RADIAN_HOME/cache/diagnostics`). The
directory is `0700` and the
file `0600`; files older than 7 days are removed, and the total is capped at 50 MiB by dropping
the oldest first — at startup and again before each write. The session directory is never
touched by this cleanup.

The log is set once the session exists and before the terminal is entered
(`terminal::set_panic_log`); a panic before that has no log. The exit line names the log path.
A diagnostics write or cleanup that fails is one `[diagnostics]` line on stderr, never a fault:
the run continues and the panic still exits 101. Tested by
`pty_clean_exit_prints_one_reason_line`, `pty_a_panic_writes_a_redacted_panic_log`,
`pty_panic_path_restores_the_terminal` and
`pty_an_unwritable_diagnostics_directory_is_only_a_notice`.

## Startup order (2026-10-13, dogfooding D1/D2)

Everything that can fail runs **before** the session file is created, so a failed start leaves
the session directory untouched and the same `--session-id` can be retried:

0. `--version` is answered by the argument parser and exits 0 before any of this runs;
1. session directory and id resolution; id validation;
2. `--header` parsing (values collected for panic-log redaction);
3. workspace validation (`radian_host::resolve_workspace`: exists, is a directory, readable,
   canonical) — a bad path is a **workspace** error, never a sandbox message and never a
   `full` suggestion;
4. model configuration (`--base-url`, key policy);
5. tier parsing, permission rules, sandbox probe (a failed probe refuses, never downgrades,
   ADR 0012 decision 2);
6. `--mcp-config` file read and validation;
7. built-in tool construction;
8. **only now** `open_journal` creates (or resumes) the session file.

MCP server **processes** still start when the tool session opens (step 8's `Driver::open`). If
that fails the session file is kept — its durable content is real — and the error says how to
continue it: `... the session file <path> exists; after fixing the problem, continue it with
`--resume <id>``. The CLI behaves the same way. Tested by
`a_missing_workspace_is_a_workspace_error_and_leaves_no_session`,
`pty_a_missing_workspace_is_a_workspace_error_and_leaves_no_session`,
`a_failed_probe_leaves_no_session_and_the_retry_creates_one`,
`pty_a_failed_probe_leaves_no_session_and_the_retry_creates_one`,
`a_bad_mcp_config_leaves_no_session`, `pty_a_bad_mcp_config_creates_no_session`.

## Not verified

- Real terminal emulators beyond the owner's one macOS pass above (Terminal.app, iTerm2,
  Alacritty, tmux, ssh): layout, frame timing and glyph width are otherwise only checked
  against a `vt100` model.
- Frame timing under load (the spike measured p95 ≪ 16 ms; this skeleton was not benchmarked).
- `Shift-Enter` on terminals without the enhanced keyboard protocol.
- Windows and Linux ptys; only macOS was exercised.
- Large transcripts: virtualized since 2026-10-13 (dogfooding D9; `tests/virtual.rs`, frame times in `docs/perf-baseline.md` §4). Not done: the first frame after resuming a large session still measures every entry once.

## Sessions

The session directory and id convention is **the same as `agent-cli`**, so a session written
by one is resumable by the other:

- `--session-dir DIR`, else `RADIAN_SESSION_DIR`, else `~/.radian/sessions`
  (`$RADIAN_HOME/sessions` when `RADIAN_HOME` is set; ADR 0020). Never a cache or temp path: a
  session is a durable record (ADR 0013 decision 4); the sandbox denies read on it and on
  `~/.radian/memory` (ADR 0020).
- The session file is `<session-dir>/<session-id>.jsonl`; `--session-id VALUE` names it, and
  a generated `time-pid` id is used when neither `--session-id` nor `--resume` is given.
- `--resume ID` opens an existing session: its stored history is rendered into the transcript
  before the first frame, and the session keeps appending to the same file. The model's
  history comes from the journal (`radian-runtime` recovers it); the transcript view is the
  same history.
- `--list` prints this workspace's sessions (id, created, last activity, message count) and
  exits without touching the terminal.

`--resume` and `--session-id` are mutually exclusive, as are `--resume` and `--list`.

## Flags

`--model`, `--base-url`, `--api-key-env` (default `OPENAI_API_KEY`), `--header NAME=VALUE`,
`--system`, `--session-dir`, `--session-id`, `--resume`, `--list`, `--version`,
`--workspace DIR`, `--mcp-config FILE`,
`--profile safe|workspace|full` (default `workspace`; the old `workspace-write`/`full-access`
are accepted aliases but not listed),
`--approval on-ask|always|never` (default `on-ask`), `--allow-shell-command PREFIX`,
`--context-window TOKENS`, `--offline-catalog`, `--catalog-provider PROVIDER`. The key is
read only from the named environment variable and is never printed.

`--version` prints `radian-tui <version>` and exits 0 before anything else runs: no
`--model` is needed, no key is read, no session file is created, and the terminal is never
entered. The version is the package's own (`CARGO_PKG_VERSION` in `Cargo.toml`), never a
literal in the source.

`--header`, `--mcp-config` and the session-id rules are the **same code** `agent-cli` uses
(`crates/radian-host`), so the two hosts cannot drift: `{session_id}` expands to the
process's id (or the resumed one), `authorization` is refused because the key has one source,
and a malformed argument is reported without echoing its value.

## Tools, permissions and the approval dialog

`--workspace DIR` mounts the built-in file and read-only git tools (`read`, `write`, `patch`,
`list`, `git_status`, `git_diff`) rooted at that directory, plus **`shell`** when a sandbox
could be built. This is the same wiring `agent-cli` uses:

- The sandbox is built **before** the tools: `workspace` and `full` call
  `radian_sandbox::Sandbox::new`, and a missing backend or a failed probe is an **error**, never
  a fallback to an unsandboxed run (ADR 0012 decision 2). The app then exits 1 without
  entering the terminal. `RADIAN_SANDBOX_PROBE_FAIL=1` is a test seam that can only cause a
  refusal.
- `shell` exists only when a sandbox does (`radian-coding` does not advertise it otherwise), and
  it never runs without one. `safe` builds no sandbox, so it has no shell; its rules deny
  writes and execution.
- `--allow-shell-command "git status"` (repeatable) adds a host `allow` rule. Only a **simple**
  command matches: any shell metacharacter (`;`, `&`, `|`, `$`, quotes, …) keeps the call at
  `ask`, so a compound command always prompts. Write these at the subcommand level, never at a
  bare program name.
- Two startup notices state what was chosen and what it means. The meaning line follows ADR
  0012 decision 4 and does **not** claim credential isolation: the macOS backend leaves
  `mach-lookup` open, so the IPC boundary is not tightened.
- A `[memory]` startup notice names the project state (the linked project id, or "no project
  linked" with the `agent memory project new`/`relink` hint) and the injection budget
  (ADR 0021 D2/D6). The agent's four memory tools are mounted and the current memory is
  injected into the system prompt each turn; `memory_search`/`memory_get` are allowed and
  `memory_save`/`memory_revise` ask. Tested by
  `pty_the_memory_notice_names_the_project_state_and_the_budget`.

Every call goes through `radian-permissions` (ADR 0012) as a `Hooks` implementation:

- `--profile safe|workspace|full` picks the rule set (default `workspace`:
  reads allowed, writes allowed only when the resolved path stays inside the workspace, and the
  network is on).
- `--approval on-ask|always|never` picks the `ApprovalPolicy` (default `on-ask`). `always`
  asks for every call a rule does not deny, which is how the dialog is exercised with the
  built-in tools; `never` turns `ask` into a denial.
- `--max-model-calls N` is the per-turn model call limit. The TUI defaults to **1000**,
  not the core's 32: an interactive coding turn reads many files and a human can cancel, but
  it stays a fuse, because the context-budget guard does not stop a loop of small calls. When a turn hits it, the pending tool calls are answered with
  "the model call budget is used up" and the turn ends; a new message starts a fresh count.
  Tested by `pty_max_model_calls_limits_the_turn`.
- `--yes` answers every `ask` with "allow once" and shows no dialog, the same as
  `agent --yes`. It removes the question only: `deny` rules still deny, the sandbox still
  applies, every decision is still audited, and the startup says it is on. With
  `workspace` this means "the sandbox is the boundary"; with `full` there is no
  boundary left, so read the profile line before you use it. Tested by
  `pty_yes_runs_an_ask_without_a_prompt_and_still_audits_it` (a mutation that keeps the
  dialog approver under `--yes` makes it fail).
- The turn source is set to `User` before each turn; the permission layer treats an unset
  source as non-human and denies, so a forgotten setter fails closed.
- Every decision is written to the session journal as a `PermissionResolved` (audit only,
  `critical = false`, never replayed).

When a call needs the human, the input box is replaced by an approval box:

```
┌ approve tool call? ──────────────────────────────────────────┐
│ tool: write                                                  │
│ path: /abs/workspace/out.txt        <- the RESOLVED path      │
│ args: { "path": "out.txt", … }                               │
│ [y] allow once   [a] always this session   [n] deny          │
└──────────────────────────────────────────────────────────────┘
```

| Key | Answer |
|---|---|
| `y` | allow once |
| `a` | allow for this session (the same tool + resolved resource stops asking; in memory only, gone after resume) |
| `n` or Esc | deny (the model sees the denial) |
| Ctrl-C | cancel the running turn; the waiting `ask` resolves as a denial and the prompt is cleared |

The prompt shows the **resolved** resource, never the raw argument: `a/../b` must not read as
if it stayed under `a/`. While the box is up, other keys do not type into the input.

## Context budget and the model catalog

The window comes from `--context-window TOKENS` (explicit; it never fetches — an existing
cache is read only for the `input`/`output` numbers), or from the **models.dev catalog**
matched by `(provider, model)` (ADR 0018). The provider is `--catalog-provider`, or derived
from the `--base-url` hostname (`opencode.ai` → `opencode`, `api.anthropic.com` → `anthropic`,
`api.openai.com` → `openai`, `api.deepseek.com` → `deepseek`, ...). The catalog is a
read-only GET with no key, header or user data, cached under the user cache directory and
refreshed at most once every 24 h (3 s timeout; a failure falls back to the cache);
`--offline-catalog` never goes out. With neither, `C` is **unknown**, there is no budget, and
the startup notice says so.

The effective input budget is **`B = min(I, C − R)`**, with **`R = max(16384, the request's max
output)`** (ADR 0018). The startup notice names the source and every number
(`C 1000000, I 922000, output 384000, R 16384, B 922000 from models.dev (opencode/…, cached 3 h
ago)`, `B 3616 from --context-window`, or a `no budget` line). `C ≤ R` is refused at startup:
the window cannot hold the reservation.

The status bar shows the projection `T` over `B` whenever a request is made, and highlights it
when `T` is over `B`. Crossing `B` does **not** stop the turn (ADR 0018): the request is sent
anyway, and the provider's over-length error is the only refusal. The wording depends on where
the number came from (ADR 0015 C1) — a report, a report plus an estimate, and a pure estimate
are three different things:

| Source | Wording |
|---|---|
| reported | `context 12,340 (reported) / 16,000` |
| reported + estimate | `context 12,340 (reported 12,000 + estimated 340) / 16,000` |
| estimate | `context ~12,340 (estimated) / 16,000` |
| configured, no request yet | `context budget B 16,000 (no request yet)` |
| no window | `context budget off (no window configured)` — no number at all |

The mixed value is never shown as a plain report, and a projection that has no reported
baseline is marked `~… (estimated)`.

There is no fork option: an option that does nothing would be worse than its absence (Phase 5
owns fork/rewind).

## MCP servers

`--mcp-config FILE` starts MCP stdio servers from the same JSON file `agent-cli` reads:

```json
{"servers": [{"id": "fx", "command": "/path/to/server", "args": [], "env": {},
             "call_timeout_secs": 30}]}
```

- The servers' tools are mounted beside the built-in tools under `server__tool` aliases
  (`crates/radian-host`'s provider). A name collision fails the session rather than silently
  shadowing a built-in tool.
- Under `workspace` an MCP call is an `ask`: it goes through the same approval dialog
  as a built-in tool. `safe` denies MCP; `full` allows it.
- **The servers themselves are not sandboxed** (ADR 0012 option B). The `workspace`
  startup notice says so, because the notice is the only safeguard.

## Status bar

The bottom line carries, most important first:

| Segment | Shows |
|---|---|
| model | the `--model` value |
| turn | `idle` / `running` / `cancelled` |
| tier | `safe` / `workspace` / `full` |
| usage | `usage <input>/<output>` from **provider-reported** totals; `-` for a field never reported, `none` for a session with no reports |
| mcp | `mcp off`, `mcp N/…` (configured, not open) or `mcp servers/tools` (what connected) |
| context | the ADR 0015 wording above |
| scroll | `scroll N/M follow|paused` |

Usage is never estimated: if the provider reports no usage, the bar says `usage none`, and a
half-reported response shows only the half it reported.

**Narrow terminals**: segments are ordered most-important-first, and the line never wraps.
When they do not fit, the rightmost segments are dropped one at a time and a `…` marks the
truncation. At 60 columns the model, turn, tier, usage and MCP counts survive; the long
context text and the scroll position are the first to go. The last segment is never dropped.

## Tool display

A tool call shows as `-> name({…args…})`, its result as `<- <text>` (red on an error), and
the status line carries a `tool <name> running` note while it runs. The call, the wait for
approval, execution and the result are therefore distinguishable states.
