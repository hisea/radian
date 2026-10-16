# radian-tui

Phase 4 / A: the full-screen TUI shell on `radian-runtime`. **Full screen only** (ADR 0009):
the alternate screen is used, so scrolling, selection and copy are ours to build rather than
the terminal's. This slice is the skeleton: one real conversation, a transcript view that
scrolls itself, streaming text and reasoning, and clean terminal recovery.

> **Do not run this in a shared terminal by hand** unless you accept that it takes over the
> screen. The tests never do: they drive the binary through `portable-pty` and read a `vt100`
> screen model, or use `ratatui`'s `TestBackend`.

## Architecture: Action → Effect (ADR 0023 U3)

The app is a pure state machine. One function changes it:

```rust
App::update(input: Input) -> UpdateResult { dirty: bool, effects: Vec<Effect> }
```

- **`Input`** is either an interaction intent (`Action`, from the keymap; `Char`, a typed
  character; `FindBackspace`; `Paste`; `Resize`; the hardcoded approval and confirmation
  answers; `PickerKey`/`PickerClose`, one key for the picker overlay) or a host fact (an
  `AgentEvent`, a runtime event, an approval request, and the result of an effect).
- **`Effect`** is everything that needs the outside world: `SendFollowup`/`Steer`/`Cancel`/
  `Quit`, `AnswerApproval`/`AbandonApproval`, `ExpandMentions` (the `@` expansion reads
  files), `CompleteMention` (reads a directory), `RunMemoryCommand`, `Compact`, `Session`
  (`/export`, `/fork`), and `OpenPicker` (reads the session index for `/sessions`).
- The event loop in `main.rs` runs the effects and feeds their results back as inputs. A key
  press is resolved by `App::key_input` (the keymap plus the current layer) and `App::on_key`
  is the thin wrapper around it.
- **`update` never** does I/O, touches the runtime, reads a file or a directory, or calls the
  memory service. `tests/purity.rs` asserts that structurally (it greps `src/app.rs` for
  `std::fs`, `MemoryService`, `tokio`, and the `@` calls).
- `dirty` says whether the renderer's input may have changed, so the loop can skip a frame;
  an input that changes nothing (an unbound key, cancel, quit) is not dirty.

Because of this, most behavior tests are pure `update` tests (`tests/update.rs`,
`tests/input.rs`, `tests/command.rs`, `tests/approval.rs`); the pty tests keep the terminal
state, the real key protocol and the recovery paths.

### The loop: one channel, three promises (ADR 0023 U2)

`main.rs`'s loop blocks on **one** `std::sync::mpsc` channel of `UiEvent`:

| Source | Event | Promise |
|---|---|---|
| facts and control | `Agent` (committed core events), `Runtime`, `Approval`, `Command` (effect results) | never dropped; unbounded, so a producer never blocks (ADR 0010 K3-1) |
| dirty notification | `Dirty` | coalescible: at most one per session is in flight |
| terminal input | `Input` (key/paste/resize), `TerminalGone` | ordered, forwarded by a dedicated input thread |
| process and host | `Signal`, `DriverStopped` | one event each |

- **Deltas never enter the channel.** `ModelTextDelta`/`ModelReasoningDelta` accumulate in
  `projection::SessionProjection`; the producer sets a `dirty` flag (`Release`) and sends one
  `Dirty` only when it was `false`. The UI clears the flag (`Acquire`) **before** reading the
  snapshot, so an update after the read wakes it again — no lost wake-ups, and at most one
  notification in flight. The snapshot then becomes the live entries through
  `Input::Stream`.
- **Nothing is polled.** Signals, the driver's end and command results are events; the input
  thread owns `read_input`'s `poll(2)` hang-up protection (it never blocks in `event::read` on
  a tty returning EOF) and coalesces a burst of resizes to the last one.
- **The loop is bounded per turn**: at most `MAX_FACTS_PER_TURN` (64) facts, then a frame if
  anything is dirty. That is the draw-fairness rule: a producer that keeps the channel busy
  cannot starve the frame (`pty_a_flooding_producer_still_draws_and_answers_keys`).
- **Idle is zero redraw**: the loop blocks in `recv`, and no display in this build changes with
  time, so nothing is drawn until an event arrives (`pty_an_idle_screen_is_not_redrawn`; the
  Step 0 baseline was ~19 frames/s, `docs/perf-baseline.md` §7b).

### The layer stack (ADR 0023 U4)

`App` holds a `Vec<Layer>`, bottom first. A layer's tier is its variant:

| Tier | Layers | Rule |
|---|---|---|
| `Modal` | the approval prompt, the `/memory` confirmation (a structured question is the documented extension point) | must be answered; always above every overlay |
| `Overlay` | the find box, the `@` completion list, the session picker | temporary; never above a modal |
| `Surface` | the transcript and the message box | always at index 0 |

`App::push_overlay` inserts below the highest modal, so an overlay pushed while a prompt is
up cannot cover it; `push_modal` goes on top. `App::key_input` then resolves a key in a fixed
order: (2) a modal's hardcoded `y`/`a`/`n`/`Esc` win over the keymap — even over a `cancel`
rebound onto `y`; (1) the bound `cancel` action (Ctrl-C by default) runs before the stack;
then the stack is walked top-down, the first layer that consumes the key winning and an
unconsumed key falling toward the surface (`PageUp` still scrolls while the find box is open).
The four former `Option` fields are gone (`tests/layers.rs`).

The `--pick` startup screen and the `/sessions` overlay share one `picker::Picker` and the
**same** event loop, terminal and input thread: there is no separate picker loop. `--pick`
runs a pre-session phase of that loop (only the picker overlay is live; no journal, driver or
MCP connection exists), and the selected id resolves the session and `{session_id}` headers
before the ordinary session phase takes over in the same terminal. The in-session overlay
opens **unfocused**, so the composer keeps the keys until the user navigates into it
(`Up`/`Down` focus it), and typing dismisses it. Selecting does not switch sessions in place
(ADR 0022): it shows the `--resume <id>` line.

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
transcript: user >, assistant, ~ reasoning, -> tool, <- result
  ◜  Working · 12s · esc to interrupt
▎ type here
 model │ idle/running/cancelled │ tier │ usage in/out │ mcp servers/tools │ context │ scroll N/M
```

The default input area is `tui.input_style = "bar"` (ADR 0035 V3, the omp `rail`): one
background band per input row (the theme's `input_bar` role — pi's `text` on `userMessageBg`,
the same band as a user message block), the accent rail `▎` on the left (`|` with
`glyphs = "ascii"`), no border and no rules; a multi-line input grows the band by its rows, and
its continuation rows carry the rail too. Directly above the band is a **fixed one-row status
line**: blank while idle but still occupying its row — so the band and the transcript never
move when a turn starts or finishes — and
`<spinner>  Working · 12s · esc to interrupt` while a turn runs (the spinner is the same one the
`lines`/`box` rule line uses, the rest is one level weaker; the clock is the footer's `elapsed`
and `esc to interrupt` appears exactly while a turn is running). `tui.input_style = "lines"` is
the earlier shape: a rule above and below with a `> ` prompt, continuation lines indented two
cells. `tui.input_style = "box"` is the owner's bordered shape (2026-10-14): **rounded**
corners, the `❯` prompt, and the border in the theme's accent role (`notice_label` — ADR 0029
revision 1 reserves accent for labels and attention surfaces). The `lines` and `box` styles put
the same running status on their top rule / border instead of on a status row. It is a real
composer in all three (cursor editing, multi-line, history).

The **startup screen** is the leading block of transcript entries: the
wordmark (three rows at the primary `text` level; below its width it drops to one line,
`<brand> radian <version>` (ADR 0027: `R` unicode, `󰰠` nerd, `rad` ascii), with the width measured by `unicode-width`), one collapsed information
line, the safety lines in the `warning` kind that are never folded (a workspace boundary that
was not your choice, every config-file and approval-policy warning), and one `Tip:` line. An
information line's `label: value` split follows a notice's: the label uses `notice_label`, the
value `notice_body` (ADR 0028 / ADR 0029). `Ctrl-P` (`toggle_startup`) folds and
unfolds the information. `tui.banner = false` hides the wordmark, `tui.tips = false` the Tip.
`tui.brand = "<text>"` (ADR 0027) replaces the brand mark in **both** the footer's
`brand` item and the one-line banner fallback (one source; ≤ 4 display columns, wider is a
config error). Without it the glyph set's default is used (`nerd` `󰰠`, `unicode` bold accent `R`,
`ascii` `rad`). A user-written glyph is **not** covered by the font-coverage rule: a font that
lacks it draws a fallback glyph, which may sit off the baseline. `[tui.footer.items.brand] style`
still overrides the color.
The collapsed summary names the sandbox in plain words: `workspace · macOS
sandbox · context …` on macOS, `workspace · Linux sandbox · context …` on Linux, `no exec` for
`safe` and `no sandbox` for `full`; the exact backend name (`macos-seatbelt`, `linux-bwrap`)
stays in the expanded `profile:` line.
The transcript scrolls with PageUp/PageDown, Shift-Up/Shift-Down and Ctrl-Home/Ctrl-End; the
bare arrow keys belong to the composer.

| Key | Action |
|---|---|
| Enter | send. Idle → `Inbox::followup`; while a turn runs → `Inbox::steer`. **While a completion menu is up it accepts the highlighted row instead** (it completes, it never executes, and it never sends) |
| Tab | complete what is at the cursor. On a `/…` line: the command tree (below). Otherwise the `@` file reference: one candidate is inserted (a directory keeps its trailing `/`, a file gets a space); several complete to their longest common prefix, and a second Tab already at that prefix shows the menu above the input box; any other word does nothing (the key is reserved). **While the menu is up it accepts the highlighted row**, like Enter |
| Ctrl-J (or Shift-Enter / Alt-Enter) | insert a newline in the input (Shift-Enter needs the keyboard-enhancement protocol; Ctrl-J works everywhere) |
| Left / Right | move the cursor one character |
| Ctrl-Left / Ctrl-Right (or Alt-Left / Alt-Right) | move by one word |
| Home / End (or Ctrl-A / Ctrl-E) | the input **line's** start / end |
| Up / Down | **while a completion menu is up, move its highlight** (and wrap); otherwise on the first / last input line the previous / next history entry (Down past the newest returns to the draft), else move between the input's lines, keeping the column |
| Backspace / Delete | delete before / under the cursor |
| Ctrl-W | delete the previous word |
| Ctrl-U / Ctrl-K | delete to the line's start / end |
| PageUp / PageDown | scroll the transcript a page |
| Shift-Up / Shift-Down | scroll the transcript one line |
| Ctrl-Home / Ctrl-End | scroll the transcript to its top / bottom |
| Esc | (`escape` in the transcript, `find_close` in the find box) close the find box or a completion menu if one is up (the input is kept); otherwise clear a non-empty input first (the line stays in the history, so `Up` finds it); with an empty input, cancel the running turn / quit |
| Ctrl-C | cancel the running turn; idle with a non-empty input, clear it (into the history); idle with an empty input, arm "press Ctrl-C again to quit" for 2 s (any other key, or the window closing, disarms it; the status bar shows `Press Ctrl-C again to quit`) |
| Ctrl-P (`toggle_startup`) | fold or unfold the startup information block |
| Ctrl-N (`menu_next`) | while a completion menu is up, move its highlight down (Ctrl-P is taken, so Ctrl-N is the menu's other key; ) |
| Ctrl-F (`find` in the transcript, `find_prev` in the find box) | open the in-transcript search box (`find: <query>`, replacing the input line; the message draft is kept). Type to search the displayed transcript case-insensitively; a second Ctrl-F or Enter is the earlier match, Shift-Enter or Ctrl-G the later one (no wrap); the status bar shows `match i/N`; Esc closes the box where the viewport is |
| bracketed paste | inserts at the cursor, `\r\n` normalized to `\n`, and never sends |

Shift-Enter needs the terminal's **keyboard-enhancement** protocol: without it most terminals
send Shift+Enter as a plain Enter, which sends. Radian asks the terminal at startup
(`supports_keyboard_enhancement`, crossterm 0.29's kitty-protocol probe: it writes
`ESC[?u ESC[c` and waits up to **2 s** for a reply; a terminal that answers the
device-attributes query but not the flags query ends the wait immediately) and, when the answer
is yes, pushes `DISAMBIGUATE_ESCAPE_CODES`; the flags are popped on every exit path, panic and
signal included. `RADIAN_KEYBOARD_ENHANCEMENT=0` skips the probe and the push (a pty has nobody
to answer, so the tests set it); `=1` pushes without asking. Ctrl-J and Alt-Enter work
everywhere and stay the fallback.

`@relative/path` in a message attaches the file's contents to the request (Phase 4): resolved
inside the workspace like the `read` tool (no absolute path, no `..`, a symlink that leaves is
refused), at most 100 KiB per file and 400 KiB in total, and a binary file makes the message
unsendable with the input kept. The transcript folds each attachment to `[file: P, N lines]`.
`Tab` completes the path; the menu is at most 10 rows plus `+N more`, clipped to the terminal
width, and never covers an approval prompt. The first (best) row is highlighted with the theme's
`selection` (reversed in `mono`); `Up`/`Down` (or `Ctrl-N`) move the highlight and wrap, `Enter`
or `Tab` accepts the highlighted row, and `Esc` closes the menu keeping the input.

### Theme and keybindings (config `tui.*`)

The file is read the same way as the CLI's (`--config`, `RADIAN_CONFIG`,
`~/.radian/config/config.toml`); every flag still wins.

```toml
schema_version = 1
[tui]
theme = "dark"                 # a built-in name, or a theme file name (see below)
input_style = "bar"            # bar (default) | lines | box
mouse = true                   # wheel + transcript selection (default true)
clipboard = "osc52"            # osc52 (default) | pbcopy (macOS) | off

[tui.colors]                   # per-role color overrides (ADR 0029 decision 5)
user = "#0af"                  # a bare color: the foreground only
notice = { fg = "oklch(62% 0.1 200)", bold = true }
mdHeading = 203                # pi role names work too (kept, not drawn, until ADR 0028)

[tui.keybindings]
send = ["ctrl-s"]              # replaces the default `enter`
newline = ["ctrl-j", "alt-enter", "shift-enter"]
history_prev = ["up"]
complete = ["tab"]             # the `@` file-reference completion
menu_next = ["ctrl-n"]         # move the completion menu's highlight down
find = ["ctrl-f"]              # open the in-transcript search box
find_next = ["ctrl-g", "shift-enter"]
```

- `input_style`: `bar` (the default: the banded input with the accent rail and the fixed status
  row above it, ADR 0035 V3), `lines` (a rule above and below, `> ` on the first line) or `box`
  (the owner's rounded box with the `❯` prompt). An unknown value is ignored with a warning and
  falls back to `bar`.
- `tips` / `banner`: whether the startup screen shows the one Tip line and the wordmark. Both
  default to on; `false` hides them.
- `footer` (ADR 0027 C2, revision 3): the status area's rows. `preset` is `deck` (the default,
  three rows: the opencode-go usage line — dropped when it has no data — then
  `model`/`cwd`/`git_branch`/`worktree` with `context_bar`/`reasoning` on the right, then
  `tokens`/`cost`/`context` on the right), `full` (revision 1's two rows), `pi` (the earlier two
  rows) or `classic` (today's one line, byte-for-byte). `separator` joins items (default
  `" │ "`, any string), each row may override it, and `separator_style` styles it (default the
  `footer_separator` role, dim). `glyphs` is `unicode` (default), `nerd` or `ascii`.
  `context_line` is `off` (default; the gap between a row's left and right groups is spaces) or
  `percentage` (ADR 0027 item 4, following omp's `statusLine.contextLine`: the gap
  becomes a `─` rule, the used fraction of the context window in the `budget` role and the rest
  in the `footer_separator` role; with no known budget the gap stays spaces, never a fabricated
  percentage). `transparent = true` (default `false`; ADR 0027 item 5, omp's
  `statusLine.transparent`) clears every footer cell's background - a configured item style's
  included - so the footer inherits the terminal's own background.
  `[tui.footer.items.<id>]` gives one item its `symbol`/`style`/`symbol_style`/`format`/
  `disabled`; a bare id in a row takes it, an inline table wins. Each `[[tui.footer.rows]]` has
  a `left` and a `right` group; an item is a bare id or a table with `item`, `icon`/`symbol`,
  `format`, `style`, `symbol_style`, `priority`, `thresholds = [{ at = 70, style = "warn" }, …]`
  and the item's own parameters. A `style` (or `[text](style)` inline) is a theme role
  (`accent`, `muted`, `footer_separator`, pi's `dim`/`warn`/`error`/`ok`/`text`) or a
  starship-style color (`red`, `bright_blue`, `#rrggbb`, a 0–255 number, with
  `bold`/`italic`/`dimmed`); a role follows the theme, a fixed color does not. `format` may
  color a span with `[text](style)`. An unknown item id, placeholder or parameter is shown raw
  with one warning and never blocks startup; a bad color is a startup warning too. Narrow
  terminals drop the `right` group first, then the `left` group from its end, lowest
  `priority` first. Numbers are compact (`15M`, `2.6M`, `1962M`, `$14.510`, `41.3%`).
  `host`, the model's price and the auto-compaction switch are read once at startup;
  `git_branch`/`worktree` and the change counts come from the background contributor, so the
  draw path never runs git.

| item | placeholders | default |
|---|---|---|
| `brand` | `{brand}` | `R` (bold accent, unicode) / `󰰠` (nerd) / `rad` (ascii); ADR 0027 |
| `host` | `{host}` | the machine's hostname, read once at startup |
| `model` | `{display_name}`, `{id}`, `{provider}` | the model's display name (or id) |
| `reasoning` | `{level}` | the thinking level, one theme role per level |
| `tier` | `{tier}` | the permission tier |
| `cwd` | `{path}`, `{path_home}`, `{basename}`, `{repo}`, `{worktree}` | `{path_home}` (`$HOME` → `~`); `{repo}` falls back to the directory name, `{worktree}` is the contributor's current worktree |
| `git_branch` | `{branch}`, `{dirty}`, `{modified}`, `{untracked}` | `{branch}`; `{dirty}` is ` ?<total>` while the tree has changes (contributor) |
| `session` | `{id}` | the session id |
| `context` | `{bar}`, `{percent}`, `{used}`, `{budget}`, `{auto}` | `{used}/{budget}`; `{auto}` is `~` (`auto` under ascii) while auto-compaction is on |
| `context_bar` | `{bar}`, `{percent}`, `{used}`, `{budget}`, `{auto}` | `{bar} {percent}%`, `width` 10, `fill` `━`, `empty` `─` |
| `tokens` | `{in}`, `{out}`, `{cache}`, `{cache_write}` | `^{in} v{out} R{cache}`; compact K/M, `-` when a provider never reported one. `{cache_write}` (ADR 0027) is a cache **write**, billed at its own rate but deliberately **not** in the default format, so the row does not grow |
| `cost` | `{usd}` | the session's accumulated fee: each call priced at the route's price **when it arrived** (`radian_host::cost`), so a `/model` switch does not re-price history. The cached tokens are billed once at the cache rate (`uncached_input()`). Hidden until some call had something to bill and when the catalog has no price — never a fabricated number, never `$0.00` |
| `speed` | `{speed}` | `42.1 tok/s`, the last finished generation's reported output over its measured time |
| `elapsed` | `{elapsed}` | `16.2s`, live while a generation runs, the last one when idle, empty after a resume |
| `pending` | `{count}` | shown only when a request waits |
| `mcp` | `{mcp}` | the configured/connected servers |
| `scroll` | `{position}`, `{mode}` | `{position} {mode}` |

| `worktree` | `{worktree}` | 只在 git worktree 里显示（贡献者） |
| `status` | `{status}` | `status.*` 贡献文本，`keys = ["…"]` 可过滤 |
| `opencode-go.usage` | — | opencode-go 用量的**剩余**百分比与重置倒计时（ADR 0027 C4，贡献者；`tui.usage_period_secs` 0 关闭） |

`worktree`/`status` 来自贡献者存储（ADR 0027 C3）：一个 `git` 贡献者每 5 s 在后台刷新
`git_branch`/`worktree`（绘制路径从不跑 git），刷新失败保留旧值并画成暗色。`[tui.widgets]`
的 `above_input`/`below_input` 把同样的条目 id 放到输入框上/下一行。

#### Glyph coverage (ADR 0027 and 6)

A glyph the font does not have is drawn by a system fallback font with different metrics, so it
sits off the baseline. The `unicode` tier (the default, no Nerd Font needed) therefore only uses
characters that **all** of Menlo, SF Mono, JetBrains Mono, Cascadia Code, Fira Code and
Source Code Pro have (ADR 0027: Operator Mono left the reference set, and the tier's
token arrows became `↑`/`↓`, its success `✓`, its fold marker `›` and its truncation `…`). The
brand is `R` (bold accent) here, `󰰠` under `nerd`, `rad` under `ascii`; `㎭` (U+33AD) is gone
(East-Asian-Wide, off-baseline fallback). The spinner's default
is `pulse` (`⏺`, one cell); the covered optional kind is `quadrant` (`▖▘▝▗`). The arc
(`◜◝◞◟`), the braille (`⠋⠙⠸⠴`) and `pulse` itself are the **documented exceptions** to this
rule (ADR 0027/10: the owner picked them after seeing them), so a font without them
draws a fallback glyph; `tui.spinner = "quadrant"` switches to the covered set.

Check a glyph with `crates/radian-tui/scripts/check-glyph-coverage.py` (needs `fonttools` in a
throwaway venv; it is not a repository dependency and not part of `cargo test`). A font that is
not installed is printed as `未查`, never guessed. The unit test
`footer::tests::unicode_tier_glyphs_are_all_verified_covered` keeps a new unicode-tier glyph
from slipping in unchecked.

| glyph | codepoint | use | Menlo | SF Mono | JetBrains Mono | Cascadia Code | Fira Code | Source Code Pro |
|---|---|---|---|---|---|---|---|---|
| `R` | U+0052 | brand (unicode) | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `@` | U+0040 | host | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `»` | U+00BB | speed | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `*` | U+002A | reasoning | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `#` | U+0023 | session | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `↑` | U+2191 | tokens in | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `↓` | U+2193 | tokens out | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `~` | U+007E | auto-compaction marker | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `~` `✓` `×` | U+007E/2713/D7 | tool status: running / success / failure | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `›` | U+203A | reasoning fold marker | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `…` | U+2026 | truncation | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `│` | U+2502 | footer separator | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `━` | U+2501 | context bar fill | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `─` | U+2500 | context bar empty | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `▖` | U+2596 | spinner frame 1 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `▘` | U+2598 | spinner frame 2 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `▝` | U+259D | spinner frame 3 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| `▗` | U+2597 | spinner frame 4 (`quadrant`) | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |

Documented exceptions (allowed to fall back; the test keeps them in a separate set):

| glyph | codepoint | use | Menlo | SF Mono | JetBrains Mono | Cascadia Code | Fira Code | Source Code Pro |
|---|---|---|---|---|---|---|---|---|
| `⏺` | U+23FA | spinner default (`pulse`) | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| `◜` `◝` `◞` `◟` | U+25DC…DF | spinner `arc` | ✅ | ❌ | ❌ | ✅ | ✅ | ❌ |
| `⠋` `⠙` `⠹` `⠸` `⠼` `⠴` `⠦` `⠧` `⠇` `⠏` | U+280B… | spinner `braille` | ❌ | ❌ | ❌ | ✅ | ❌ | ❌ |

Replaced or dropped in the `unicode` tier (they were not in all six fonts):

| was | U+ | now |
|---|---|---|
| `⌂` host | U+2302 | `@` |
| `◷` elapsed | U+25F7 | no icon (text only) |
| `◫` context | U+25EB | no icon (text only) |
| `◐` reasoning | U+25D0 | `*` |
| `⛁` session | U+26C1 | `#` |
| `⬡` tier | U+2B21 | no icon (text only) |
| `^` `v` tokens | U+005E/76 | `↑` `↓` |
| `⟲` auto | U+27F2 | `~` |
| `⠋⠙⠸⠴` spinner | U+280B… | `▖▘▝▗` |
| `▸` thinking fold | U+25B8 | `›` |
| `⟳` `✔` `✘` tool status | U+27F3/2714/2718 | `~` `✓` `×` |
| `▶` `▼` startup fold | U+25B6/BC | `+` `-` |
| `㎭` brand | U+33AD | `R` / `󰰠` / `rad` |

Not in the all-six set but kept (they are not tier-gated icons): the `❯` input prompt (the
owner's choice; `box` style only) and text arrows (`→`, `•` in the question UI — `•` is in all
six, `→` is not, but it is prose, not an icon). The banner wordmark and the box-drawing
characters (`╭╮╯╰`, `┌┐└┘`, `─│`) are in all six.
- `theme`: `dark` and `light` are **pi's own palettes**, read
  through the same ADR 0029 conversion a theme file uses: the three text levels are pi's
  brightness gradient (dark: `text` 89%, `muted` 67%, `dim` 56%), and every semantic color
  sits at the 67% level, told apart by hue. `mono` uses no color at all (bold/underline/dim
  only). A non-empty `NO_COLOR` environment variable forces `mono`
  (<https://no-color.org>). An unknown theme is ignored with a warning.
#### Theme files (ADR 0029): any pi theme works as-is

`tui.theme` accepts any name. The built-ins (`dark`, `light`, `mono`) resolve first; any other
name is read from `~/.radian/themes/<name>.json` — and that file is **pi's theme JSON, read
as-is**. Copy a pi theme over and it just works:

```sh
mkdir -p ~/.radian/themes && cp ~/.pi/agent/themes/kanagawa-wave.json ~/.radian/themes/
```

(set `theme = "kanagawa-wave"` in the config; the file's `name` field should match.) Colors
come in six forms, all supported: `#rgb`/`#rrggbb`, `oklch(L C H)`, `okhsl(H S L)`, a 0–255
palette index, a `vars` reference (chains allowed), and `""` (the terminal default). With
truecolor (`COLORTERM` is `truecolor` or `24bit`) colors render as RGB; otherwise each is
approximated to the nearest 256-color entry, with the same algorithm pi uses.

Radian is more forgiving than pi on purpose (ADR 0029 decision 4): a missing variable, a
circular reference or an unknown role only makes the *affected roles* fall back to the
built-in palette (chosen by the file's `appearance`, `dark` by default), each problem warning
once on the startup line; a broken file falls back as a whole. Nothing blocks startup. pi's
reserved `system` theme (OSC terminal probing) is deliberately not implemented.

Every pi role maps onto a Radian role (`[tui.colors]` accepts both name families):

| Radian role | pi role(s) it takes |
|---|---|
| `user` | `userMessageText` (fg) + `userMessageBg` (bg) |
| `input_bar` | `text` (fg) + `userMessageBg` (bg) — the `bar` input's band, the same band as a user message block (ADR 0035 V3) |
| `selection` | `selectedBg` (bg only — the text keeps its own foreground under the band) |
| `tool_pending_bg` / `tool_success_bg` / `tool_error_bg` | `toolPendingBg` / `toolSuccessBg` / `toolErrorBg` (the tool card's band by state) |
| `notice_label` / `notice_body` | `accent` / `muted` (a structured notice's label and the rest) |
| `diff_added` / `diff_removed` / `diff_context` | `toolDiffAdded` / `toolDiffRemoved` / `toolDiffContext` (the unified-diff line kinds) |
| `diff_hunk` | `accent` (pi has no hunk role; this covers the `@@` markers and the file headers) |
| `reasoning` | `thinkingText` |
| `reasoning_off` … `reasoning_max` | pi's `thinkingOff` … `thinkingMax` (the footer's `reasoning` item, one role per level) |
| `tool_call` / `tool_result` / `tool_result_error` | `toolTitle` (= `text`) / `toolOutput` (= `muted`) / `error` |
| `text` | `text` (the main level: answers, user words, tool titles) |
| `notice` | `text` (the notice line itself) |
| `hint` / `completion` / `truncation` | `dim` (the weak level) |
| `turn_idle` / `turn_running` / `turn_cancelled` | `success` / `warning` / `error` |
| `interrupted` / `note` / `budget_over` | `warning` |
| `model` / `mcp` / `find` | `accent` |
| `auto` | `warning` (pi has no auto role; the status bar's `AUTO` marker is reversed) |
| `tier` / `usage` / `budget` | `muted` |
| `approval_border` | `borderAccent` |
| `auto`（状态栏的 `AUTO` 标记，ADR 0026 D9） | `warning` |

The pi roles nothing renders yet — the `md*`, `toolDiff*`, `syntax*` and `thinking*` families
and the panel roles (`scrollbar*`, `searchMatch*`, `customMessage*`, `toolPendingBg`,
`toolSuccessBg`, `toolErrorBg`, `bashMode`) — are parsed and kept on the theme (ADR 0029
decision 3), so ADR 0027/0028 can wire them without re-reading the file.
- `mouse` (default `true`): the app enables button + wheel reporting (`CSI ?1000h`), button
  motion (`CSI ?1002h`, the transcript drag) and SGR coordinates (`CSI ?1006h`), so a wheel
  notch scrolls the transcript instead of the terminal translating it to arrow keys (dogfood
  p5: the wheel called the input history). Deliberately **not** `?1003h` (any-motion): a
  pointer move without a button is not input and never causes a frame. With `mouse = false`
  the app sends no mouse sequence at all and behaves exactly as before — no wheel scrolling
  and no transcript selection/copy. **Cost**: while reporting is on, a plain drag selects via
  the app rather than the emulator; most terminals keep their native selection on
  **Shift+drag** (they do not report a shifted mouse at all — Radian drops such events if one
  arrives), but that is the emulator's choice and is not guaranteed here. Set `mouse = false`
  to keep native selection always.
- `clipboard` (default `osc52`): how a transcript-selection copy reaches the system clipboard.
  `osc52` writes `CSI ]52;c;<base64>BEL` to stdout — no child process, works over ssh, but it
  is fire-and-forget: the status bar therefore says `copied N chars (via OSC 52)`, what
  Radian did, never that the clipboard holds it. `pbcopy` runs macOS's `pbcopy` and can
  report failure honestly; configured anywhere else it is a startup warning and OSC 52.
  Selections over 1 MiB are refused with the reason. `off` keeps the selection visual and
  copies nothing.
- `keybindings` maps an **action** to one or more key strings; a listed action's keys
  **replace** its defaults. Key strings are `[ctrl-][alt-][shift-]key` (`enter`, `ctrl-j`,
  `alt-enter`, `shift-up`, `pageup`, `space`, a single character, …). An unknown action or key
  is ignored with a warning; **one key bound to two actions of the same context refuses
  startup** (the message names the key and both actions).
- Actions: `send`, `newline`, `cursor_left`, `cursor_right`, `word_left`, `word_right`,
  `line_start`, `line_end`, `history_prev`, `history_next`, `delete_before`, `delete_under`,
  `delete_word`, `delete_to_line_start`, `delete_to_line_end`, `scroll_page_up`,
  `scroll_page_down`, `scroll_line_up`, `scroll_line_down`, `scroll_top`, `scroll_bottom`,
  `escape`, `cancel`, `complete`, `find`, `find_prev`, `find_next`, `find_close`,
  `form_next`, `form_prev`, `form_up`, `form_down`, `form_select`, `form_submit`,
  `form_decline`. The defaults are exactly the table above, so an empty config changes nothing.
- **The approval and command-confirmation prompts are not rebindable.** `y`/`a`/`n`, `!` and
  `Esc` are handled before the keymap, so a bad keybinding cannot approve a tool call the user
  meant to deny; `cancel` still works while a prompt is up. `!` (ADR 0026 D9) approves the
  pending request and turns auto on for the session; `Y` stays `y` (allow once) and never
  switches auto on.
- There are three **contexts**, and a binding only conflicts with another binding of its own
  context. The find box (`find_prev`, `find_next`, `find_close`) is a layer over the
  transcript: while it is open its keys are looked up first, so `enter` is `find_prev` there
  and `send` in the transcript, and `ctrl-f` is both "open the box" (`find`) and "earlier
  match" (`find_prev`), exactly as the pre-keymap behavior was. The **question form**
  (`form_next`, `form_prev`, `form_up`, `form_down`, `form_select`, `form_submit`,
  `form_decline`) is the third: it is the one modal whose keys are rebindable (ADR 0024 Q7),
  and `cancel` (Ctrl-C) still runs before it.
- While a completion menu is showing, `up`/`down` (and `menu_next`) move its highlight,
  `escape` closes it (it does not clear the input), and `complete`/`send` accept the highlighted
  row; any other key dismisses it. These follow the keymap, so a rebound `escape`/`complete`
  moves that behavior too. Likewise `find_close`'s `esc` closes the
  box before `escape` would clear the input.
- Note that rebinding `send` away from `enter` leaves `enter` a no-op unless `newline` is given
  it too (as in the example) — the keys are a total map, not layered.

The history is this session's submitted lines (messages and `/…` commands). A resume rebuilds
it from the journal's user messages. It is not persisted on its own.

The **draft** is: on a clean exit (Esc/quit, SIGTERM/SIGHUP) a non-empty input is written to
`<session-dir>/<session-id>.draft` (0600, atomic temp-file + rename); `--resume` restores it
once and deletes the file. An idle Ctrl-C with a non-empty input clears it first (owner
2026-10-14, see the key table), so a Ctrl-C exit saves the *cleared* input; a signal exit
saves what was typed. A panic writes nothing (the panic path does no file I/O). `--list`
and the index only read `*.jsonl`, so a draft is never a session.

## Slash commands (`/`)

A `/…` line is a command: it never reaches the model, the transcript or the journal. Typing `/`
shows the command tree's candidates above the input area; **every level completes**, with one
fuzzy match (a subsequence, prefix matches first): `/mem` → `/memory`, then `/memory fo` →
`forget`. The menu highlights its first row; `Up`/`Down` move the highlight, `Tab` or `Enter`
accepts the highlighted row (it completes the line, it never runs it) and `Esc` closes the menu
keeping the line. `/mem fo` completes both levels at once. The `/theme`, `/model` and
`/thinking` rows mark the value in effect with `● current`, and each `/thinking` row names the
wire value it sends (`enabled:4096` for Anthropic, `high` for openai-compatible).

The levels are data (`command.rs`): the command names from their `/help` lines, `/memory`'s
subcommands, the record ids of `show`/`edit`/`archive`/`restore`/`forget`/`confirm` (listed by
the host — `update` never does I/O), `/export`'s `md|json` and `--force`, and free text for
`/sessions` and `/compact`. A future `/model` (provider → model, ADR 0026) or `/login` is one
more entry in the same tree.


A submitted line whose first non-space character is `/` is a **command**: it never reaches the
model, the transcript or the session journal. `//text` is the escape — one `/` is dropped and
the rest is sent as an ordinary message.

| Command | What it does |
|---|---|
| `/help` | list the commands |
| `/compact [note]` | summarize the history now and keep the recent part (ADR 0018) |
| `/fork` | fork this session at its last turn boundary (ADR 0022); idle only, and it does not switch to the child |
| `/theme <name>` | switch the color theme immediately and write `tui.theme` back to the config file (ADR 0008 revision): a built-in (`dark`/`light`/`mono`) or a `*.json` file in `~/.radian/themes/`. `NO_COLOR` wins, so it does not switch; a read-only config (a newer `schema_version`) applies for the session but is never written |
| `/sessions [query]` | open the session **picker overlay** for this workspace (the `--pick` screen); type to filter, `Tab` full-text search, Enter shows the copyable `--resume <id>` line |
| `/export [md\|json] [path] [--force]` | export **this** session (default `md`, default `<workspace>/radian-<id>.<ext>`) |
| `/memory <command> …` | the same durable-memory commands as `radian memory` (ADR 0021 D7): `list`, `search`, `show`, `add`, `edit`, `archive`, `restore`, `forget`, `purge`, `confirm`, `export`, `import`, `project` / `project new`, `relink`, `split`, `merge` |
| `/jobs [kill <id\|all>]` | list this session's background `shell` jobs (id, state, elapsed, command, output file) as notices, or stop one/all (ADR 0037 decision 4). `kill` sends SIGTERM to the process group, then SIGKILL after 2 s; the model gets a completion message on the busy/idle lane |

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
- `/sessions` opens the session picker overlay (the same one `--pick` runs); the event loop loads the rows, because that reads the session directory. It never switches sessions in place — selecting shows `--resume <id>`, and the composer keeps typing until you navigate into the picker. `/export` uses the same
- `/jobs` lists the session's background `shell` jobs as notices; `/jobs kill <id>` and `/jobs kill all` stop them. A settle the model has not seen arrives as a System message (busy: injected into the running turn; idle: a follow-up turn that wakes it), and after three wake-started turns with no user input it is injected only (ADR 0037 decision 3).
  `radian-host` renderer as `radian exec --export` (redacted, read-only JSONL), writes atomically,
  and refuses to overwrite without `--force`; a relative path is resolved against the
  workspace, an absolute one may point anywhere the user named.
- `/fork` (ADR 0022) creates a child session from this session's effective history at its
  last turn boundary, through the writer this process already holds (no second lock). It is
  refused while a turn is running, and it does **not** switch this window: the notice names
  the child and how to open it (`radian --resume <child>`).
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
| 19 | `--pick` lists this workspace's sessions, filters, full-text searches, resumes the choice (the next request carries its history), `n` starts new, `Esc` exits 0 with no session file; `/sessions` lists a copyable `--resume`; `/export` writes and refuses to overwrite without `--force` | `tests/pty.rs`, `tests/sessions.rs` (host) |
| 20 | Ctrl-C: idle with text, the first press clears it into the history (not a quit); idle and empty, the first press only shows `Press Ctrl-C again to quit` and a second inside 2 s quits with the exit line; a key in between, or the window closing, disarms | `tests/quit.rs`, `tests/pty.rs` |
| 20 | auto (ADR 0026 D9): `!` approves once and enables auto (the next ask never prompts, `AUTO` on the status bar); `Y` stays allow-once; `/auto on\|off`, `/yolo` alias; switch audited + `ConfigChanged` journaled; resume restores on `workspace`, not on `full` (with the hint); config `auto` honored on `workspace`, ignored with a warning on `full` | `tests/auto.rs`, `tests/pty.rs` |
| 21 | syntax highlighting (ADR 0028 R2): keyword/string/comment take the theme's `syntax*` colors, a theme file's value reaches the cells, an unclosed or unknown-language block stays plain, `highlight = false` disables it, a truncated line keeps its colors, and the text is unchanged (copy/search) | `tests/highlight.rs` |
| — | Enter=followup idle / steer running; Ctrl-C cancel-then-quit | `tests/input.rs` |
| — | background jobs (ADR 0037): `shell` `background:true` returns at once, `job_output wait=true` returns the increment and the settle, `job_list`/`job_kill` work, `/jobs` lists and `/jobs kill all` stops them, a settle arrives as a System message, and a crash reports an orphan | `tests/jobs.rs` (coding), `tests/job_notify.rs` (host), `tests/pty.rs` |

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
- **the `ask_user` panel** (ADR 0024): does the tab row read at your width, is the
  current tab obvious, and do the bottom-border hints (which change per tab) make the keys
  findable without `/help`? On the `Other` row, do Up/Down still move between the options while
  the typed text stays? Does `alt-h` collapse to one line and back without losing answers?
  pty automation asserts the text, not the feel (ADR 0009).
- **the shell approval box with a long command**: a real command line is often longer than a
  file path. Does the `command:` line stay readable at your width, or does the wrapped preview
  become a wall of text? The pty test only asserts that the whole command is present.
- **the budget status segment**: with a small `--context-window`, is the `T / B` projection
  and the reported-vs-estimated wording readable at your width, and does the over-budget
  highlight stand out without looking like an error?
- **the footer** (ADR 0027 C2, revision 1): the default is the `full` two rows (identity, then
  numbers). Are the brand `R`, the hostname, the repo/worktree name and the change counts
  right, and do the numbers row's `speed`/`cost`/`elapsed` read as expected? Does the context
  share/`~` marker read at your width, and does a narrow terminal drop the low priorities
  first? Try `tui.footer.preset = "pi"` and `"classic"` and compare.
- **`!` and `!!`** (ADR 0033): type `!ls` — does the `$ ls` block and its color read right, and
  does the output arrive without a perceptible stall? Type `!!ls` — is the dimmer block clearly
  different? Does `!sleep 30` cancel with Esc, and does a second `!` while one runs say so? Is
  the sandbox visible (a write outside the workspace fails with a readable line) and does the
  command never open an approval box? On the `safe` tier `!` must refuse with one line.
- **the `AUTO` status marker** (ADR 0026 D9): turn auto on with `/auto on` — does the reversed
  `AUTO` segment stand out as "this session behaves differently" in `dark`, `light` and `mono`,
  without reading as an error? pty automation asserts the text, not the prominence.
- **the `/access` confirmation and marker** (ADR 0038 slice 2): type `/access` — does it name
  the current tier and its source (`flag`/`config`/`project`/`session`/`default`)? Type
  `/access full` — does the confirmation panel make "full means no sandbox" obvious, is the
  default highlight on `cancel`, and do ↑/↓/Enter/Esc feel right? After "remember for this
  project", restart — is the tier applied and is the `(project)` marker readable in the footer
  and the startup summary? pty automation asserts the text, not the feel (ADR 0009).
- **the `/mode` switch** (ADR 0038 slice 3): type `/mode` — does it name the mode in effect?
  Type `/mode lead` — does the notice say the role changes from the next turn, and does the next
  reply read like a project lead (it plans, briefs and reviews) rather than a coding agent?
  `/mode general` switches back; a resumed session must open in the mode it was left in.
- **the input area's three styles**: is the default `bar` band
  comfortable — the rail clear, the band reaching the right edge, multi-line and a CJK line
  banded on every row, and the status row above it (`◜  Working · 12s · esc to interrupt`) not
  shifting the band or the transcript? Compare `input_style = "lines"` (the rule above and
  below, `> `) and `input_style = "box"`.
- **notice and hint readability**: `/help`'s output and the placeholder must be
  readable on your background. `dark` now uses the terminal's default foreground for notices and
  `Gray` for hints; is that right on your terminal, and is the light palette's hint (dark gray on
  light) readable?
- **the slash completion's fuzzy order**: type `/` and a few letters — are the
  candidates in the order you expect, and is the description column readable at your width? Is
  the one-key-accepts-it rule (Tab/Enter) comfortable, or does Enter accepting a candidate
  surprise you?
- **the default workspace**: run `radian` with no `--workspace` from a
  project directory — the file tools must be there and the sandbox must confine them there. From
  `$HOME` the startup line must say `workspace: ~/tmp (cwd is $HOME)`, and the filesystem root
  must be refused.
- **`--resume` without a value**: does the picker list only sessions (no `n`),
  and does Esc leave with nothing created?
- **the exit line**: after Ctrl-C twice (or a SIGTERM), is the printed
  `resume: radian --resume <id> …` command paste-ready in your shell, with the paths and
  quoting right?
- **mouse selection/copy**: with the default `tui.mouse = true`, the app reports the wheel,
  buttons and button motion (`?1000h ?1002h ?1006h`) and owns the transcript selection
  itself: press, drag, release copies (double-click a word, triple-click a message). Drag to
  the top or bottom edge and hold: the view auto-scrolls. Verify the copy lands where you
  expect (`tui.clipboard`: `osc52`/`pbcopy`/`off`) and that the status bar's `copied N chars
  (via …)` wording is what you want. **Shift+drag** stays the emulator's native selection on
  most terminals (Radian drops shifted mouse events) and `mouse = false` restores it
  everywhere — then the wheel is back to the terminal's alternate-scroll translation and no
  longer scrolls the app.
- **the mouse wheel**: with `tui.mouse = true` (default), type a draft in the message box and
  scroll the wheel **up** — the transcript must move up by three lines per notch and the draft
  must stay (dogfood p5 was the wheel replacing it with the input history). Scroll down: it
  returns toward the bottom. Open the session picker (`/sessions`) and scroll: the selection
  moves, not the transcript. Open the approval prompt and scroll: nothing moves.
- **the MCP approval box**: with `--mcp-config`, an MCP tool call prompts like any other. Is
  the tool name (`server__tool`) and the argument preview clear at your width, and is it
  obvious the server runs outside the sandbox (the startup notice says so)?
- **the picker**: run with `--pick` in a workspace that has a few sessions. Is the list
  scannable, does typing a filter feel responsive, is the highlighted row obvious, and does
  `Tab` full-text search read clearly (the snippet vs the summary)? Is it obvious that `Esc`
  leaves and `n` starts fresh? pty automation asserts the rows and the keys, not the feel.
- **in-transcript search**: in a long session, `Ctrl-F` and type a word that appears once
  near the top. Does the jump land where you expect, is the highlighted match obvious, and is
  `match i/N` readable? Press Enter / Ctrl-G a few times: does moving between matches feel
  like a terminal's scrollback search, and does Esc leave you where you were? pty automation
  asserts the jump and the status, not the feel.
- **`/help` and a `/memory` write**: type `/help` (a notice, no model request), then
  `/memory forget <id>` — is the `[y/N]` prompt obvious and does `n` feel like a cancel? pty
  automation asserts the notices and the keys, not the readability.
- **the composer's feel**: type a paragraph and edit in the middle — does Left/Right/Home/End
  (Ctrl-A/Ctrl-E) land where you expect, does a word jump (Ctrl-Left) feel right, and do
  Ctrl-W / Ctrl-U / Ctrl-K delete the amount you meant? Press Up/Down at the first/last line:
  is the history/draft handoff obvious? Try a CJK line and an emoji: does the cursor stay on
  the character you expect at your width? pty automation asserts the cursor cell, not the feel
  (ADR 0009).
- **the growing input box**: paste or type more than ten lines — does the box cap at ten rows
  and scroll to the cursor, or does the transcript get squeezed uncomfortably?
- **the status bar at your usual width**: is the tier / usage / mcp / context order readable,
  and does the truncation (`…`) drop what you would have dropped? The pty and `TestBackend`
  tests assert the text and the 60-column rule, not the feel.
- **the `@` completion list**: in a workspace with many files, type `@` and press Tab twice.
  Does the first press extend to the common prefix and the second show the list above the input
  box? Is the list readable at your width (10 rows plus `+N more`), does Esc close it without
  clearing the line, and does typing dismiss it? pty automation asserts the text and the keys,
  not the feel.

Suggested 3-line manual pass (in a throwaway terminal, not your working one):

```sh
cargo run -p radian-tui -- --model <model> --base-url <...> --session-dir /tmp/radian-tui-manual
# 1. send a message; while it streams, PageUp and keep sending: the view must not jump back.
# 2. Ctrl-End; a new reply should follow. Paste a 5-line block, edit it with the arrows, and
#    press Ctrl-J then Up: the cursor should move between the input's lines, not the history.
# 3. drag-select some transcript text and copy it; then Ctrl-C twice and confirm the prompt
#    is clean (no `%`) and `stty -a` still shows `icanon`.
```

### Owner hand test, 2026-10-14 (everything merged tonight)

pty automation (`vt100` screen model, instant query answers, no glyph metrics) cannot judge
the items below. Each one says what to do, what to expect, where to look, and what to record
when it fails. **Record on every failure**: a screenshot, the terminal name and version,
`echo $TERM $COLORTERM`, the font, and the smallest step that reproduces it. Run in a
throwaway terminal, not your working one.

1. **`/theme` switching and write-back** (`/theme <name>`, ADR 0008 revision + ADR 0029;
   merge `22b2753`, `01177c2`). Put three themes in `~/.radian/themes/`
   (`mkdir -p ~/.radian/themes`; `cp crates/radian-tui/tests/data/themes/{kanagawa-wave,gruvbox-light}.json ~/.radian/themes/`).
   *Steps*: `/theme` + Tab → pick `kanagawa-wave`; `/theme` → `gruvbox-light` (a light
   background); `/theme` → `mono`. Then `Ctrl-C` twice, reopen `radian` in the same directory.
   *Expect*: the whole screen recolors **immediately**; `mono` has no color at all (only
   bold/dim/underline); the reopened session starts in the theme you last chose; the config
   file at `~/.radian/config/config.toml` has `tui.theme = "<name>"` and nothing else changed
   (comments, key order, unknown fields). *Look*: the `user`/`assistant`/`reasoning` roles and
   the status bar; the light theme must not have dark-on-dark text.
   *Also*: `NO_COLOR=1 radian` must force `mono` and `/theme dark` must **not** switch (a
   notice says so). *Fail*: the theme applies only after restart (not immediate), a
   non-theme line changed in the config, or `NO_COLOR` still colors.
2. **Startup wordmark, Tip, Ctrl-P** (ADR 0027; merge `22b2753`). *Steps*: open `radian` in a
   wide terminal (≥80 cols); `Ctrl-P`; reopen at ~40 cols; open again at ~20 cols.
   *Expect*: wide → the three-row wordmark plus one collapsed info line, one `Tip:` line, and
   the safety/warning lines (never folded); `Ctrl-P` folds/unfolds only the info line; narrow
   → the wordmark drops to the single line `<brand> radian <version>` (`R` unicode, `󰰠` nerd,
   `rad` ascii); ≤ its width → no wordmark. *Look*: **does the brand mark render?** The unicode
   `R` is one column; the nerd mark needs a Nerd Font (a box means it is missing). *Fail*:
   record the font name and what you see.
3. **Rounded input box and `❯`** (`tui.input_style = "box"`; merge `22b2753`; the prompt and
   cursor fixes are `pi/ui-r3`). *Steps*: set `input_style = "box"` in `[tui]`, restart; type a
   multi-line draft (`Ctrl-J`); resize. *Expect*: rounded corners, the `❯` prompt **in `lines`
   and `box`**, the border in the theme accent (`notice_label`); the cursor sits right after the
   text (empty: one cell into the prompt; the old byte-length bug put it two cells right); the
   continuation line aligns under the text; no corner breaks at any width. *Fail*: record the
   width where a corner/glyph breaks.
3c. **The `bar` input and its status row** (ADR 0035 V3; default). *Steps*: open `radian`
   (no `input_style`), type a one-line draft, then a `Ctrl-J` multi-line one, then a CJK line;
   send a message and watch a turn run; resize to 40/80/120. *Expect*: one banded row per input
   row, the accent `▎` rail on each, the band reaching the right edge (never a highlight that
   stops at the text); above the band one always-present row — blank when idle, `◜  Working ·
   12s · esc to interrupt` while a turn runs — and neither the band nor the transcript moves
   when the turn starts or ends; the cursor after the rail (CJK two cells). *Look*: is the rail
   the accent and is the band the same color as a user message block? *Fail*: record the width
   or the font where it reads wrong.
3b. **Shift+Enter** (`pi/ui-r3`). *Steps*: in Terminal.app, iTerm2/Ghostty (or another
   kitty-protocol terminal) and inside herdr, press `Shift+Enter` in a non-empty draft; then
   `Ctrl-J`. *Expect*: Shift+Enter inserts a newline where the terminal reports it (herdr and
   kitty-protocol terminals), and `Ctrl-J` always does; a terminal that cannot report it sends
   instead - record which of the three did what. *Fail*: a newline that also sends.
3c. **Spinner** (`tui.spinner`; ADR 0027/9/10). *Steps*: start a turn (send a
   message) with `tui.spinner = "pulse"` (the default), then `"arc"`, then `"quadrant"`, then
   `"braille"`, then `glyphs = "ascii"`. *Expect*: the input rule shows
   `── <spinner>  Working ` (two spaces) while the turn runs, a plain rule when idle; `pulse`
   is a `⏺` that only changes color (500 ms); `arc`/`quadrant`/`braille` turn every 250 ms;
   `ascii` is `*` (pulse) or `| / - \`. *Look*: **does the spinner render in your terminal
   font?** `pulse` (`⏺`) and `arc` are the documented exceptions (no checked font has `⏺`; a
   box means the fallback font lacks it); `quadrant` is the covered alternative. *Fail*:
   record the font and what you see.
4. **Ctrl-C** (merge `22b2753`, `01177c2`). *Steps*: type text, `Ctrl-C` (clears; press `↑`
   to find it); empty the input, `Ctrl-C` (status shows `Press Ctrl-C again to quit`), then
   **resize the window** and `Ctrl-C` again inside 2 s; then `Ctrl-C` twice with no resize.
   *Expect*: typing clears first; the quit window survives a **resize** (only a real key
   press disarms it); two presses exit with the `session … saved; resume with …` line and a
   clean shell prompt. *Fail*: record whether the resize disarmed the quit (the `01177c2` fix)
   and whether the exit line is paste-ready.
5. **Message and tool-card backgrounds, selection on top** (ADR 0028; merges `0f5a302`,
   `608ce64`, `8456074`). *Steps*: send a message that calls a tool; drag-select across a user
   message and a tool card; double-click a word, triple-click a message.
   *Expect*: the user message and each tool card paint a **full-width** background band; the
   selection highlight is visible **on top of** those bands (not hidden by them); the copied
   text matches what you selected. *Look*: contrast in `dark`, `light`, `mono`. *Fail*: record
   which role loses the selection and the theme.
6. **Thinking fold `Ctrl-T` and tool output `Ctrl-O`** (ADR 0028 R3/R4; merges `0f5a302`,
   `608ce64`). *Steps*: with a reasoning model (e.g. `--thinking adaptive`), get a reply with
   reasoning; press `Ctrl-T`; then call a tool with long output and press `Ctrl-O` three times.
   *Expect*: `Ctrl-T` toggles reasoning between collapsed and expanded (default collapsed;
   `tui.display.reasoning` sets the start); `Ctrl-O` cycles tool output
   `errors → expanded → collapsed → errors` (default `errors`; `crates/radian-tui/src/app.rs`, `ToggleToolOutput`); the truncated body shows the `[N more
   lines]` marker rather than silently cutting. *Fail*: record the fold state after each press
   and the config value.
7. **Mermaid** (ADR 0028 R8; merge `5628737`). *Steps*: ask for a small ` ```mermaid `
   flowchart; widen then narrow the terminal past the `TooWide` threshold, then widen again.
   *Expect*: wide → a Unicode diagram at the body width; narrow → the **source** plus one dim
   `mermaid: <reason>` line (never half a diagram); widening re-renders it. *Look*: node/edge
   colors from the theme (`mermaid_node`/`mermaid_edge`/`mermaid_text`), and `mono`.
   *Fail*: record the reason line and the widths.
8. **Code blocks** (ADR 0028; merge `0f5a302`). *Steps*: get a long single-line code block,
   then a block with CJK characters.
   *Expect*: code **never wraps**; a line wider than the body ends in `…`; CJK code lines
   align on the character grid. *Fail*: record the terminal width and whether the `…` is
   visible (not clipped).
9. **`/model`, the confirm box, and `/thinking` completion** (ADR 0026 D5–D8, D11; merge
   `1303db4`). *Steps*: `/model` + Tab (two levels: provider, then model); pick a model that
   makes the context over budget (or use a small `--context-window`); `/thinking` + Tab.
   *Expect*: the switch is queued for the next turn boundary (the status bar's `next:` shows
   it); when over budget a confirm box asks whether to compact first (`y` compacts then
   switches, `n` cancels); `/thinking` completion lists **only** the current model's supported
   levels, and an unsupported level clamps with a notice. *Fail*: record the box text, the
   candidate list, and the model.
10. **`/login` wizard and `/logout`** (ADR 0026 D3/D10; merge `c120b0e`). *Steps*: `/login`
    → `custom`; step through name, protocol, base URL, key; watch the key field; then
    `/logout <provider>`.
    *Expect*: the key is **never echoed** (masked while typing and absent from every
    notice/error); the verification request runs before saving and a failure lets you cancel
    or save anyway; `/logout` asks first, then says the provider was removed. *Look*: the
    wizard's layout at your width. *Fail*: record any step that echoes the key, and the
    verification error text (it must name a redacted URL, never the key).
11. **`/skill:` and `/skills`** (ADR 0025 slices 3+4; merge `1a8c0e8`). *Steps*: put a skill
    under `~/.radian/skills/<name>/SKILL.md` (or a `[skills] extra_paths` root); type
    `/skill:` + Tab; run one with args; run `/skills`.
    *Expect*: completion lists the discovered skill names; `/skill:<name> args` folds the body
    into one user message containing the args; `/skills` lists name + description, with
    `(model-invocation off)` for a `disable-model-invocation: true` skill; an unknown name is a
    notice, not a message. *Fail*: record the completion list and the folded message.
12. **auto mode `!` and the `AUTO` marker** (ADR 0026 D9; merge `8086b40`; also in the bullet
    list above). *Steps*: trigger an approval; press `!`; trigger another. *Expect*: `!`
    approves once **and** turns auto on for the session (the next ask never prompts), the
    status bar shows the reversed `AUTO`; `Y` stays allow-once. *Look*: does `AUTO` stand out
    in `dark`/`light`/`mono` without reading as an error? *Fail*: record the theme.
13. **OSC 52 copy in herdr** (ADR 0029; merge `8456074`; a long-standing question).
    *Steps*: select transcript text and copy (default `tui.clipboard = "osc52"`); paste into
    another app. Then try `tui.clipboard = "pbcopy"` (macOS) and `"off"`.
    *Expect*: `osc52` actually reaches the system clipboard **inside herdr** (a terminal
    multiplexer may or may not forward OSC 52); the status bar says `copied N chars (via …)`;
    `pbcopy` works on macOS; `off` writes nothing and says so. *Fail*: record herdr's version
    and whether OSC 52 forwarding is enabled — this is the known-open question.

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
| startup/config refusal (bad workspace, failed probe, bad MCP file, bad header, ...) | 1 | `radian: <the error>` (terminal never entered) |
| driver task ends with a `RunError` | 1 | `radian: driver: <error>` |
| driver task panics | 1 | `radian: the driver task ended unexpectedly: <join error>: <redacted panic>` |
| the app itself panics (`main` thread) | 101 | `radian: the app panicked: <redacted panic> (panic log: <path>)` |
| `--list` | 0 | none: the listing is the output |
| `--pick` quit (Esc/Ctrl-C) | 0 | none: the picker created nothing |

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
- Large transcripts: virtualized since 2026-10-13 (dogfooding D9; `tests/virtual.rs`, frame times in `docs/perf-baseline.md` §4). Not done: the first frame after resuming a large session still measures every entry once (ADR 0023 Step 0 measured it: 50k messages → **2.2–4.7 s** to the first frame; a width change also re-wraps the whole transcript, ~70 ms at 50k). Those Step 0 numbers come from `TestBackend` and a real pty (`crates/radian-tui/tests/perf_render.rs`): they measure our render cost and the bytes arriving at a pty, **not** the draw latency of a real terminal emulator.

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
  exits without touching the terminal. A closed stdout (`radian --list | head -1`) stops
  quietly with exit 0, never a panic (dogfooding D21).
- `--pick` opens the session **picker overlay** (the same one `/sessions` uses) in the one
event loop, before any session exists: `Up`/`Down` select, type to filter by id/summary,
  `Tab` (or `Ctrl-S`) toggles full-text search, `Enter` resumes the selection, `n` starts a new
  session, `Esc`/`Ctrl-C` quits. It never creates a session file, and the whole process enters
  the alternate screen **once** (the picker and the session share it). `--pick` conflicts with
  `--resume` and `--session-id`. With no sessions it goes straight to a new session and says so.

`--resume`, `--session-id` and `--list` are mutually exclusive pairwise.

## Flags

`--model`, `--base-url`, `--api-key-env` (default `OPENAI_API_KEY`), `--header NAME=VALUE`,
`--system`, `--session-dir`, `--session-id`, `--resume`, `--list`, `--pick`, `--version`,
`--workspace DIR`, `--mcp-config FILE`,
`--profile safe|workspace|full` (default `workspace`; the old `workspace-write`/`full-access`
are accepted aliases but not listed),
`--approval on-ask|always|never` (default `on-ask`), `--allow-shell-command PREFIX`,
`--context-window TOKENS`, `--offline-catalog`, `--catalog-provider PROVIDER`. The key is
read only from the named environment variable and is never printed.

`--version` prints `radian <version>` and exits 0 before anything else runs: no
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
  linked" with the `radian memory project new`/`relink` hint) and the injection budget
  (ADR 0021 D2/D6). The agent's four memory tools are mounted and the current memory is
  injected into the system prompt each turn; `memory_search`/`memory_get` are allowed and
  `memory_save`/`memory_revise` ask. A default-scope `memory_save` or `/memory add` that
  **registers** this workspace as a project (ADR 0021) is shown as a `Notice`
  transcript entry — one line naming the new project and its identity key — because stderr
  would be drawn over by the fullscreen UI. Tested by
  `pty_the_memory_notice_names_the_project_state_and_the_budget` and
  `pty_a_memory_add_shows_the_created_project_notice`.

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
  `radian exec --yes`. It removes the question only: `deny` rules still deny, the sandbox still
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

### The pending queue and the structured question (ADR 0023 U7 / ADR 0024)

Approvals and `ask_user` questions share **one** arrival-ordered queue in `radian-host`
(`PendingQueue`), and the UI shows its head: a second request waits behind the first instead of
superseding it. The queue is a wake-up source (its `watch` receiver is a `UiEvent::Pending`), so
a waiter that gives up — a cancelled turn — removes the modal immediately, without waiting for
anything else. The status bar counts the queue (`N pending`) while it is not empty. Answering
goes through the session facade (`answer`), which re-validates the `AnswerRequest` capability,
so a facade without it refuses with a notice rather than resolving a request.

`radian` always registers `ask_user` (ADR 0024 Q5); `agent` never does and its startup line
says `ask_user: off (no interactive user)`. The tool is fixed `allow` in every tier (ADR 0024
Q6): it writes nothing and runs nothing, and the answer is data, not a permission.

A question request is one `Modal::Question`, drawn with the shared panel primitive (ADR 0035
revision 1, ADR 0024 R2): a titled rounded box, a **tab row** (each question's
`header`, capped at 16 display cells, a `✓` on an answered one, then `Submit`), the current
question's options (a `❯` cursor, `○`/`◉` single, `☐`/`☑` multi, ` (Recommended)`, a `↳`
description), the `Other` row with inline editing, and the key hints in the bottom border
(they change with the tab). `alt-h` collapses the panel to one line.

```
╭─ Ask · <session> ───────────────────────────────────────────╮
│  Load  Scope  Submit                                        │
├─────────────────────────────────────────────────────────────┤
│ How should the UI extension be loaded?                      │
├─────────────────────────────────────────────────────────────┤
│ ❯ ○ Built-in Rust components                                │
│   ○ External process plugins                                │
│      ↳ a separate program, JSON-RPC over stdio              │
│   ○ Other (type your own)                                   │
╰─ ⏎ select · n note · ↑/↓ move · ⇥/←/→ question · esc cancel…╯
```

| Key | Action |
|---|---|
| `Tab`/`→` / `Shift-Tab`/`←` (`form_next`/`form_prev`) | the next/previous tab (a question or `Submit`), wrapping |
| `Up` / `Down` (`form_up`/`form_down`) | the option cursor; on the `Other` row it still moves between options and keeps the typed text (a `text` question and a note are multi-line, so there it moves the composer's cursor) |
| `Space` (`form_select`) | choose the highlighted option without advancing (toggle it for a `multi` question) |
| `Enter` (`form_submit`) | save a note; a `single` selects **and advances**; a `multi` toggles; on `Submit`, submit |
| `n` | a note for the current question (type, `Enter` saves, `Esc` cancels) |
| `Esc` (`form_decline`) | ask once; a second `Esc` declines (`user_declined`) |
| `alt-h` | collapse the panel to one line (`Ask · 2/3 answered · alt-h to show`) |
| typing | the focused free-text field (a `text` question, the `Other` row, or a note), with the composer's editing keys |

The transcript also shows the `ask_user` call and result as a boxed block (ADR 0024
R1): the same questions, the chosen options marked `◉`/`☑`, a `↳` description, and the
declined/cancelled status; `glyphs = ascii` falls back to `+ - |`, `(*)`/`[x]` and `->`.

A `required` question with no answer blocks the submit with a notice. The drafts live in the
form, so moving between questions keeps them. These are the **only** rebindable modal keys; the
approval's `y`/`a`/`n`/`Esc` stay hardcoded, and Ctrl-C still cancels the turn before the form
sees the key (ADR 0023 U4, invariant 1). While a question is up, the model's streamed text
before it stays on screen.

## Syntax highlighting (ADR 0028 R2)

A **closed** fenced code block whose info string names a common language is highlighted with the
pure-Rust `syntect` (the `regex-fancy` backend; no oniguruma). While the fence is open
(streaming) the block is plain; it is highlighted when the closing fence arrives — the same
"only the fixed blocks are rendered" rule as Mermaid. An unknown language, or no info string,
stays the code block's uniform style. Highlighting changes **colors only**, never the text:
selection, copy, `Ctrl-F` and the line count all see the same characters (ADR 0028 R9's
no-wrap and `…` truncation are unchanged).

**Colors come from the Radian theme, not from syntect.** syntect's own themes are not compiled
in; the parser yields one of nine roles, and the active theme resolves it (ADR 0029). A theme
file's `syntax*` values and `[tui.colors]` overrides reach them like any other role; `mono`
uses no color (a comment may be italic). `tui.display.highlight = false` (config) renders every
code block as plain text.

| Role | syntect scope (the innermost/highest-priority match) |
|---|---|
| `syntax_comment` | `comment*` |
| `syntax_keyword` | `keyword*`, `storage*` (Sublime's `fn`/`let`/`struct`) |
| `syntax_function` | `entity.name.function*`, `support.function*`, `entity.name.tag*` |
| `syntax_variable` | `variable*`, `meta.function.parameters*` |
| `syntax_string` | `string*` |
| `syntax_number` | `constant.numeric*` |
| `syntax_type` | `entity.name.type/class/struct/enum/namespace*`, `support.type/class*` |
| `syntax_operator` | `keyword.operator*` |
| `syntax_punctuation` | `punctuation*` |

The table is ordered by **priority**, not just innermost-first: a comment's `//`
(`punctuation.definition.comment`) and a string's quotes (`punctuation.definition.string`) stay
comment/string-colored because comment and string outrank the punctuation inside them. Anything
else is `Plain` (the code block's own style). The mapping lives in `src/highlight.rs`.

**Languages** (the info strings that highlight) are: `rust`/`rs`, `python`/`py`,
`javascript`/`js`/`jsx`, `typescript`/`ts`/`mts`/`cts`, `tsx`, `go`/`golang`,
`sh`/`bash`/`shell`/`zsh`, `json`, `yaml`/`yml`, `toml`, `markdown`/`md`, `diff`/`patch`, `c`,
`c++`/`cpp`/`cc`/`cxx`, `c#`/`csharp`/`cs`, `java`, `ruby`/`rb`, `php`, `sql`, `html`/`htm`,
`css`, `xml`, `lua`, `perl`/`pl`, `r`, `scala`, `haskell`/`hs`, `make`/`makefile`,
`clojure`/`clj`, `erlang`/`erl`, `ocaml`/`ml`, `objective-c`/`objc`/`mm`, `latex`/`tex`,
`matlab`, `lisp`/`el`, `graphviz`/`dot`.

The grammar data is syntect's packed default set (75 grammars) plus three grammars vendored
under `third_party/syntaxes` (`TypeScript`, `TypeScriptReact`, `TOML`; see its
`RADIAN-CHANGES.md` and the root `NOTICE`) — syntect's set does not ship those. A hand-reduced
`.packdump` of the default set was attempted first; syntect resolves cross-syntax context
references at build time, so a hand-picked subset panics unless the full transitive closure is
computed. `build.rs` instead loads the default set, parses the three vendored YAML grammars and
dumps the **combined** set to `$OUT_DIR` (parsing the ~200 KB TypeScript YAML with `yaml-rust`
costs ~2 s each, which would be a visible freeze on the first code block at runtime); the binary
embeds that dump and only deserializes it. The **language table above is what limits
highlighting**; a grammar the set ships but the table does not name renders as plain text.

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

The effective input budget is **`B = min(I, C − R)`**, with **`R = max(min(16384, ⌊C/4⌋), the
request's max output)`** (ADR 0018: a quarter of the window, capped at 16384, raised
by the output the request itself sends). The startup notice names the source and every number
(`C 1000000, I 922000, output 384000, R 16384, B 922000 from models.dev (opencode/…, cached 3 h
ago)`, `B 15000 from --context-window` for C = 20000, or a `no budget` line). `C ≤ R` is refused
at startup: the window cannot hold the reservation (with the adaptive `R` it needs the request's
own output to reach `C`).

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

A tool call shows as `-> <the card's header>` and its result as `<- <the card's body>` (banded
by state: pending / success / error). The header and the body come from a per-tool renderer,
looked up by name in a compile-time registry with a generic fallback:

| Tool | Header | Body |
|---|---|---|
| `shell` | the command and the exit code | the output (the `--- stderr ---` section on the warning level) |
| `read` | the path and the line range | collapsed by default (`[read: N lines]`) |
| `patch` | the path and `+N/-M` | the `-old` / `+new` replacement, no invented context |
| `write` | the path, byte count and line count | the content written (no `+` markers: nothing to compare) |
| `git_diff` | the file count and `+N/-M` | git's unified diff, colored by the `diff_*` roles |
| `git_status` / `list` | the entry count | the tool's lines |
| `ask_user` | the question count | one `prompt → answer` line per question |
| `memory_search` / `_get` / `_save` / `_revise` | the operation and the record id | the tool's record summary |
| anything else (MCP included) | the name and the arguments' JSON summary | the result text |

The lookup is on the **exact** built-in name: an MCP tool is aliased `server__tool`
(`agent-extensions`' `alias.rs`), so a server-provided `read` arrives as `fs__read` and takes
the generic card — its arguments and output are that server's own, not the built-in's.

`tui.display.tool_output = "expanded" | "errors" | "collapsed"` (default `errors`) and
`tui.display.tool_preview_lines` (default 5) fold the bodies; `Ctrl-O` (`toggle_tool_output`)
steps through them; a failed call's body is never hidden. The status line still carries the
`tool <name> running` note, so the call, the wait for approval, execution and the result stay
distinguishable states.

## Compaction summaries and search over folded content (ADR 0028 R5/R6)

A compaction summary (a `User` message with a `compaction_summary` origin, ADR 0018) shows as
one line: `── context compacted (rev 3, 12000 → 4000) ──`. Expanding it (the tool-output fold,
`Ctrl-O`) shows the model's summary and the user's words Core keeps verbatim (ADR 0018 revision
6). The revision and the token counts are **live only** — the journaled message does not carry
them, so a resume replay shows `── context compacted ──` rather than inventing numbers.

`Ctrl-F` matches the **full** content: a folded thinking block, a folded tool body and a folded
summary are all searched. A hit in a folded entry opens **exactly that entry** (a per-entry
override, part of the layout cache key) and the status bar says `expanded 1 folded entry for the
match`; the other entries stay folded. The override is dropped by the next global fold toggle
(`Ctrl-T` / `Ctrl-O`); closing the search box keeps it, so the text the user searched for does
not disappear again. The jump centers the match's **display** line, not its expanded one.

Already a Notice-style one-liner (verified, R5): the memory **fixed block** never enters the
transcript (the hook appends to the system prompt) and its only transcript trace is the startup
`[memory] …` notice. A later turn's addition **does** enter it, as a `System`-source message
(ADR 0021), which the TUI renders like any other `System` message (the same shape as
ADR 0037's completion notice); an approval is a modal, not a transcript entry (the call and its
result are the card). A pending question's result arrives as the `ask_user` card.

## Mermaid blocks (ADR 0028 R8)

A fenced code block whose info string is `mermaid` (ASCII case-insensitive) and which is
**closed** is drawn as a Unicode diagram with the vendored `codex-mermaid` (R8), at the
transcript body width. While the fence is open (streaming) the source shows; the diagram
appears when the closing fence arrives. `tui.display.mermaid = false` (config) renders every
code block as source.

A diagram the vendor cannot render (unsupported syntax, its limits, or too narrow) keeps the
**source** and puts one dim `mermaid: <reason>` line first — never a half diagram. Widening the
terminal re-renders: the diagram cache is keyed by `(width, text_len)`, so a `TooWide` failure
can succeed on a wider screen.

The three diagram roles take their colors from the theme (ADR 0029): `mermaid_node` (pi's
`accent`), `mermaid_edge` (pi's `dim`) and `mermaid_text` (pi's `text`). `mono` uses modifiers
only; `[tui.colors]` overrides each like any other role.

Selecting a diagram selects its block (the diagram has no source offsets of its own), so a copy
yields the Mermaid **source** — the text that actually exists. Ctrl-F searches the diagram's
rendered lines (node labels included) because `body_text_lines` returns what is drawn.
