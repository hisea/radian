# radian-tui

Phase 4 / A: the full-screen TUI shell on `radian-runtime`. **Full screen only** (ADR 0009):
the alternate screen is used, so scrolling, selection and copy are ours to build rather than
the terminal's. This slice is the skeleton: one real conversation, a transcript view that
scrolls itself, streaming text and reasoning, and clean terminal recovery.

> **Do not run this in a shared terminal by hand** unless you accept that it takes over the
> screen. The tests never do: they drive the binary through `portable-pty` and read a `vt100`
> screen model, or use `ratatui`'s `TestBackend`.

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
| 13 | budget stop sends nothing and shows the honest wording; continue relaxes one request; the guard resumes; no window says the guard is off | `tests/pty.rs`, `tests/budget.rs` |
| 14 | `--header` `{session_id}` expands for new and resumed sessions; `authorization` and a malformed header are refused without echoing the value | `tests/pty.rs` |
| 15 | `--mcp-config` mounts the fixture; its tool runs through the approval dialog; the status bar shows the connected servers/tools; the startup notice names the MCP sandbox gap | `tests/pty.rs` |
| 16 | the status bar shows the tier, reported usage (or `none`) and the MCP counts; a 60-column terminal drops the long segments and marks it | `tests/status.rs` |
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
- **the budget stop notice**: when the context guard stops a turn, is the reason and the
  reported-vs-estimated wording readable at your width, and is it clear which key continues and
  which abandons?
- **mouse selection/copy**: this app does **not** enable mouse capture, so selection is the
  **terminal emulator's own** (drag to select, Cmd-C on macOS, or the emulator's copy key).
  The caveat ADR 0009 notes applies: a selection that spans the input box and the transcript
  follows the emulator's rules, not ours. Verify it selects what you expect.
- **the MCP approval box**: with `--mcp-config`, an MCP tool call prompts like any other. Is
  the tool name (`server__tool`) and the argument preview clear at your width, and is it
  obvious the server runs outside the sandbox (the startup notice says so)?
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

## Not verified

- Real terminal emulators beyond the owner's one macOS pass above (Terminal.app, iTerm2,
  Alacritty, tmux, ssh): layout, frame timing and glyph width are otherwise only checked
  against a `vt100` model.
- Frame timing under load (the spike measured p95 ≪ 16 ms; this skeleton was not benchmarked).
- `Shift-Enter` on terminals without the enhanced keyboard protocol.
- Windows and Linux ptys; only macOS was exercised.
- Large transcripts: this slice renders every entry each frame (no virtualization; Phase 5).

## Sessions

The session directory and id convention is **the same as `agent-cli`**, so a session written
by one is resumable by the other:

- `--session-dir DIR`, else `RADIAN_SESSION_DIR`, else the platform data directory
  (`~/Library/Application Support/radian/sessions` on macOS,
  `$XDG_DATA_HOME/radian/sessions` or `~/.local/share/radian/sessions` elsewhere). Never a
  cache or temp path: a session is a durable record (ADR 0013 decision 4).
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
`--system`, `--session-dir`, `--session-id`, `--resume`, `--list`, `--workspace DIR`,
`--mcp-config FILE`,
`--profile safe|workspace-write|full-access` (default `workspace-write`),
`--approval on-ask|always|never` (default `on-ask`), `--allow-shell-command PREFIX`,
`--context-window TOKENS`. The key is read only from the named environment variable and is
never printed.

`--header`, `--mcp-config` and the session-id rules are the **same code** `agent-cli` uses
(`crates/radian-host`), so the two hosts cannot drift: `{session_id}` expands to the
process's id (or the resumed one), `authorization` is refused because the key has one source,
and a malformed argument is reported without echoing its value.

## Tools, permissions and the approval dialog

`--workspace DIR` mounts the built-in file and read-only git tools (`read`, `write`, `patch`,
`list`, `git_status`, `git_diff`) rooted at that directory, plus **`shell`** when a sandbox
could be built. This is the same wiring `agent-cli` uses:

- The sandbox is built **before** the tools: `workspace-write` and `full-access` call
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

Every call goes through `radian-permissions` (ADR 0012) as a `Hooks` implementation:

- `--profile safe|workspace-write|full-access` picks the rule set (default `workspace-write`:
  reads allowed, writes allowed only when the resolved path stays inside the workspace).
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
  `workspace-write` this means "the sandbox is the boundary"; with `full-access` there is no
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

## Context budget

`--context-window TOKENS` configures the ADR 0015 guard. Omitted means **off**: the app does
not guess a window and the status bar says `context guard off (no window configured)`.

When a turn ends because the next request would cross the threshold, the transcript gets a
notice and the status bar shows the numbers. The wording depends on where the number came
from (ADR 0015 C1) — a report, a report plus an estimate, and a pure estimate are three
different things:

| Source | Wording |
|---|---|
| reported | `context 12,340 of 16,000 at 80% (reported)` |
| reported + estimate | `context 12,340 of 16,000 at 80% (reported 12,000 + estimated 340)` |
| estimate | `context ~12,340 of 16,000 at 80% (estimated)` |
| no window | `context guard off (no window configured)` — no number at all |

The mixed value is never shown as a plain report, and a projection that has no reported
baseline is marked `~… (estimated)`.

Keys, while the input box is empty (so typing is never swallowed):

| Key | Action |
|---|---|
| `c` | continue once: the next turn's first request passes the guard, then the guard applies again |
| Esc | dismiss the notice (the next message is a normal, guarded turn) |

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
- Under `workspace-write` an MCP call is an `ask`: it goes through the same approval dialog
  as a built-in tool. `safe` denies MCP; `full-access` allows it.
- **The servers themselves are not sandboxed** (ADR 0012 option B). The `workspace-write`
  startup notice says so, because the notice is the only safeguard.

## Status bar

The bottom line carries, most important first:

| Segment | Shows |
|---|---|
| model | the `--model` value |
| turn | `idle` / `running` / `cancelled` |
| tier | `safe` / `workspace-write` / `full-access` |
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
