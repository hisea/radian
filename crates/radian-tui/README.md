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

## Review note: the panic path exits without unwinding (2026-10-12)

The panic hook restores the terminal and then calls `process::exit(101)` instead of letting
the panic unwind, because unwinding drops the tokio runtime and that drop hangs on the
blocking stdin reader (the same hang `agent-cli` avoids with `shutdown_background`).

Accepted **for now**, because of two facts checked in review:

- **Durable data is safe.** Every journal append is fsynced before it returns (ADR 0001 D3),
  so an abrupt exit loses at most a torn tail, which the reader truncates (ADR 0006 §3.2).
- **No child process can be orphaned today.** `radian-tui` does not depend on
  `agent-extensions` or `radian-coding`, so it spawns no MCP servers and no shell.

**This stops being true the moment the TUI owns child processes.** Skipping unwinding skips
every `Drop`, including the ones that kill MCP server process groups and (later) sandboxed
shell children. Before the TUI gains MCP or the shell tool, the panic path must either kill
the registered process groups itself before `exit`, or unwind with the runtime shut down via
`shutdown_background`. Treat this as a precondition of that slice, not a follow-up.

## Not verified

- Real terminal emulators (Terminal.app, iTerm2, Alacritty, tmux, ssh): layout, frame timing
  and glyph width are only checked against a `vt100` model.
- Frame timing under load (the spike measured p95 ≪ 16 ms; this skeleton was not benchmarked).
- `Shift-Enter` on terminals without the enhanced keyboard protocol.
- Windows and Linux ptys; only macOS was exercised.
- Large transcripts: this slice renders every entry each frame (no virtualization; Phase 5).
- `--resume` and a session list are not implemented; a session is always new.

## Flags

`--model`, `--base-url`, `--api-key-env` (default `OPENAI_API_KEY`), `--header NAME=VALUE`,
`--session-dir`, `--system`. The key is read only from the named environment variable and is
never printed. Tools are `NoTools` in this slice (a chat loop); wiring the coding tools is a
later slice.
