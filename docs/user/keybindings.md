---
description: Every key in the full screen interface, by area, and how to rebind it.
---

# Keybindings

This page lists every key the full screen interface answers to. The first group
is rebindable through `tui.keybindings` in `configuration.md`. The second group
is fixed: those keys are handled before the keymap on purpose, so a bad
configuration can never approve a tool call you meant to deny.

A key string looks like `[ctrl-][alt-][shift-]key`. Examples: `enter`,
`ctrl-j`, `alt-enter`, `shift-up`, `pageup`, `space`, a single character such
as `a`. The spelling is case-insensitive, and `return` means `enter`, `escape`
means `esc`, `del` means `delete`, `pgup` means `pageup`, `pgdn` means
`pagedown`.

Set one action to one or more keys. A listed action's keys **replace** its
defaults.

```toml
[tui.keybindings]
send = ["ctrl-s"]
newline = ["ctrl-j", "alt-enter", "shift-enter"]
```

An unknown action name or an unparsable key is skipped with one startup
warning. Binding two actions of the same area to one key refuses startup and
names the key and both actions. Binding a bare printable character (no
`ctrl`/`alt`/`shift`) in the message area is kept but warns, because that
character can no longer be typed into the message box.

There are three areas, and a key only conflicts with another key of the same
area. The message area covers the composer and the transcript. The find area
covers the in-transcript search box while it is open. The form area covers the
structured question form while it is on top.

## Message area and transcript

These bindings edit the message, scroll the transcript, and drive the search
box and the folding. The table shows the action name for `tui.keybindings`
and its default keys.

| Action | Default keys | What it does |
|---|---|---|
| `send` | `enter` | Send the message. Idle, it opens a turn at once. While a turn runs, it steers the turn. While a completion menu is up, it accepts the highlighted row instead. |
| `newline` | `ctrl-j`, `alt-enter`, `shift-enter` | Insert a newline. `shift-enter` needs the keyboard-enhancement protocol; `ctrl-j` works everywhere. |
| `cursor_left` | `left` | Move the cursor one character left. |
| `cursor_right` | `right` | Move the cursor one character right. |
| `word_left` | `ctrl-left`, `alt-left` | Move the cursor one word left. |
| `word_right` | `ctrl-right`, `alt-right` | Move the cursor one word right. |
| `line_start` | `home`, `ctrl-a` | Move the cursor to the start of the input line. |
| `line_end` | `end`, `ctrl-e` | Move the cursor to the end of the input line. |
| `history_prev` | `up` | On the first input line, show the previous history entry. Otherwise move the cursor up one input line. While a completion menu is up, move its highlight. |
| `history_next` | `down` | On the last input line, show the next history entry (past the newest returns to the draft). Otherwise move the cursor down. While a completion menu is up, move its highlight. |
| `menu_next` | `ctrl-n` | While a completion menu is up, move its highlight down. It does nothing otherwise. |
| `delete_before` | `backspace` | Delete the character before the cursor. |
| `delete_under` | `delete` | Delete the character under the cursor. |
| `delete_word` | `ctrl-w` | Delete the previous word. |
| `delete_to_line_start` | `ctrl-u` | Delete to the start of the input line. |
| `delete_to_line_end` | `ctrl-k` | Delete to the end of the input line. |
| `scroll_page_up` | `pageup` | Scroll the transcript up one page. |
| `scroll_page_down` | `pagedown` | Scroll the transcript down one page. |
| `scroll_line_up` | `shift-up` | Scroll the transcript up one line. |
| `scroll_line_down` | `shift-down` | Scroll the transcript down one line. |
| `scroll_top` | `ctrl-home` | Scroll the transcript to its top. |
| `scroll_bottom` | `ctrl-end` | Scroll the transcript to its bottom. |
| `escape` | `esc` | Close the find box or a completion menu first, keeping the input. Then clear a non-empty input (the line stays in history). With an empty input, do nothing. |
| `cancel` | `ctrl-c` | Cancel the running turn (or the running `!` command). Idle with a non-empty input, clear it into the history. Idle with an empty input, arm "press ctrl-c again to quit" for 2 seconds. |
| `toggle_startup` | `ctrl-p` | Fold or unfold the startup information block. |
| `complete` | `tab` | Complete what is at the cursor. On a `/` line it completes the command tree. Otherwise it completes the `@` file reference. While a menu is up it accepts the highlighted row. |
| `find` | `ctrl-f` | Open the in-transcript search box. Pressed again while the box is open, it moves to the earlier match. |
| `toggle_reasoning` | `ctrl-t` | Fold or unfold the thinking blocks. |
| `toggle_tool_output` | `ctrl-o` | Expand or fold the tool result bodies. |

Type any other printable character to insert it. Paste inserts at the cursor
and never sends. The mouse `wheel` scrolls the transcript three lines per notch
while `tui.mouse` is on. The draft stays where it is.

While a completion menu is up, `up` and `down` (and `menu_next`) move its
highlight and wrap around. `enter` and `tab` accept the highlighted row. `esc`
closes the menu keeping the input. Any other key dismisses it.

Rebinding `send` away from `enter` leaves `enter` a no-op unless `newline`
takes it too. The keys are a total map, not layered.

## Find area

These bindings run while the in-transcript search box is open. The box's
layer is looked up before the message area, so `enter` is the earlier match
here and the send key outside it.

| Action | Default keys | What it does |
|---|---|---|
| `find_prev` | `enter`, `ctrl-f` | Move to the earlier match. |
| `find_next` | `shift-enter`, `ctrl-g` | Move to the later match. |
| `find_close` | `esc` | Close the box where the viewport is. |

Type to change the query; the search matches the displayed transcript
case-insensitively. `backspace` deletes one query character. `pageup`,
`pagedown`, `home`, `end` and the arrow keys still reach the transcript while
the box is open. The status line shows `match i/N`.

## Form area

These bindings run while a structured question form is on top. They are the
one modal group that is rebindable.

| Action | Default keys | What it does |
|---|---|---|
| `form_next` | `tab` | Move to the next question tab. |
| `form_prev` | `backtab`, `shift-tab` | Move to the previous question tab. |
| `form_up` | `up` | Move the option cursor up. |
| `form_down` | `down` | Move the option cursor down. |
| `form_select` | `space` | Select the highlighted option (`single` picks without advancing, `multi` toggles). In a free-text field that already holds the answer, the space is text. |
| `form_submit` | `enter` | On the last question show the overview; on the overview submit the answers. |
| `form_decline` | `esc` | Decline the whole request. The first press arms the decline; the second confirms it. While a note edit is open it closes the editor instead. |

The form has three of its own fixed keys on top of these. `alt-h` collapses
the panel. `left` and `right` switch questions while no free-text field has
focus. `n` opens a note for the current question.

`cancel` (`ctrl-c`) still runs before the form. Typing into an `Other` row
chooses it, so the typed text survives a move away and counts as the answer.

## Fixed keys

These keys are handled before the keymap and cannot be rebound.

### Approval prompt

A tool that needs your answer opens the approval prompt. While it is up, keys
do not reach the message box.

| Key | What it does |
|---|---|
| `y` (or `Y`) | Allow this request once. `Y` never turns auto-approve on. |
| `a` (or `A`) | Allow for this session. |
| `n` (or `N`), `esc` | Deny. |
| `!` | Approve this request and turn auto-approve on for the session. |
| `up`, `down`, `pageup`, `pagedown` | Scroll the prompt body. |

### Confirmations

| Key | What it does |
|---|---|
| `y` (or `Y`) | Confirm: run the `/memory` write (`forget`, `purge`, `split`, `merge`), remove the `/logout` credential. |
| `n` (or `N`), `esc` | Cancel it. |

### Trust prompt

The startup trust prompt opens on an untrusted directory.

| Key | What it does |
|---|---|
| `up`, `down` | Move between the choices. |
| `enter` | Take the highlighted choice. The default is the persistent "do not trust this directory". |
| `esc` | Do not trust, this session only. Nothing is written, so the next start asks again. |

### Access switch prompt

`/access full` asks for confirmation because it removes the sandbox.

| Key | What it does |
|---|---|
| `up`, `down` | Move between the choices. |
| `enter` | Take the highlighted choice. The default is cancel. |
| `esc` | Cancel. |

### Login wizard

| Key | What it does |
|---|---|
| `char` (any character) | Type into the current field. The key field behaves the same however the keymap is set. |
| `backspace` | Delete one character. |
| `enter` | Confirm the step. |
| `up`, `down` | Move between steps or choices. |
| `esc` | Go back. |
| `y` | On a failed verify, save anyway. Any other key cancels. |

### Session picker

The in-session `/sessions` overlay opens unfocused: the composer keeps the
keys until you navigate into it, and typing dismisses it.

| Key | What it does |
|---|---|
| `up`, `down` | Focus the picker and move the selection one row. |
| `pageup`, `pagedown` | Focus the picker and move ten rows. |
| `enter` | Select: show the copyable `--resume <id>` line. It never switches sessions in place. |
| `tab`, `ctrl-s` | Toggle full-text search. |
| `backspace` | Delete one filter character. |
| `n` | Start new, but only when the filter is empty. |
| type | Filter the rows. |
| `esc`, `ctrl-c` | Close the picker. |

### Inspector

The inspector (`/jobs`, `/tools`, `/mcp`, `/hooks`, `/panel`) owns every key
while it is open. Typing never falls through to the composer.

| Key | What it does |
|---|---|
| `up`, `down` | Move the selection one row. |
| `pageup`, `pagedown` | Move the selection ten rows. |
| `left`, `right`, `tab`, `shift-tab` | Change the page. |
| `enter` | Toggle the row detail. |
| `k` | On the jobs page, arm the kill confirm. |
| `y` | Confirm the armed kill. |
| `n` | Cancel the armed kill. |
| `esc` | Close a pending kill first, else close the inspector. |

### Side answer overlay

`/btw` streams its answer into an overlay. Every other key falls through to
the composer, so you can keep typing while it streams.

| Key | What it does |
|---|---|
| `up`, `down` | Scroll one line. |
| `pageup`, `pagedown` | Scroll one page. |
| `esc` | Cancel a running request, else close a finished answer. |

### Quit

There is no quit command. Press `ctrl-c` twice (`ctrl-c ctrl-c`) within 2 seconds on an empty
input to quit. Any other key, or the window closing, disarms the second
press. The status line shows `Press Ctrl-C again to quit`.
