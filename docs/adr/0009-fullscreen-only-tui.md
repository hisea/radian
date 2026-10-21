# ADR 0009: Fullscreen-Only TUI

Status: accepted

## Background

A terminal TUI has two display modes:

- **Fullscreen**: enters the alternate screen (`ESC[?1049h`). The program owns the full display. On exit the terminal restores its prior state. While the program runs, the user cannot see or scroll the earlier terminal history. Program output does not stay in the scrollback after exit.
- **Inline**: does not enter the alternate screen. It uses only a few lines under the current cursor as a live region. All other content still scrolls into the scrollback. The terminal provides scrolling, mouse selection, and copy natively.

For a coding agent with transcripts thousands of lines long, inline mode would gain these capabilities for free. But a `ratatui` inline viewport crashes on real-terminal resize: `compute_inline_size()` calls `backend.get_cursor_position()`, crossterm sends DSR (`ESC[6n`) and blocks for the CPR reply. A late reply can then pollute shell input. Fullscreen needs no such query (a fullscreen viewport starts at `(0,0)`). The difference is architectural, not implementation quality.

## Decision

**Radian implements a fullscreen-only TUI.** Scrolling, selection, copy, search, and virtualization of long transcripts are all implemented directly. No inline viewport exists, and no manual line-position management emulates one.

Terminal lifetime constraints:

- After alternate-screen plus raw-mode entry, every exit path (normal exit, Ctrl-C, panic, terminal loss) must restore the terminal. The panic path performs no new file I/O (see the panic-path precondition in `crates/radian-tui/README.md`).
- On fullscreen exit, print one extra newline so no `%` mark appears before the shell prompt.
- Input reading never blocks directly on `event::read`: first `poll(2)` detects hangup, then drains the crossterm buffer. A lost terminal is an event (`TerminalGone`), not a hang.

## Verification constraints

Capabilities that need terminal answers (DSR/CPR, keyboard enhancement queries, device attribute queries) cannot prove feasibility through pty automation: the `vt100` screen model answers deterministically at once, but real terminals give no guarantee under resize, high load, or remote sessions. Such thresholds require verification by hand on a real terminal before they count as settled. Pty tests cover only screen content and exit paths.
