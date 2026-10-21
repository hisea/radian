# dsh-TUI study: shareable TUI components and an orchestration view

- Date: 2026-10-16
- Worktree/branch: `<worktree>/dsh-tui-study` at branch `oc/dsh-tui-study`
  from `origin/main` (HEAD `e15576b`).
- Source read: `https://github.com/ccch1mneyyy/dsh-TUI`, cloned shallowly into
  `/private/tmp/dsh-tui-src-2026-10-16`, **pinned at commit
  `6e9f07d26ad412fceada8ff47fbe19882dd7ac7c`** (2026-10-15,
  `feat(plugin): hint the spec shapes at the argument position (#1549)`).
- Purpose: a study, not code. It answers the owner's two questions (shareable components;
  orchestration view), maps them onto radian, inventories dsh-TUI's non-orchestration UI
  components, and lists open questions. **It changes no code.**
- Status: read-only of an untrusted repository. dsh-TUI was read; its scripts, `npm install` and
  its code were **never run**.

## How to read this

- `path:line` is a fact read in the dsh-TUI tree at the pinned commit above, or in this radian
  worktree. A path with no leading `src/` or `crates/` is dsh-TUI-relative.
- `(ADR NNNN)` means the fact comes from a radian ADR, not from code.
- `(unverified)` marks a claim I could not check in the read I did. Claims about timing,
  concurrency and process lifetime are not relied on.
- dsh-TUI is ~1,781 `.ts`/`.tsx` files under `src/`. This study reads the UI-relevant subset
  listed in the appendix; it is not a line-by-line audit.

---

## 0. What dsh-TUI is

dsh-TUI is a large, terminal-only, React-based TUI written in TypeScript that renders through a
**checked-in fork of Ink** (its own reconciler, a port of Yoga layout, frame diffing, and
terminal-capability detection). It is the TUI recommendation of the "DSH" agent and ships as
both a standalone TUI and a plugin host (plugins register panels, status lines and scenes
in-process). It is single-column by default and adds a right side panel only above a width
threshold. It is **not** a Rust project and **not** a general component library: the design-system
folder is small and the screens are large.

Numbers that set the scale (read from the tree):

- `src/`: 1,781 `.ts`/`.tsx` files.
- `src/components/`: 15 subdirectories (`approvals`, `Chat`, `design-system`, `messages`,
  `questions`, `sessions`, `sidePanel` (+ 4 sub-dirs), `Spinner`, `trajectory`, `workspaces`) and
  ~170 component files.
- `src/screens/`: 8 scenes (`Chat`, `Launchpad`, `Onboarding`, `Settings`, `SessionSupervisor`,
  `SessionTree`, `StatusLine`, `TrajectoryScene`, `AgentTranscriptScene`).
- `src/ink/`: the renderer fork.

---

## 1. What dsh-TUI does — verified (`file:line`)

### 1.1 The renderer: a maintained Ink fork (`src/ink/`)

dsh-TUI does not use stock Ink. `src/ink/` is a heavily modified copy with:

- its own **reconciler and Yoga-based layout** port, a **frame diffing** pass, and throttled
  rendering (`(unverified)` exact symbol names; the directory and its `components/` file list
  were read, not each file).
- a **16 ms throttled `scheduleRender`** and a **blit / dirty-row** fast path (`(unverified)`).
- **DEC 2026 synchronized output**, **Kitty/Sixel graphics**, and **DECSET 1004 focus** support.
- an alternate-screen mode.

The practical consequence for this study: dsh-TUI can pay for per-frame diffing, mouse hover,
animations and graphics because it owns the renderer. **radian does not** (it renders through
`ratatui` and draws a full frame per `dirty`; ADR 0023 U2). Anything that depends on
sub-frame diffing, hover dwell, or blitting is therefore **not portable to radian as-is** and is
called out as such below.

### 1.2 The component model

- **Props + React state.** Components are function components with `React.ReactNode` return types;
  e.g. `export function SubagentCard({ subagent, focused, onClick, variant = 'default' }):
  React.ReactNode` (`src/components/SubagentCard.tsx:21`).
- **Theme by context.** A `ThemeProvider` publishes a theme; components read it with `useTheme()`
  and resolve theme keys to colors inside `ThemedBox`/`ThemedText`
  (`src/components/design-system/ThemeProvider.tsx:97`, `.../ThemedBox.tsx`, `.../ThemedText.tsx:12`).
  No component names a raw color.
- **Layout by flex, not by a grid.** `Box` flex props (direction, gap, grow, wrap) lay out
  children; `PageMargin`/`SurfaceEdges` add page insets and bleed edges
  (`src/components/PageMargin.tsx:54`, `src/components/SurfaceEdges.tsx:19`).
- **Focus by an index, not a focus tree.** Lists keep a `focusIndex` and window it; e.g.
  `Select` (`src/components/Select.tsx:19`) windows around the focused row, and `listWindow.ts`
  budgets visible rows by row height. Panels receive keys through a host runtime
  (`usePanelInput`, §1.4).
- **Mouse by handler props.** Clickable boxes take `onClick(event: ClickEvent)`; there is a hover
  tooltip system with a ~600 ms dwell (`src/components/Tooltip.tsx:75`). Both depend on the Ink
  fork's mouse/hover support.
- **Rendering/diffing is the renderer's job** (§1.1); components just return nodes.

### 1.3 The design system (`src/components/design-system/`)

Small and primitive-only — the reusable vocabulary is a dozen files:

| File:line | Primitive |
|---|---|
| `design-system/Pane.tsx:15` | a bordered/filled pane container |
| `design-system/Divider.tsx:80` | a themed rule (optional label) |
| `design-system/ListItem.tsx:57` | one list row (title, meta, selected state) |
| `design-system/StatusIcon.tsx:24` | a state glyph + color |
| `design-system/ProgressBar.tsx:13` | a filled-bar primitive |
| `design-system/Byline.tsx:15` | a dim byline / metadata line |
| `design-system/HintLine.tsx:10` | a dim key-hint line |
| `design-system/LoadingState.tsx:20` | a spinner + label block |
| `design-system/InputCaret.tsx:6` | a block/bar caret |
| `design-system/ThemedBox.tsx` / `ThemedText.tsx:12` | theme-key → color resolution |
| `design-system/ThemeProvider.tsx:97` | the theme context |

Everything else is a screen-level composition of these. This is the model radian should copy:
**a handful of pure primitives, composed by screens** — not a widget framework.

### 1.4 Panels: how a plugin or built-in contributes UI

The panel contract is one interface, `PanelDefinition`
(`src/components/sidePanel/types.ts:44`):

- `id` `:46`, `titleKey`/`title` `:48-49`, `icon` `:51`, `order` `:52`, `minColumns` `:54`,
  `source: 'builtin' | 'plugin'` `:56`, `mountPolicy` `:58`, `capabilities` `:59`,
  `component: React.ComponentType<PanelProps>` `:60`, and a `compact` variant `:64`.
- A **`PanelStore` singleton** holds registered panels; **`PanelHost`** mounts
  `enabled ∩ registered`, wrapping **each panel in its own `PanelErrorBoundary`** with a
  crash-disable policy (3 crashes / 60 s) so one bad panel cannot take the app down
  (`src/components/sidePanel/PanelHost.tsx`, `PanelErrorBoundary.tsx`).
- Layout is a split with a **draggable divider**; the panel only knows its own width, not the
  terminal width or its position on screen (`types.ts:4`, `SidePanelLayout.tsx`).
- Keyboard reaches a panel through a **runtime seam** (`registerInput` / `dispatchKey`,
  `types.ts:109-113`); unhandled keys return to the host.
- Panels show **badges** (level / unread) in a `PanelBar` (`src/components/sidePanel/PanelBar.tsx`).

**Plugin panels** register in-process through a capability-checked service
(`ctx.tuiPanels.register(descriptor)`, `src/dsh-adapter/panels.ts:308`). The host:

- namespaces the id to `plugin:<id>` automatically;
- **forces `mountPolicy: 'active'`** and grants **no capabilities**;
- gives no fullscreen button (fullscreen is a separate *Scene* seam, §1.6);
- rejects invalid registrations with a warn + `undefined`/`false`, never by throwing at the
  plugin (`panels.ts:296`).

A second service, `ctx.tuiStatus` (`src/dsh-adapter/status.ts:155`), lets a plugin `set(key,text)`
(scalar, sanitized and joined into one line) or `registerView` (`status.ts:196`) to contribute a
richer node. Status is rendered **above the prompt**, not in the footer (`(unverified)` exact
placement; the file was read, the mount site was not).

### 1.5 Task-completion notifications

`createTaskCompletion` (`src/dsh-adapter/channel/task-completion.ts:10`) fires on
`turn.end` with `reason.kind === 'completed'` (`:50`) and then **waits** for the dispatch to
settle and for status to be idle before notifying (`:55-64`); it suppresses a notice when the
session is working, is `disposed`, has a non-docked pending item, or has an active goal (`:63`).
The transport is `createTerminalNotifier` (`src/ink/terminal-notification.ts:31`), which picks a
protocol (`auto | osc9 | bel`, or OSC 777/99) and only writes when the terminal is a TTY and,
in the default `'unfocused'` condition, only when the window is **not** focused
(`terminal-notification.ts:37`, `getTerminalFocused`). OSC payloads are truncated to a byte budget
(2 KB) on grapheme boundaries.

### 1.6 The scene / screen seam

Screens are full-terminal "scenes" (`src/screens/*.tsx`), each a self-contained render of a
region of state: `Chat`, `Launchpad`, `Onboarding`, `Settings`, `SessionSupervisor`,
`SessionTree`, `StatusLine`, `TrajectoryScene`, `AgentTranscriptScene`. Plugins can contribute a
**scene** through a separate seam (`src/scenes.ts`, `PluginSceneBoundary.tsx`) — distinct from the
panel seam, and the only way to take the full screen.

### 1.7 What I could not verify

- Exact renderer symbol names and the diffing/blit internals (the directory was listed; individual
  files were not all read).
- The exact mount point of `tuiStatus` contributions (above vs inside the composer cluster).
- Whether multiple terminal-notification protocols are ever selected at runtime, or whether
  `auto` is the only path exercised.
- Anything about dsh-TUI's own tests or CI (not read).

---

## 2. The orchestration view in dsh-TUI — verified (`file:line`)

dsh-TUI shows several agents/tasks/sessions at once through **four** pieces:

### 2.1 The roster: `SubagentDashboard` + `SubagentCard`

- `SubagentDashboard` (`src/components/SubagentDashboard.tsx:63`) is the Agents panel: a
  **counts row** — running / completed / failed / nested
  (`:176-191`) — then the roster, then an optional **Peers** section (`:244-260`).
- It keeps **top-level children first** when the roster contains nested agents (`:79`) and handles
  roster navigation as panel keys (`:127`).
- `SubagentCard` (`src/components/SubagentCard.tsx:21`) is one row per agent:
  - a **status glyph + color** from `status` (running/starting/unknown/failed/cancelled),
    with an ASCII fallback under `minimalUi` (`:39-45`);
  - **elapsed** — live-computed while running, else the reported duration (`:26-31`);
  - **tools count** — `reportedToolUses ?? toolCalls.length` (`:30`);
  - **total tokens** — reported total or input+output (`:32`);
  - a **meta line** that only appears at ≥ 34 columns and appends `N tools` (`:57`).

The state this renders comes from `SubagentActivityStore`
(`src/dsh-adapter/subagents.ts:31`), a projector over channel events:
`onSpawned` `:67`, `recordTool` `:373`, `patch` `:406`, `costSnapshot` `:500`,
`finish` `:515`. It carries `status`, `toolCalls[]` (name, status, startedAt, argsPreview),
`tokens` (input/output/total), per-model **cost buckets**, and output lines.

### 2.2 The detail: `AgentTranscriptScene` (read-only, with a workbench rail)

`AgentTranscriptScene` (`src/screens/AgentTranscriptScene.tsx:105`) is the main-screen view of one
agent. Its doc is explicit that it is **read-only**: it is "a scene over the mounted parent Chat,
not a second Channel", it "mutates the roster, the Channel or the parent's queues" never (`:73`),
and it does not own state the parent needs. A composer appears only when the adapter supplies a
`control` seam (`:85`).

- A **workbench rail** appears only on wide terminals, and only with a roster (`:18`, `:116`).
- It shows **metadata**, a **tool list**, a **parent line**, and a **sibling switcher**
  (`AgentSwitchRow` `:40`, `AgentParentRow` `:58`).
- The parent fact is **never guessed**: "a named parent, the main loop (depth 1), or unknown"
  (`:56-62`); a parent the roster lost is named but not switchable.
- Switching re-renders **in place** (`onSwitchAgent`, `:90-93`); without it the rail is read-only.
- A second, smaller detail view exists (`SubagentDetailScene`, `:202`) for the panel form.

### 2.3 Multi-session: `SessionSupervisor`

`SessionSupervisor` (`src/screens/SessionSupervisor.tsx:65`) is the "several sessions at once"
screen: a **left rail** of workspaces/sessions (a durable ledger — add, rename, delete) and a
**session list** where each row shows the session's **live state** and **occupancy** (`:43-61`).
Occupancy is "proven by liveness rather than by a lease" (`:58`), so a parked session whose
process exited is shown as live-but-idle until a new process takes it. **Source tabs** separate
this terminal's sessions from "foreign" sources (`:172-177`), folded into a `+N` menu.

### 2.4 What it does not do

- The roster and cards are **polled from projected state**, not from a push subscription per card;
  the projector updates on channel events.
- There is **no per-agent board/kanban/timeline** in the read set; the trajectory screen
  (`src/screens/TrajectoryScene.tsx`, `src/components/trajectory/*`) is a different, single-session
  visualization and was not studied for orchestration.
- **Cost** is collected per model (`costSnapshot` `:500`) but the card shows **tokens**, not cost.

---

## 3. Proposal: a shareable component layer in radian

### 3.1 The constraints that shape it

- **ADR 0023** — the composition boundary: the TUI is a **pure state machine**
  (`App::update → UpdateResult`), panels are **compile-time** (no dynamic third-party UI plugin),
  and the default is **single-column**. A shared layer must therefore be **pure Rust functions over
  state**, with no plugin registry and no per-component runtime.
- **ADR 0035 V1** — "completeness over configurability": prefer a small set of primitives that
  cover radian's actual screens completely, over a configurable widget kit.
- **ADR 0035/2** already introduced `panel.rs` and `select_list.rs` as the first shared
  primitives, modeled on omp. This proposal **continues that line**, it does not restart it.
- **ADR 0028** — the transcript already has a display model (entry → blocks → lines), so
  transcript-side components are **line producers**, not widgets.
- **ADR 0030 slice 3** — `App` is being split into global state + per-session view right now on
  another branch. Any new component must take its data as arguments (a `&SessionView`-like view),
  not reach into `App`.

### 3.2 The component list (12)

`new` = a new primitive; `extend` = grow an existing module. All are **pure**: `(data, theme,
width) → Vec<Line<'static>>` (or a small struct with one such method). None does I/O, none holds
state across frames.

| # | Component | Kind | What it is | dsh-TUI model |
|---|---|---|---|---|
| C1 | `chrome::Panel` | extend `panel.rs` | rounded box, title in the top border, optional **right title**, body `Line`s, `├──┤` dividers, key hints in the bottom border | `design-system/Pane.tsx:15`, omp `OverlayPanel` |
| C2 | `chrome::SelectList` | extend `select_list.rs` | `❯ ` cursor, two columns past 40 cols, scroll window follows selection, one-column scrollbar; add a `current` marker and **group dividers** | `Select.tsx:19`, `listWindow.ts` |
| C3 | `chrome::StatusIcon` | new | the single `state → (glyph, role)` map (pending / running / ok / error / cancelled / stalled), with an ASCII fallback | `design-system/StatusIcon.tsx:24`, `SubagentCard.tsx:39-45` |
| C4 | `chrome::Badge` | new | a small right-aligned label with a level (info/warn/error/unread) and an optional count, for list rows and the panel bar | `sidePanel/PanelBar.tsx` |
| C5 | `chrome::ProgressBar` | extend `footer.rs:1586` | the context-bar cell math promoted to a reusable bar (fill/empty, optional label) | `design-system/ProgressBar.tsx:13`, `ContextBarView.tsx:40` |
| C6 | `chrome::KeyValueRow` | new | a dim `key  value` line, values right-padded; the metadata vocabulary for panels and detail views | `design-system/Byline.tsx:15`, `messages/MessageMetadata.tsx` |
| C7 | `chrome::Divider` | new | a themed rule with an optional label; one shape used by panels, the detail view and the roster | `design-system/Divider.tsx:80` |
| C8 | `chrome::HintLine` | extend panel hints | `&[Hint] → Line`, so panels, the approval box and the pickers share one hint renderer | `design-system/HintLine.tsx:10` |
| C9 | `chrome::StatRow` | new | a row of `label value` cells that **drops cells by width** (state · elapsed · tokens · cost · tool), the roster-row primitive | `SubagentCard.tsx:32,57` |
| C10 | `chrome::Notice` | extend `render.rs:1325` | a one-line transient note with a level and a TTL, drawn above the composer (already partly there) | `dsh-adapter/status.ts:155` |
| C11 | `chrome::ToolCard` | extend `tools.rs` | the shared card frame (head + body + state band) already used by the transcript, plus a **live body line** while running | `messages/AssistantToolUseMessage.tsx` |
| C12 | `chrome::ScrollView` | new (extract) | `(offset, viewport, follow_tail)` over laid-out lines, so the transcript's scroll and a panel's scroll are the same code | `MessageList.tsx:139`, `ScrollBox` (`unverified` symbol) |

**Composition.** Screens keep composing, as today: a screen builds a `Vec<Line>` by calling these
functions and nesting their output. There is **no component tree, no focus tree, no registry** —
composition is function calls. Panels (ADR 0023 U8 / ADR 0030 slice 11) are structs that call C1
and C12.

**Theming.** Every component takes `&Theme` and names only **roles** (ADR 0029). No component
names a `Color`. A `chrome` module gets a small set of layout constants (one-cell padding, the 34-
and 40-column thresholds already in `select_list.rs`) as `const`s, not as config keys.

**Interface shape.** Keep it boring: free functions, or thin structs with a `lines(&self, theme,
width) -> Vec<Line<'static>>`. This matches `panel.rs` and `select_list.rs` and keeps every
component unit-testable against `TestBackend` with no terminal.

### 3.3 What to copy, and what not to

**Copy:**

- The **primitives-only design system** (§1.3): a dozen pure components, screens compose them.
- The **panel contract's data, not its machinery**: id/title/icon/order/min-width/source are good
  fields for *radian's own* compile-time panels (ADR 0023 U8); drop the store, the runtime seam and
  the error boundary (radian panels are not third-party code).
- The **roster-card information set**: status glyph, elapsed, tool count, tokens, and the
  width-gated meta line (C9). This is exactly what an orchestration list needs (C3/C4/C9).
- The **"never guess the parent" rule** (`AgentTranscriptScene.tsx:56-62`) — a fact is named,
  the main loop, or unknown; never inferred.
- The **notification gating logic**: only when idle, not working, no pending item, no active goal,
  and (by default) only when the window is unfocused (`task-completion.ts:55-64`).

**Do not copy:**

- The **Ink fork** (§1.1): radian draws through `ratatui` per `dirty` frame. Hover dwell,
  blitting, per-frame diffing and graphics are not available and should not be emulated.
- The **plugin registry / capability system / error boundaries** (§1.4): ADR 0023 forbids dynamic
  third-party UI. A store + runtime seam would be dead weight.
- The **React-style state ownership** in cards/screens: radian's `App` is pure (ADR 0023), and
  slice 3 is splitting it. Components take arguments.
- The **theme-key-to-color resolver in the component layer**: radian already resolves roles in
  `theme.rs`; a second resolver would be a second source of truth.
- **Two-pane split diff** (`SplitDiffView.tsx`) and **hover tooltips** (`Tooltip.tsx:75`) — both
  need the fork; radian has a unified diff (`tools.rs:317-320`) and that is enough for V1.

---

## 4. Proposal: the orchestration views

This section maps each piece onto ADR 0030's slices and says what it needs from the slice-2
**observer**. The observer (slice 2) is the read-only view over **committed journal facts**; it is
the only new source the TUI needs.

### 4.1 Agents/Tasks panel (the roster)

A list panel, one row per task agent:

`<status glyph> <name/id>  <current tool>  <elapsed>  <tokens/cost>  <last write>`

- **state** — from the observer (`task_status`: starting/running/done/failed/cancelled; ADR 0030 R5).
- **current tool** — the last tool-call fact for that session (`ToolCallStart` without a matching
  completion, or the latest one).
- **elapsed** — locally timed in the TUI (`timing.rs`, no wall clock in `update`), reset on the
  task's start fact.
- **cost** — tokens always; cost only when the usage fact carries a price (ADR 0007). dsh-TUI
  collects cost but shows tokens (`subagents.ts:500`); radian should show cost **when known**, and
  a dim `—` otherwise.
- **last write** — the most recent file the task wrote, from the git/journal facts (ADR 0030 R8
  already puts changed files into `list_tasks`); falls back to the last tool name.
- **stalled marker** — a task with no new committed fact for a threshold (proposed 60 s) gets the
  `stalled` role on its status glyph. ADR 0030 R5 makes a stall observable via `task_status`; the
  observer must expose `last_activity_at` for the TUI to draw it.
- A **counts row** on top (running / done / failed), copied from `SubagentDashboard.tsx:176-191`.

Rows are `chrome::SelectList` + `chrome::StatRow` + `chrome::StatusIcon`/`Badge`; the whole panel is
`chrome::Panel` + `chrome::ScrollView`.

### 4.2 Task detail view (read-only, through the observer)

Selecting a row opens a **read-only detail**: metadata (C6 rows: worktree, branch, started,
elapsed, tokens, cost, changed files), the **current tool**, and the **task's transcript** as
committed facts — rendered with `chrome::ToolCard` and the existing transcript line producers.

- It is **read-only by construction**: it reads the observer, which reads committed facts. It never
  sends an input to a task agent (matching "task agents never get an input box"; ADR 0030 R9 and
  the ADR 0035 boundary).
- It reuses dsh-TUI's **workbench-rail idea** (`AgentTranscriptScene.tsx:116`): metadata + tools +
  parent/lead, but with radian's rule that the parent is **never guessed**.
- It needs from the observer only what the roster needs, plus the per-session **tool-call list** and
  **output/fact lines**. That is slice 2's `JournalTail` surface; if slice 2 exposes only state,
  the detail view is deferred (see Q4 in §6).

### 4.3 Switching

- **In-panel switching** is just "move the selection" — no separate mechanism.
- **Full-screen switching** (the task detail taking the screen, and back) belongs with **slice 11**
  ("TUI session switching and `/tasks`"; ADR 0030). v0 should open the detail as a
  **full-width overlay panel** (fits ADR 0023's single-column default) and reserve a right side
  panel for a later revision behind a width threshold.
- Switching the **viewed session** must not switch the **active/leaded session**: the lead stays
  authoritative; the view is a lens. This is the same distinction `AgentTranscriptScene` draws
  (`:90-93`, switch in place, read-only otherwise).

### 4.4 Notifications

- In-TUI **notice** (C10) when a task finishes while the user is looking at another view.
- **Terminal notifications** are optional and **off by default**: when a task reaches a terminal
  state and the window is unfocused, emit OSC 777/99/BEL through `terminal.rs`, reusing the gating
  from `task-completion.ts:55-64` and the semantics of ADR 0037's background-completion notice.
  Gate on a config key; default `off` is consistent with ADR 0013's spirit (no default outward
  side effects) without adding telemetry.
- NYI in v0: no per-slice notification, only per-task terminal state.

### 4.5 Fit to ADR 0030's slices

| Piece | Slice | Needs from the observer |
|---|---|---|
| Roster (state, elapsed, current tool) | **slice 2** (observer) + **slice 11** (TUI) | `task_status` per task; current tool fact; `last_activity_at` |
| Last-write column | slice 2 + R8 (`list_tasks` files) | changed files / last write fact |
| Counts row, badges | slice 11 | roster facts only |
| Detail view (read-only transcript) | slice 2 + slice 11 | tool-call list + fact/output lines per session |
| In-place switching | slice 11 | mapping session id → observer handle |
| Full-screen detail | slice 11 | none beyond the above |
| Terminal notification | slice 11 (+ ADR 0037 semantics) | terminal state + idle gating |
| Cost column | slice 2 (usage facts) | priced usage, else omitted |

**What the observer must expose** (the load-bearing requirement): for each task session —
`status`, `started_at`, `last_activity_at`, `current_tool`, `tool_calls[]`, `tokens`, optional
`cost`, `changed_files[]`, and a tail of committed fact lines. Without `last_activity_at` there is
no honest stalled marker; without the fact tail there is no detail view.

---

## 5. Component inventory: dsh-TUI UI components that are not about orchestration

Categories requested by the owner. "radian equivalent" names a radian file/module or "none";
"recommend" is yes / no / later with a one-line reason. dsh-TUI paths are repo-relative; `src/`
prefix omitted.

### 5.1 Input / composer

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `components/PromptInput.tsx` (editor core) | the multiline draft: caret, selection, paste cleanup, `[Image #N]` tokens | `composer.rs` (text + cursor + editing keys) | yes — radian's composer is the equivalent; take the *paste/image-token* ideas only if images land |
| `components/PromptEditor.tsx:28` | full-screen draft editor as a root-level layer over the whole Chat tree | none (radian has no full-screen draft) | later — nice for long prompts; needs a root layer radian does not have |
| `components/composerImageTokens.ts` | atomic geometry for `[Image #N]` tokens | none | later — only if image input ships |
| `components/design-system/InputCaret.tsx:6` | block/bar caret | `render.rs` caret drawing | yes — same idea, radian already draws it |
| `components/AgentMessageComposer.tsx:87` | the composer used to message *another agent* | none | later — becomes relevant only with agent-to-agent messaging (ADR 0030 R9 routes via lead) |

### 5.2 Transcript blocks

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `components/MessageList.tsx` | the transcript list: fixed-height spacers + viewport culling | `render.rs` + `blocks.rs` (virtualization budget, ADR 0028 R7) | yes — same model; radian has the equivalent |
| `components/messages/AssistantTextMessage.tsx` | an assistant text block with turn margins/selection | `render.rs` body/`blocks.rs` | yes — already covered |
| `components/messages/UserPromptMessage.tsx` | a user block with turn margin + selection | `blocks.rs` user block + `render.rs` band | yes — already covered |
| `components/messages/AssistantThinkingMessage.tsx` | a collapsible reasoning block with a thinking spinner | `render.rs` reasoning fold (ADR 0014/0028 R3) | yes — covered |
| `components/messages/AssistantToolUseMessage.tsx` | a tool-call block | `tools.rs` (per-tool card, ADR 0028 R4) | yes — covered |
| `components/messages/TranscriptLeaves.tsx` | agent↔agent message leaves | none | later — only with agent messaging |
| `components/messages/TranscriptImages.tsx` | inline images in the transcript | none (terminal.rs has no graphics) | no — needs Kitty/Sixel; radian renders text |
| `components/messages/MessageMetadata.tsx` | metadata line under a message | partial (`tools.rs` head, `footer.rs`) | later — fold into C6 when a detail view needs it |
| `components/messages/liveOutputLines.ts` | live tool output as transcript lines | `tools.rs` live card body | yes — take the live-line idea into C11 |
| `components/messages/subagentTranscript.ts` | renders a subagent's transcript | none | later — belongs to §4.2 detail view |

### 5.3 Tool and diff views

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `components/messages/AssistantToolUseMessage.tsx` | tool-call header + body + fold | `tools.rs` | yes — covered |
| `components/ToolUseLoader.tsx` | a spinner row while a tool runs | `spinner.rs` + `tools.rs` | yes — covered |
| `components/SplitDiffView.tsx` | **two-pane** diff | none (unified only: `tools.rs:317`) | no — needs the fork/side-by-side width; unified is enough for V1 |
| `components/diffPatch.ts` | parses a unified patch into hunks + `(+N -M)` | `tools.rs:505-516` (`git_diff` counts) | yes — radian already parses the same stat |
| `components/SyntaxText.tsx:20` | syntax-highlighted text span | `highlight.rs` (syntect, ADR 0028 R2) | yes — covered |

### 5.4 Markdown / code

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `components/Markdown.tsx` (doc) | the markdown renderer | `markdown.rs` (pulldown-cmark, ADR 0028 R2) | yes — covered |
| `components/StreamingMarkdown.tsx:203` | renders streaming markdown in **sealed groups** of completed blocks so only the tail re-renders | `render.rs` (whole-entry re-layout) | later — radian's frame budget absorbs this today; revisit if streaming gets expensive |
| `components/MarkdownTable.tsx:158` | table layout with column-width + row-line caps | `markdown.rs` table handling (ADR 0028 R2) | yes — radian lays out tables; keep radian's rules |
| `components/CodeBlockFrame.tsx:52` | a fenced code block in a light `┌─ lang ─` frame | `markdown.rs`/`highlight.rs` code frames | yes — covered (ADR 0035 settled the frame) |
| `components/MathBlock.tsx:46`, `InlineMathParagraph.tsx:41`, `mathPreview.ts` | LaTeX math block/inline rendering | none | no — out of scope for V1 ("completeness over configurability") |
| `components/MermaidDiagram.tsx:63` | renders mermaid in the terminal | ADR 0028 R8 (port codex-mermaid; planned) | later — already decided in ADR 0028 R8, not this study |

### 5.5 Dialogs and pickers

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `components/Select.tsx:19` + `listWindow.ts` | the shared selectable list (window follows focus) | `select_list.rs` | yes — radian has the equivalent; take the `current`/group ideas into C2 |
| `components/ModelPicker.tsx:26` | `/model` picker | `select_list.rs` driven from `app.rs` (`/model`) | yes — covered |
| `components/ThemePicker.tsx:94`, `ThemePreviewPane.tsx:36` | theme picker + live preview pane | `/theme` via `select_list.rs`; no preview pane | later — a preview pane is a nice add once panels exist |
| `components/ColorPicker.tsx:16` | arbitrary color picker | none | no — themes are files (ADR 0029), not an in-TUI color editor |
| `components/PermissionsPicker.tsx:17`, `ModePicker.tsx:17`, `PlanPicker.tsx:14`, `ActivityPicker.tsx:16`, `PresetPicker.tsx:22`, `SkillsPicker.tsx:41`, `LangPicker.tsx:12`, `MigratePicker.tsx:20`, `ChannelPicker.tsx:35`, `KernelPicker.tsx:26` | assorted single-purpose pickers | `select_list.rs` + `picker.rs` cover the shape; commands exist for mode/theme/skills (ADR 0021/0026) | later — add per picker only when the corresponding radian command needs a list (e.g. `/skills`, `/mode`) |
| `components/questions/AskUserQuestionPanel.tsx:149` | the ask-user-question modal (progress header, chips, focus pointer, multi-select) | `question.rs` (ADR 0024) | yes — covered; compare the header/chip treatment |
| `components/questions/PlanReviewPanel.tsx:74` | plan review modal | none | later — radian has no plan-review step |
| `components/questions/QuestionMinimizedBar.tsx:37` | a minimized question bar | none | later — only if modals can be minimized |
| `components/approvals/ApprovalPanel.tsx:104` | the approval modal (permission divider, options, feedback) | approval modal in `render.rs`/`app.rs` (ADR 0012) | yes — covered; take the "reject with reason" idea if feedback lands |
| `components/HistorySearchDialog.tsx:19` | reverse history search dialog | `find.rs` is in-transcript; no prompt-history search | later — useful, small |
| `components/HelpMenu.tsx:19` | a help/keybinding menu | none (keymap in `keys.rs`, README table) | later — a `/help` overlay would be cheap once `chrome::Panel` exists |
| `components/RewindPicker.tsx:23` | rewind/fork-to-message picker | none (session fork exists: ADR 0022) | later — pairs with fork/rewind if the owner wants it |
| `components/ExtensionDialog.tsx:54` | a plugin extension dialog | none | no — no dynamic plugin UI (ADR 0023) |
| `components/SdkInstallWizard.tsx`, `PluginInstallPane.tsx`, `PluginManagePane.tsx` | plugin install/manage flows | none | no — no plugin market (ADR 0023) |
| `components/ImagePreviewOverlay.tsx:66` | image preview overlay | none | no — no graphics |

### 5.6 Menus / suggestions

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `components/CommandSuggestions.tsx:34` | slash-command completion overlay | `command.rs` + `select_list.rs` completion | yes — covered |
| `components/FileSuggestions.tsx:93` | `@` file completion overlay | `mention.rs` + `select_list.rs` | yes — covered |
| `components/SearchBox.tsx:80` | a reusable search input | `find.rs` has the matcher; no shared box | later — extract a search box with `chrome::Panel` |
| `components/SuggestionCard.tsx:28` | a "try this" suggestion card | none | later — small nicety |
| `components/WorkspacePicker.tsx:12`, `WorkspaceMenuPicker.tsx`, `WorkspaceFlowPicker.tsx` | workspace pickers/menus | `workspace.rs` + `picker.rs` | later — radian's workspace is cwd-based; not needed for V1 |
| `components/SkillsPicker.tsx:41` | skills picker | commands exist (`/skills`); list shape via `select_list.rs` | later — add when `/skills` needs a picker |

### 5.7 Status bar / footer

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `screens/StatusLine.tsx:176` | the footer: context bar, left group (model·tokens·think·cache·tps), right group (git·cwd·title·id), mode/hint line | `footer.rs` (rows of left/right items, presets, ADR 0027 C2) | yes — radian's footer is the direct analogue; take the **segment click targets** only if mouse lands |
| `screens/StatusMetrics.ts` | the metric formatting for the footer | `timing.rs`, `footer.rs` item formatters | yes — covered |
| `components/ContextBarView.tsx:40` | the segmented context bar + readout | `footer.rs:1586` context bar | yes — covered; promote to C5 |
| `components/TurnUsageRow.tsx:15` | the per-turn usage ledger line (in/out/cache/duration/model/retries) | `footer.rs` usage items + `render.rs` usage | later — radian shows usage in footer/reports; a transcript row is optional |
| `components/CompactionStatusRow.tsx:44` | a compaction status row | `command.rs` compaction notes (ADR 0018) | later — a row vs a notice |
| `components/AutoRecapRow.tsx:17`, `RecapPanel.tsx:16` | an auto-recap row/panel | none | no — no auto-recap in radian |
| `components/BalanceReportRow.tsx:17` | a provider balance row | none (cost in footer/reports) | no — provider-specific |
| `components/TurnInterruptedRow.tsx:8` | marks an interrupted turn | `render.rs` interrupted marker (ADR 0003) | yes — covered |
| `components/WorkingSpinner.tsx:31` | phase-aware working line (verbs by stage) | `spinner.rs` (ADR 0027 revs 2/9/10) | yes — covered; radian's spinner already varies by time |
| `components/ActivityLine.tsx:18` | context-pressure % (amber ≥80, red ≥95) with the working line | `footer.rs` context % (thresholds) | later — radian shows %; the amber/red role split is a small add |
| `components/Spinner/SpinnerGlyph.tsx:26`, `SpinnerAnimationRow.tsx:65`, `GlimmerMessage.tsx:27` | spinner glyph, animation row, glimmer text | `spinner.rs` | yes — covered; the glimmer is fork-dependent |
| `components/Spinner/useStalledAnimation.ts` | a stalled-animation hook | `timing.rs` (no animation) | yes — the *concept* (a stalled marker) is reused in §4.1 |

### 5.8 Notifications / toasts

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `ink/terminal-notification.ts:31` | OSC 777/99/BEL notifier, truncates on grapheme boundary, default `unfocused` | none | later — propose in §4.4, off by default |
| `dsh-adapter/channel/task-completion.ts:10` | fires a completion notice only when idle, not working, no pending, no active goal | none (ADR 0037 background-completion notice is the nearest) | yes — take the **gating logic** |
| `components/ExtensionDialog.tsx:54` (toast-like) | a transient decision dialog | `render.rs:1325` transient note | yes — radian has the transient-note slot |
| `components/StarPrompt.tsx:73`, `TipsPanel.tsx:18`, `SuggestionCard.tsx:28` | product nags/tips | `startup.rs` one Tip | later — radian already shows one Tip at startup |

### 5.9 Scrolling and selection

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `components/MessageList.tsx` | transcript scroll (fixed-height spacers + culling) | `render.rs` scroll + virtualization (ADR 0028 R7) | yes — covered |
| `components/ScrollbarGutter.tsx:59` | a 2-column scrollbar gutter with a hover position chip | none (the transcript has no scrollbar; `select_list.rs:156` has one for lists) | later — a transcript scrollbar is a real gap; take the *thumb* idea, not the hover chip |
| `components/TimelineRail.tsx:60` | a timeline rail with turn ticks and chevrons | none | later — could help jump between turns; not V1 |
| `components/SurfaceEdges.tsx:19` | bleed/fade at scroll edges | none | no — cosmetic, fork-friendly |
| `components/OverlayAbove.tsx:27` + `overlayBudget.ts` | upward-anchored overlays with a clamped height budget (so the focus row is never clipped) | panel/picker overlays in `render.rs`/`picker.rs`; budget logic is ad-hoc | yes — the **budget invariant** (clamp overlay height to the space above the anchor) is worth copying as a bug-class fix |

### 5.10 Mouse

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `components/Tooltip.tsx:75` | hover tooltips with a ~600 ms dwell | none (grep found no tooltip) | no — needs the fork's hover; radian has no hover |
| click targets in `screens/StatusLine.tsx:127` | footer segments open pickers on click | `select.rs` handles mouse for selection only; no click targets | later — a click-to-open-model picker is a small win once mouse selection is stable |
| mouse selection | cell → offset selection | `select.rs` (two anchors, cell → byte offset) | yes — radian's model is the analogue and is more robust (anchors, not screen positions) |
| `components/ScrollbarGutter.tsx` hover chip | hovered scroll position chip | none | no — hover-dependent |

### 5.11 Themes

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `design-system/ThemeProvider.tsx:97` | theme context + env override | `theme.rs` (Theme + roles; ADR 0029) | yes — covered; radian resolves roles in one place |
| `design-system/ThemedBox.tsx` / `ThemedText.tsx:12` | theme-key → color | `theme.rs` + `render.rs` (roles only) | yes — covered |
| `customTheme.ts`, `themeCatalog.ts`, `themePrefs.ts`, `ThemePreviewPane.tsx:36` | theme files, catalog, prefs, preview | `theme/file.rs` (theme files, ADR 0029) | yes — mostly covered; the **preview pane** is the only new part |

### 5.12 Animations

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `components/Whale.tsx:136` | the pixel whale, animated by timed frames (layers, idle poses) | `spinner.rs` (time-driven, 12-step pulse; ADR 0027) | no — radian is deliberate about idle = zero redraw (ADR 0023 U2); a whale fights that |
| `components/WhaleGirl.tsx`, `ClaudeGirl.tsx`, `whaleFrames.ts`, `whaleLayers.ts`, `whaleIdle.ts`, `claudeGirlSprite.ts` | mascot sprites/frames | none | no — same reason, plus no graphics |
| `components/shimmer.ts`, `components/smoothReveal.ts` | text shimmer / smooth reveal | none | no — needs sub-frame timing |
| `components/Spinner/SpinnerAnimationRow.tsx:65`, `ActivityLine.tsx` | spinner + activity animation | `spinner.rs` | yes — covered by the time-driven spinner |
| `components/splashArtRows.ts`, `splashFonts.ts`, `splashEggs.ts`, `bigfont.ts`, `LogoV2.tsx`, `maidPortrait.tsx` | big-font splash art / easter eggs | `startup.rs` wordmark (ADR 0027) | later — radian's wordmark is settled; not V1 |

### 5.13 Layout and resizing

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `components/PageMargin.tsx:54`, `tuiDisplayPrefs.ts` | page insets, user-adjustable margin | `render.rs` fixed padding | later — a margin pref is cheap; not V1 |
| `components/SurfaceEdges.tsx:19` | bleed edges | none | no — cosmetic |
| `components/sidePanel/SidePanelLayout.tsx` | split layout with a draggable divider | none (single column; ADR 0023) | later — the §4 orchestration panel is the reason to add one |
| `sidePanel/dimensions.ts`, `useSidePanel.ts`, `types.ts:30-41` | width math + "the panel knows only its own width" contract | none | yes — copy the **contract** (a panel gets its width, not the terminal's) for radian's future panels |
| `components/OverlayAbove.tsx:27`, `overlayBudget.ts` | overlay height budget vs available rows | overlay sizing in `picker.rs`/`render.rs` | yes — see §5.9; the budget is a bug-class fix |
| `ink/components/TerminalSizeContext` | terminal size context | `events.rs`/`Resize` → `App` | yes — covered |

### 5.14 Design-system primitives

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `design-system/Pane.tsx:15` | bordered/filled pane | `panel.rs` | yes — extend (C1) |
| `design-system/Divider.tsx:80` | themed rule | none | yes — add (C7) |
| `design-system/ListItem.tsx:57` | list row | `select_list.rs` `Item` | yes — extend (C2) |
| `design-system/StatusIcon.tsx:24` | state glyph + color | none (state glyphs live in `tools.rs`/`spinner.rs`) | yes — add (C3) |
| `design-system/ProgressBar.tsx:13` | bar | `footer.rs:1586` | yes — extend (C5) |
| `design-system/Byline.tsx:15` | dim byline | none | yes — add (C6) |
| `design-system/HintLine.tsx:10` | dim hint line | `panel.rs` hints | yes — extend (C8) |
| `design-system/LoadingState.tsx:20` | spinner + label | `spinner.rs` | yes — covered |
| `design-system/InputCaret.tsx:6` | caret | `render.rs` | yes — covered |

### 5.15 Screens and other

| dsh-TUI component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `screens/Chat.tsx` | the main chat screen (composer + transcript + overlays + status) | `app.rs` + `render.rs` | yes — the analogue; composition model differs (see §3) |
| `screens/Launchpad.tsx` | a home/launch screen | `startup.rs` + picker | later — radian has a startup block; a full launchpad is scope |
| `screens/Onboarding.tsx` | first-run onboarding | `login.rs` (ADR 0026) | later — a wizard exists for auth; product onboarding is scope |
| `screens/Settings.tsx` + `settings/` | an in-TUI settings screen | none (config is file-driven, ADR 0008) | no — radian's settings are files/commands, deliberately |
| `screens/SessionTree.tsx` | a session tree | `picker.rs` (flat) | later — relevant only if sessions become hierarchical |
| `screens/TrajectoryScene.tsx`, `components/trajectory/*` | a single-session trajectory visualization | none | no — out of scope for this study |
| `components/GoalTodoPanel.tsx:50` | goal + todo panel | none (ADR 0030 has goals at the channel level) | later — revisit with orchestration goals |
| `components/JobsPanel.tsx:31` | background jobs panel | ADR 0037 background commands | later — pairs with background commands, not orchestration |
| `components/ThinkingToggle.tsx:12` | a thinking on/off toggle | `/thinking` command (`select_list.rs`) | yes — covered |
| `components/BtwPanel.tsx:18`, `sidePanel/btw/*` | a side "by the way" chat thread | none | no — product-specific |
| `components/sidePanel/companion/*` | a mascot companion panel | none | no — product-specific |

---

## 6. Open questions for the owner (with a recommendation each)

**Q1. How far to formalize the shared component layer?**
Build a `radian-tui::chrome` module holding C1–C12 as pure functions/structs, or keep adding
primitives ad hoc next to their screens?
**Recommend:** the `chrome` module. It continues ADR 0035 revs 1–2 (panel/select_list) instead of
starting over, and it is what makes the orchestration view cheap to build. No trait, no registry —
ADR 0023 forbids dynamic UI.

**Q2. Do we add a design-token layer (spacing, widths, roles) or keep constants next to use?**
dsh-TUI has theme keys plus scattered width constants.
**Recommend:** a tiny constants module only (one-cell padding, the 34- and 40-column thresholds
already in `select_list.rs`, the state glyphs in C3). No configurable token system — ADR 0035 V1.

**Q3. What shape is the Agents/Tasks panel for V1?**
A right side panel (new column), or a full-width overlay panel opened by `/tasks` (and a footer
item), with the side panel deferred?
**Recommend:** the **overlay panel first**. ADR 0023 defaults to single-column, and slice 11 already
plans `/tasks`; the side panel is a later revision behind a width threshold.

**Q4. What does the task detail view require from the slice-2 observer?**
Only per-task state, or also the per-session fact/output tail that a transcript needs?
**Recommend:** require **state + current tool + `last_activity_at` + changed files** in slice 2, and
treat the **fact tail as a follow-up** — the roster is useful without it, the detail view is not.
Do not block slice 2 on the tail.

**Q5. What is the stalled marker's threshold and meaning?**
"No new committed fact for N seconds" (needs `last_activity_at`), or "no tool progress"?
**Recommend:** no new committed fact for **60 s**, drawn by C3 as a dim→amber status role. 60 s is
short enough to notice a hang and long enough to not fire on a slow provider; keep it a `const`.

**Q6. What goes in the "last write" column?**
The last file the task wrote, or the last tool name?
**Recommend:** last file write (ADR 0030 R8 already puts changed files in `list_tasks`), falling
back to the last tool name when no write has happened.

**Q7. Do we emit terminal notifications for finished tasks?**
OSC 777/99/BEL when the window is unfocused, or in-TUI notices only?
**Recommend:** **yes, off by default**, gated on a config key, reusing the `task-completion.ts`
conditions and ADR 0037's background-completion semantics. Off by default keeps ADR 0013's
"no default outward side effects" spirit.

**Q8. Do we copy dsh-TUI's panel **badges** (unread/warn) now?**
Badges need per-panel unread state, which is new state in `App`.
**Recommend:** **later**. Add the roster's counts row now (pure projection), and add per-panel
unread badges only when a panel actually produces unread events (a later orchestration slice).

**Q9. Does the component layer become `pub` for reuse outside `radian-tui` (e.g. a future
`radian-coding` UI)?**
**Recommend:** **no** for V1. Keep it `pub(crate)` inside `radian-tui`. Revisit if a second
front-end appears; premature sharing would freeze an interface before it is used twice.

---

## Appendix: provenance and files read

- **Pinned dsh-TUI commit:** `6e9f07d26ad412fceada8ff47fbe19882dd7ac7c`, cloned to
  `/private/tmp/dsh-tui-src-2026-10-16`. Read-only; no scripts or code run.
- **dsh-TUI files read (partial):** `src/ink/` (directory), `src/components/design-system/*`,
  `Select.tsx`, `listWindow.ts`, `SubagentCard.tsx`, `SubagentDashboard.tsx`,
  `SubagentDetailScene.tsx`, `MessageList.tsx`, `Markdown.tsx`, `StreamingMarkdown.tsx`,
  `MarkdownTable.tsx`, `CodeBlockFrame.tsx`, `SyntaxText.tsx`, `SplitDiffView.tsx`, `diffPatch.ts`,
  `TimelineRail.tsx`, `ScrollbarGutter.tsx`, `OverlayAbove.tsx`, `overlayBudget.ts`, `Tooltip.tsx`,
  `PageMargin.tsx`, `SurfaceEdges.tsx`, `ActivityLine.tsx`, `WorkingSpinner.tsx`, `TurnUsageRow.tsx`,
  `ContextBarView.tsx`, `CommandSuggestions.tsx`, `FileSuggestions.tsx`, `SearchBox.tsx`,
  `approvals/ApprovalPanel.tsx`, `questions/*`, many `*Picker.tsx`, `Whale.tsx`,
  `messages/*`, `sidePanel/*`, `dsh-adapter/subagents.ts`, `dsh-adapter/panels.ts`,
  `dsh-adapter/status.ts`, `dsh-adapter/channel/task-completion.ts`, `ink/terminal-notification.ts`,
  `screens/{AgentTranscriptScene,SessionSupervisor,StatusLine,Chat,SessionTree,Settings,
  Launchpad,Onboarding,TrajectoryScene}.tsx`.
- **radian files read:** ADRs 0023, 0027, 0028, 0029, 0030, 0035, 0037, 0013, 0007, 0021, 0024,
  0026, 0029; `research/2026-10-15-adr-0030-implementation-plan.md`;
  `research/2026-10-14-omp-design.md`; `crates/radian-tui/README.md`;
  `crates/radian-tui/src/{panel,select_list,render,app,footer,tools,markdown,highlight,composer,
  find,clip,command,mention,question,picker,projection,spinner,startup,theme,timing,select,
  events,keys,blocks,workspace,login,profile,signals,terminal,lib}.rs`.
- **Not verified in this study:** dsh-TUI renderer internals, its tests/CI, the `tuiStatus` mount
  site, and anything requiring the app to run.
