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
 model │ idle/running/cancelled │ scroll N/M follow|paused
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
- **mouse selection/copy**: this app does **not** enable mouse capture, so selection is the
  **terminal emulator's own** (drag to select, Cmd-C on macOS, or the emulator's copy key).
  The caveat ADR 0009 notes applies: a selection that spans the input box and the transcript
  follows the emulator's rules, not ours. Verify it selects what you expect.

Suggested 3-line manual pass (in a throwaway terminal, not your working one):

```sh
cargo run -p radian-tui -- --model <model> --base-url <...> --session-dir /tmp/radian-tui-manual
# 1. send a message; while it streams, PageUp and keep sending: the view must not jump back.
# 2. End; a new reply should follow. Paste a 5-line block and check it lands in the box.
# 3. drag-select some transcript text and copy it; then Ctrl-C twice and confirm the prompt
#    is clean (no `%`) and `stty -a` still shows `icanon`.
```

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

## Not verified

- Real terminal emulators (Terminal.app, iTerm2, Alacritty, tmux, ssh): layout, frame timing
  and glyph width are only checked against a `vt100` model.
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
`--system`, `--session-dir`, `--session-id`, `--resume`, `--list`, `--workspace DIR`. The key
is read only from the named environment variable and is never printed.

## Tools

`--workspace DIR` mounts the built-in file and read-only git tools (`read`, `write`, `patch`,
`list`, `git_status`, `git_diff`) rooted at that directory, behind the approval dialog. No
`shell`: it needs the sandbox and permission-tier wiring, and follows in the next slice.
