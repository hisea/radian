# ADR 0027: TUI Chrome, Footer, and Contributions

Status: accepted

## Decisions

### C1 Input box style

`tui.input_style` has two levels `"lines"` and `"box"`, default **`lines`** (two horizontal lines, `❯ ` prompt; the top line carries spinner and state while a turn runs). `box` is a half-frame box with the same shape as code-block open frames. `bar` is not valid: writing it warns at startup and falls back to `lines`. Spacing, glyphs, and acceptance details for both levels live in ADR 0035.

### C2 Footer is a composition of components

- **Rows**: `[[tui.footer.rows]]` each row is a table. `left` and `right` are both optional, plus optional `separator` and `position = "footer" | "above_input" | "below_input"` (default footer). `[tui.widgets]` converts on read with old semantics and warns for one version.
- **Items**: one string (id only, with default icon and format) or one table (`item` required, plus optional `icon`, `format`, `style`, `symbol_style`, `priority`, `thresholds`, and item-only parameters).
- **Component model**: each built-in item is a `Component` (internal trait, not public): id, placeholders, four default icon sets (unicode, nerd, ascii, emoji), default role, priority, thresholds, `data(&App)`. The render pipeline knows no id. Contributor-registered items are also `Component`. The only public extension point is the `command` component.
- **Presets are data**: `deck`, `full`, `pi`, and `classic` are each one TOML file (`crates/radian-tui/presets/<name>.toml`) through the same parser as user configuration. `radian footer export <preset>` prints it. Default preset is `deck`:
  - Row 1: provider usage component. Without usage data this row never appears.
  - Row 2: left group `model` · `cwd` · `git_branch` (with change count) · `worktree`; right group `context_bar`/`context` · `reasoning` · `queued` (shows followup count when pending; hidden at 0).
  - Row 3: right group `tokens` (`↑in ↓out R{cache}`) · `cost` · `context` (`{percent}/{window}`, window in K/M).
  - The `full` preset holds `jobs`; the other three presets do not (users add it in rows).
- **Built-in items**: `brand`, `host`, `model`, `reasoning`, `tier`, `cwd`, `git_branch`, `worktree`, `session`, `context` (with `{bar}`), `tokens`, `cost`, `speed`, `elapsed`, `pending`, `queued`, `mcp`, `scroll`, `status`, `jobs`, plus contributor items. An item without data shows empty. It never invents numbers.
- **Placeholders**: `model`: `{display_name}`, `{id}`, `{provider}`. `cwd`: `{path_home}`, `{path}`, `{basename}`, `{repo}`, `{worktree}`. `git_branch`: `{branch}`, `{dirty}`, `{untracked}`, `{modified}`, and more. `context` and `context_bar`: `{bar}`, `{percent}`, `{used}`, `{budget}`, `{auto}`. `tokens`: `{in}`, `{out}`, `{cache}`, `{cache_write}` (default format omits it). `cost`: `{usd}`. `pending` and `queued`: `{count}`.
- **Style by threshold**: `thresholds = [{ at = 70, style = "warn" }, { at = 90, style = "error" }]` is the default two levels.
- **Per-field configuration**: `[tui.footer.items.<id>]` writes that item default `symbol`, `style`, `symbol_style`, `format`, and `disabled`. Inline tables in rows win. `format` can color one span with `[text](role)`. `glyphs` decides the default `symbol`; a per-item `symbol` overrides it.
- **Colors**: `style`, `[text](…)`, and `symbol_style` accept theme roles or literal colors (named colors, `#rrggbb`, 256-color numbers, with `bold`/`italic`/`dimmed`). Separator color is `[tui.footer] separator_style` (default `footer_separator`, one level weak in built-in themes). An unparsable color is a configuration error.
- **Default separator is ` · `**. `glyphs` defaults to `unicode`. `nerd`, `ascii`, and `emoji` are optional.
- **Compact numbers**: tokens and windows use K/M (below 10 one decimal, 10 and above integers; below 1000 as-is). Percent uses one decimal. Cost uses `$` with 3 decimals. Items separate with one space and no label words.
- **Context fill line**: `[tui.footer] context_line = "off" | "percentage"` draws a fill line between left and right groups for context use.
- **`transparent`**: `tui.footer.transparent = true` leaves the footer without background color.
- **Tolerance**: unknown item ids, unknown placeholders, and unknown parameters show as-is and warn once. They never block startup. Narrow terminals omit right-side items first, then left-side items from the end. `priority` can override default priority.
- Items read only projections and the contribution store. **Drawing does no I/O.** `git_branch`, `cwd`, and `worktree` data refresh in the background.

### C3 Contributors

- `StatusContributor` trait (`crates/radian-host/src/contrib.rs`): `id`, item id list, refresh period, `refresh(ctx) -> Vec<Segment>`. `Segment` is "text plus theme role" with no raw escape sequences. The theme stays the only color source.
- Each contributor runs its own periodic task on tokio. Results write into a shared contribution store, then send one dirty notice per U2. The UI draws it next frame. On refresh failure it keeps the last value and marks it stale (dimmed). Consecutive failures send only one notice. The UI never waits on contributors.
- No contributor replaces the header, the whole footer, or the input box.

### C4 Usage component

`usage` is a provider-related component. Data is one `UsageSnapshot { provider, windows: [{ label, remaining, resets_at }] }`. The component only accepts this struct. The data source is the provider descriptor optional field `usage`:

- `Endpoint`: polls one URL and parses (opencode-go `GET https://opencode.ai/zen/go/v1/usage`, key from `auth.json`; default 60-second refresh, configurable from 15 seconds to 10 minutes, can turn off; active and shown only when the current provider is opencode-go; stops on switch away, refreshes at once on switch back). The fetch reads the provider key pool's **current** key at each fetch (ADR 0026 D2), not a key resolved once at spawn; when the entry has several keys the snapshot's title is `Usage · <key name>` (`Usage` alone with one key). A key switch refreshes at the next period; other keys' windows are not fetched.
- `Headers`: reads limit headers from each model response (Anthropic `anthropic-ratelimit-*`, OpenAI-compatible `x-ratelimit-*`; ChatGPT `x-codex-*`). `ModelResponse.limits` carries back to the host. The `limits` field of `AgentEvent::ModelResponseReceived` forwards to the footer. Limit windows are live footer facts and never enter the journal.
- A provider whose descriptor lacks `usage` leaves the component empty and the row absent. `/usage` reads the same snapshot and prints all windows in full.
- Display content: **remaining** percent per time window (not used quota) plus reset countdown. `resetsAt` accepts ISO-8601 strings and epoch second or millisecond numbers. One window parse failure drops only that window, never the whole row. Keys never enter journal, logs, or notices. Only display values enter the contribution store.

### Out-of-process `command` component

`[tui.footer.items.<id>] command = "<executable or command line>"`, `period = <seconds>` (15-3600; `0` means run once at startup). stdin receives one JSON object (model, cwd, branch, context share, tokens, cost). stdout first line is display text, or one JSON line `{"text","style","symbol"}`. Rules: user-level configuration only; runs directly as the user, never in the sandbox; environment follows the MCP child `env_clear` allowlist plus `RADIAN_*`; 5-second timeout; output truncates to 1 line and 256 bytes; timeout or nonzero exit keeps the last value and marks it stale; output enters only display, never journal, model context, or herdr reports. This is not radian own network item (ADR 0013 unchanged).

### Spinner and state line

- **One definition everywhere**: thinking collapsed rows, tool-block running states, and the input-box top state line all use the same spinner and the same beat. Animation frames drive from U2 timers.
- **Default `pulse` (fading breath)**: one `⏺` (U+23FA) per **2.0 s** cycle. It interpolates **16** color steps between accent and one level weak by sine (125 ms each, slower at both ends). Only color changes; glyph, visibility, position, and width never change. Each step is 125 ms and runs only while the spinner is visible. It redraws one frame only when color truly changes; idle never refreshes. When both end colors are RGB, interpolation runs in RGB space. When either end is not RGB (16 or 256-color themes, `mono`), it falls back to two-color switch (500 ms). The `ascii` level `*` blinks by color the same way.
- Optional `tui.spinner = "arc"` (`◜◝◞◟`, 250 ms), `"quadrant"` (`▖▘▝▗`), `"braille"`.
- **Input-box top state line**: while a turn runs, the `lines`-style input-box top line becomes `── ◜  Working ──────…` (two spaces between spinner and text). Text follows state. Idle restores the normal line. `box` style puts the same content at the top-border language-label position.

### Brand, splash screen, and glyphs

- **Splash wordmark**: thin uppercase, 3 rows of box characters, primary level color (`text`). Right side aligns version, model and thinking level, and workspace in secondary and weak levels. Below come the collapsed startup information line and Tip line. When terminal width cannot fit, only one line `<brand> radian <version>` shows. `tui.banner = false` turns off the wordmark. `tui.tips` controls the Tip line (default on).
- **Brand glyph is configurable**: `[tui] brand = "<text>"`, any short text (at most 4 display widths; over is a configuration error). The footer `brand` item and the splash one-line fallback both use it. When unset, glyph defaults apply (`nerd` icon, `unicode` bold accent `R`, `ascii` `rad`). User-written glyphs skip coverage checks. The README states "a font without the glyph draws it from a fallback font".
- **`unicode` level uses only wide-coverage characters**: reference fonts are Menlo, SF Mono, JetBrains Mono, Cascadia Code, Fira Code, and Source Code Pro. Each built-in item unicode icon, spinner frame, and separator must exist in all six: `↑`/`↓`, `✓`, `×`, `›`, `…`. A missing one changes to a present one or to text only. `ascii` level falls back to pure ASCII. Check script is `scripts/check-glyph-coverage.py`; results go into `crates/radian-tui/README.md`.
- **Exceptions**: brand glyph is configurable and unchecked. Pulse `⏺` exists in none of the reference fonts and draws from a fallback font. `tui.spinner` can change it.

### Footer numbers and cost

- Data sources: cache tokens use Core usage `cache_read` and `cache_write`. `speed` and `elapsed` are TUI realtime clocks. `cost` computes from model unit prices. Prices come from the models.dev catalog. Without a price it hides. It never invents numbers.
- **Cost accumulates per call**: on each usage arrival it prices that call micro-dollars at the **then-route price** and adds into the projection. `/model` switch affects only later calls.
- **Cache prices only once at cache price**: full-price input uses `ModelUsage::uncached_input()`; cache reads and writes each price at their own price.
- **Price follows the effective route**: startup uses the startup-route price (resume restoring another model uses the price resolved from hot cache). Each `ConfigChanged` changes the price. Resolution is always offline.
- **`$0.00` never appears**: only when at least one count is above 0 and that count has a price does a fee arise. All-zero responses and priceless models produce no fee. The `cost` item hides when it accumulates no fee.
- **Compaction call usage also counts**: `AgentEvent::CompactionUsage` adds into the session total through the same path as `ModelResponseReceived.usage`, but it **never** reuses `ModelResponseReceived` (or the summary counts as the current-turn response in speed and elapsed). It sends per call.
- **Resume folds back**: `open_session` builds the projection, then calls `cost::restore_usage`. It folds back both `ModelUsageRecorded` and `CompactionUsageRecorded`, each priced at its then-effective route (the session-snapshot route prices calls before the first `ConfigChanged`). `TranscriptReplaced` never zeroes. A route without a resolvable price restores tokens only with no fee.
- **Incomplete marks `≥`**: when one call has a nonzero count without a price, the fee is the known-item sum marked incomplete and shows `≥$0.06`. When the total holds any incomplete call, the total carries `≥`.
- **Each call records its fee**: `ModelUsageRecorded` and `CompactionUsageRecorded` can carry an optional `fee`. Restore directly adds a recorded fee with no recompute. Old records without the field recompute per the current catalog and mark the total as estimated, showing `~$1.00`. When both estimated and incomplete apply, `~` wins.

## Constraints and Costs

- Footer is a component composition: the four preset outputs match the old version byte for byte except separators. The `command` component is the only public extension point. No in-process plugin API exists.
- `usage` data source depends on provider. A provider without a source leaves the row absent. It never invents.
- Cost estimates and incomplete marks are display facts. They never write back into journal history.

## Verification

`crates/radian-tui/tests/footer.rs` and `pty.rs` cover preset snapshots, per-field configuration, colors, compact numbers, narrow-terminal omit, bad-write fallback, `command` component (timeout and stale, two lines taking only the first, JSON style), and `jobs`. `radian-host` `contrib`, `usage_gate`, and `cost_resume` tests cover the contribution store, usage gating, pricing, and resume foldback. `spinner` unit tests cover breath frames and interpolation. `scripts/check-glyph-coverage.py` plus the README glyph table. Each of the two provider contracts adds "with headers and without headers" cases.
