# opencode and omp: their TUI components, against radian

- Date: 2026-10-16
- Worktree/branch: `<worktree>/tui-components-study`, branch
  `oc/tui-components-study`, from `19bbd0b` (origin/main plus the dsh-TUI study).
- Purpose: a study, not code. The owner wants to enrich radian's TUI and make panels and
  other components **shareable**. This document answers, for **opencode** and for **omp**:
  (1) the component model, (2) a component inventory with radian's equivalent and a
  recommendation, (3) what radian has today and the ranked gaps, (4) open questions.
  **It changes no code.**
- Companion: `docs/research/2026-10-16-dsh-tui-study.md` (the dsh-TUI study). Read its
  sections 3 and 5 first; this document does not repeat them, and uses the **same table
  shape** (`component (file:line) | what it does | radian equivalent | recommend`) so the
  three can be read together. Where a row here duplicates one there, it says so.
- Status: read-only of untrusted third-party trees. No scripts, `npm install`, or binaries
  were run. opencode and omp were only read.

## How to read this

- **opencode** paths are relative to
  `<scratch>/opencode-src/packages`:
  `tui/` = `packages/tui`, `plugin/` = `packages/plugin` (the plugin SDK),
  `theme/` = `packages/theme`. Version read: **opencode 2.0.30**, TUI package
  `@opencode/tui` 2.0.30 (`tui/package.json:2-3`).
- **omp** paths: `tui/` = `@oh-my-pi/pi-tui` and `pca/` = `@oh-my-pi/pi-coding-agent`,
  both under `<home>/.bun/install/global/node_modules/@oh-my-pi/`. Version read:
  **17.3.8** for both (`pca/package.json:3` = `"version": "17.3.8"`,
  `tui/package.json:3`). See §3.0 for why the version matters.
- **radian** paths are `crates/radian-tui/src/...` in this worktree.
- `(ADR NNNN)` = a radian ADR, not third-party code. `(dsh §N)` = a section of the dsh study.
- `(unverified)` marks a claim I could not check in the read I did. opencode's renderer
  lives in `@opentui/*`, which is **not** in the tree — its internals are not verifiable
  here, only how opencode calls it.
- Third-party line numbers were confirmed with `grep -n`/`sed -n` during the read. The two
  large inventories (§2, §4) were assembled in a read-only pass that quoted each line.

---

## 0. What each TUI is (scale)

**opencode 2.0.30** is a **SolidJS** application rendered through **`@opentui/solid`**
(`tui/src/app.tsx:3`), not React and not Ink. `tui/src/` has **318 files**;
`tui/src/component/` has ~50 top-level `.tsx` files plus a `prompt/` subdir; `tui/src/ui/`
is the overlay/dialog/primitive layer; `tui/src/routes/session/` is the session screen;
`tui/src/plugin/` and `tui/src/feature-plugins/` are the built-in-plugin layer. Its whole
UI is **plugin-contributed**: built-in features and third-party plugins register through one
contract (`plugin/src/tui/context.ts`). It is **multi-pane by default** (session tabs + a
right sidebar + resizable split, `tui/src/component/session-frame.tsx`).

**omp** is two packages. `@oh-my-pi/pi-tui` 17.3.8 is a **self-contained terminal renderer
plus ~17 components** (`tui/src/tui.ts` 5,379 lines; `tui/src/components/*`). Its component
contract is `interface Component { render(width): readonly string[]; … }`
(`tui/src/tui.ts:151`). `@oh-my-pi/pi-coding-agent` 17.3.8 is the application built on it
(~90 files under `pca/src/modes/components/`). omp is **single-column with overlays**: the
main view is a fixed vertical stack of `Container`s, and everything modal is a full-screen
or bottom-anchored **overlay** (`pca/src/modes/interactive-mode.ts:1076-1092`, §3.5). There
is **no built-in sidebar/split** in the app view; there is an **overlay-based multi-agent
hub** (§4.14).

**dsh-TUI** (per the dsh study §0) is React on a checked-in **Ink fork** with its own
reconciler/diffing — a different machine from both. Anything that needs sub-frame diffing
or hover dwell is out of scope here for the same reason it was out of scope there.

**The one-line contrast that matters for radian.** opencode = *everything is a plugin slot
in a Solid tree*; omp = *a component tree drawn by a hand-written renderer, extended by
data-only extension APIs*; radian = *a pure state machine that draws ratatui frames and
composes **free functions** (ADR 0023)*. opencode's plugin registry and omp's tree do not
transplant into radian; the reusable ideas are the **information sets**, the **primitives**,
and the **contribution contracts** (slots/widgets), not the machinery.

---

## 1. opencode: the component model

### 1.1 Renderer and mount

- Entry re-exports the runner: `export { run } from "./app"` (`tui/src/index.tsx:1`).
- `run` is an Effect generator that builds a renderer with `createCliRenderer(options)`
  (targetFps 60, `useKittyKeyboard`, `useMouse` from config) (`tui/src/app.tsx:210`,
  `:239-251`, `:265`).
- Mount is Solid's `render(() => (<LogProvider>…</LogProvider>), renderer)` with the
  renderer as the second argument; the leaf is `<App />` (`tui/src/app.tsx:299`, `:406`,
  `:442`). Components are Solid function components returning JSX: `function App()` returns
  a `<box>` tree (`tui/src/app.tsx:459`), `SessionFrame` returns JSX
  (`tui/src/component/session-frame.tsx:29`), `Dialog` returns JSX (`tui/src/ui/dialog.tsx:34`).
- Dynamic (plugin) components render through Solid's `createComponent`, not JSX tags
  (`tui/src/plugin/render.tsx:66`, `:119`).
- The root is wrapped in an `ErrorBoundary` with `<ErrorComponent>` (`tui/src/app.tsx:311-316`).
- `@opentui/solid` also provides `useRenderer`, `useTerminalDimensions`, `extend`,
  `useKeyboard` (`tui/src/app.tsx:3`; `tui/src/component/terminal-pane.tsx:3,16`). The
  library's **internals are not in this tree** (unverified).

### 1.2 Composition and layout (panes, splits, sidebars)

- Layout constants: `SESSION_SIDEBAR_WIDTH = 42`, `SESSION_SIDEBAR_MAX_WIDTH = 72`,
  `SESSION_TABS_COMPACT_BREAKPOINT = 12`, plus private minima and
  `SESSION_PANEL_SPLIT_MIN_WIDTH = 108` (`tui/src/ui/layout.ts:1-9`). Pure clamp helpers
  `sessionTabsFitVertically`, `clampSessionTabsWidth`, `clampSessionPaneWidth`
  (`tui/src/ui/layout.ts:11-26`).
- The root is a column box sized to the terminal; inside it a `flexDirection="row"` box holds
  vertical tabs | content | resize handle (`tui/src/app.tsx:1362`, `:1381-1389`). Route
  switching is a `<Switch>`: `home`/`session`/`plugin` (`tui/src/app.tsx:1399-1420`), with
  `<Slot path="app" />` under the route content (`tui/src/app.tsx:1422`).
- Sidebar auto-shows only when wide: `wide() = width - verticalTabsWidth > 120` and
  `config.session.sidebar ?? "auto"` (`tui/src/component/session-frame.tsx:106-111`).
  Right-column precedence: panel → sidebar → terminal → sidebar-auto (`:112-117`).
- Resizing is a reusable controller: `createPaneResize` turns mouse events into a clamped
  preferred size, drag/release bound on the **parent** so resizing survives leaving the
  handle, and a **double boundary-click resets** to the default (`tui/src/ui/pane-resize.ts:4`,
  `:41`, `:49-60`). `PaneResizeHandle` is an absolute 2-cell hit area with a 1-cell visible
  line that highlights on hover/resize (`tui/src/ui/pane-resize-handle.tsx:4-30`).
  `SessionFrame` owns the session|right-pane split with its own `createPaneResize`
  (`tui/src/component/session-frame.tsx:45-56`).
- **Panel host**: `PanelHost` renders the `session.panel` slot and passes width /
  presentation / focused / focus / close / toggleFullscreen (`tui/src/component/panel-host.tsx:36-54`).
  The split-vs-fullscreen decision is `canSplit = width >= SESSION_PANEL_SPLIT_MIN_WIDTH`
  with `presentation = canSplit ? requested : "fullscreen"` (`tui/src/context/panel.tsx:15-16`);
  panel lifecycle is `open`/`close`/`release(plugin)`/`toggleFullscreen` (`:23-42`).
- **Composer tabs**: `Composer` (`tui/src/routes/session/composer/index.tsx:21`) is a tab
  host with `ShellTab`, `SubagentsTab`, `TerminalsTab` registered as tabs
  (`tui/src/routes/session/composer/{shell-tab.tsx:12,subagents-tab.tsx:25,terminals-tab.tsx:9}`),
  and a footer-hints accessor per tab (`:35`).

### 1.3 Focus

- There is **no single focus-stack module**. Focus is distributed:
  - the renderer exposes `currentFocusedRenderable` / `currentFocusedEditor`
    (`tui/src/component/session-frame.tsx:134,147`; `tui/src/context/keymap.tsx:94`);
  - focus changes arrive as `CliRenderEvents.FOCUSED_RENDERABLE`; `SessionFrame` maps the
    focused node to `activePane` ("session" | "right") by walking parent pointers
    (`tui/src/component/session-frame.tsx:146-152`, `:402-408`);
  - `SessionFrame` exposes `focusSession` / `focusRightPane` (`:131-145`);
  - interactivity is a boolean tree: `InteractivityProvider` ANDs with its parent, and an
    inactive subtree forces keymap layers `enabled:false` (`tui/src/context/interactivity.tsx:6-13`;
    `tui/src/context/keymap.tsx:188,206,213`). The session pane and the panel are each wrapped
    so only the active one is interactive (`tui/src/component/session-frame.tsx:287`,
    `tui/src/component/panel-host.tsx:60`).
- The only explicit **stack** is the **dialog stack** (§1.5); it pushes a `"modal"` keymap
  mode and captures/restores the previously focused renderable.
- `select-controller.ts` is **pure index math** (no focus logic):
  `reconcileSelection`, `moveSelection`, `revealSelectionOffset`, `moveSelectionOffset`
  (`tui/src/ui/select-controller.ts:5,14,28`).

### 1.4 Theming

- Two theme generations coexist. Built-in legacy themes are 33 JSON files imported into
  `DEFAULT_THEMES` (`tui/src/theme/v1.ts:2-33,37-70`); `assets/v2/opencode.json` is the v2
  default (`tui/src/theme/index.ts:4`). Registry order is defaults < custom files < generated
  system (`tui/src/theme/index.ts:26-37`); `parseTheme` migrates v1 or parses v2
  (`:53-61,85-92`).
- **Component-facing theming is a hook returning a proxy**: `useTheme(): ComponentTheme`
  (`tui/src/context/theme.tsx:362-364`), built by `createComponentTheme`
  (`tui/src/theme/component.ts:5-49`), which lazily exposes
  `hue/categorical/text/background/border/scrollbar/diff/syntax/markdown` and memoizes
  `surface(name)` children.
- Roles are typed by an Effect schema in `@opencode/theme/tui`: hue steps 100–900, required
  semantic hues `accent|interactive|neutral`, action variants `primary|secondary|destructive`,
  action states `disabled|pressed|focused|selected|hovered`, feedback kinds
  `error|warning|success|info`, surface `"dialog"`, plus `SyntaxToken`/`MarkdownToken`
  (`theme/src/tui/schema.ts:3-21,51-58,127,142`). The **resolved** shape consumed by
  components is `ResolvedThemeTokens` (`theme/src/tui/types.ts:25-72`).
- A component reads concrete keys, e.g. `theme.text.feedback[variant].base`
  (`tui/src/ui/toast.tsx:54`), `theme.background.raised.base` (`tui/src/routes/session/sidebar.tsx:23`),
  `theme.text.action.primary.focused` (`tui/src/ui/dialog.tsx:211`),
  `theme.hue.accent[200]` (`tui/src/ui/dialog-select.tsx:718`).
- **Ambient surface switch without remount**: `ThemeContextProvider` re-derives
  `surface(name)` for a subtree; `Dialog` uses `context="dialog"` (`tui/src/context/theme.tsx:368-379`,
  `tui/src/ui/dialog.tsx:42,47`).
- System mode probes the live terminal palette via the renderer and caches it in storage key
  `"system-theme"` (`tui/src/context/theme.tsx:190-218`); legacy resolution is
  `resolveThemeColors` (`tui/src/theme/resolve.ts:11-52`). Theme files are discovered from
  `<configDir>/themes/*.json` (`tui/src/theme/discovery.ts:4-20`).

### 1.5 Dialogs, overlays and toasts

- One shared shell `Dialog`: full-screen absolute box at `zIndex={3000}` with a dim backdrop
  and a centered inner box (`tui/src/ui/dialog.tsx:48-85`). Sizing:
  `dialogWidth` medium 60 / large 88 / xlarge 116; `DIALOG_INSET = 3`,
  `DIALOG_LIST_INSET = 4` (`:24-32`). Composable parts: `DialogHeader` (title + `esc`),
  `DialogLayout` (header/body/footer with side inset), `DialogBleed`, and `DialogFooter`
  (resolves each action's key with `Keymap.useShortcuts`, hides actions with no shortcut)
  (`:90-198`).
- **Stacking** is a Solid store array `{element,onClose,key}[]` with `size`/`centered`;
  `replace()` clears and pushes, `setSize`/`setCentered` persist across the top
  (`tui/src/ui/dialog.tsx:225-234,319-356`). Depth drives `keymap.mode.push("modal")` and
  closes the topmost on `escape`/`ctrl+c` (`:239-243,264-305`). `DialogProvider` renders only
  the top entry, keyed (`:391-400`).
- The reusable implementations: `DialogSelect` (fuzzysort filtering, grouped/flat, category
  headers, its own `"modal"` keymap layer) (`tui/src/ui/dialog-select.tsx:102,173-207,412-516`);
  `DialogPrompt` (textarea + busy; modal layers with `priority:1` and a `target`)
  (`tui/src/ui/dialog-prompt.tsx:32,52-92`); `DialogConfirm` (`tui/src/ui/dialog-confirm.tsx:19-56`);
  `DialogAlert` (`tui/src/ui/dialog-alert.tsx:11-28`).
- **Toasts** are a second surface: `ToastSurface` draws top-right, absolute, colored by
  `theme.text.feedback[variant].base`, click-to-activate (`tui/src/ui/toast.tsx:20,46-62`);
  single current toast + FIFO queue, default 5000 ms, hover pause/resume, `error(err)` helper
  (`:121-214`). `<Toast />` is mounted last in `App`, above the route content
  (`tui/src/app.tsx:1439`).
- **Attention** (sounds + OS notifications) is separate again: `AttentionProvider` builds
  `createTuiAttention({renderer,config})` (`tui/src/context/attention.tsx:11-14`); the
  built-in notifications plugin maps session/form/permission events to
  `context.attention.notify({title,message,notification:{when:"blurred"},sound:{when:"always"}})`
  (`tui/src/feature-plugins/system/notifications.ts:8-52`). *(This is the opencode analogue
  of dsh's `task-completion` gating, dsh §1.5.)*

### 1.6 How plugins and extensions contribute UI

- Contract: `Definition { id, setup(context) }` via `define()` (`plugin/src/tui/plugin.ts:7-14`).
  The `Context` surface is `options, location, app, renderer, client, data, attention, theme,
  themeMode, markdown, keymap, storage, ui` (`plugin/src/tui/context.ts:544-560`).
- **What a plugin contributes** (all through `context.ui`):
  - **Slots** `ui.slot(claim)` with placements `prepend|append|before|after|replace` against a
    fixed **SlotMap**: `app`, `home.footer`, `home.footer.status`, `prompt.footer`,
    `prompt.footer.status`, `prompt.footer.file`, `session.composer.top`, `session.panel`,
    `sidebar.content`, `sidebar.footer` (path containment is prefix-based)
    (`plugin/src/tui/context.ts:191-203,224-262,540-541`).
  - **Routes** `ui.router.register(Page{name,render})`, `navigate`, `current`
    (`plugin/src/tui/context.ts:154-157,496-500`).
  - **Panels** `ui.panel.open(name,{presentation})`/`close`/`current`, rendered into
    `session.panel` (`plugin/src/tui/context.ts:501-508`, `PanelInput` `:168-178`).
  - **Keymaps** `keymap.layer(factory)` + `dispatch/shortcuts/commands/pending/active/mode`
    (`plugin/src/tui/context.ts:435-488`).
  - **Dialogs** `ui.dialog.show/set/clear/alert/confirm/prompt/select`
    (`plugin/src/tui/context.ts:394-405`).
  - **Toasts** `ui.toast.show` (`:280-282`).
  - **Markdown code renderers** `markdown.registerCodeBlockRenderer(language, render)`
    (`:554-556`).
  - **Session tabs** `ui.tabs.*`, **model/variant** `ui.model.*` (`:509-539`), and
    **storage** `storage.store` (durable) / `storage.memory` (`:31-53`).
- The host adapter: `usePluginHost` collects host hooks once (`tui/src/plugin/api.tsx:64-86`);
  `createPluginContext` builds the plugin `Context` (`:91,104-106`). Slot rendering is
  `<Slot path=… input=…>` which enforces mount containment, reference-counts paths, and
  applies `before → (prepend|children|append)|replace → after`, each claim inside a
  `PluginBoundary` error boundary (`tui/src/plugin/render.tsx:85-144`). Placement resolution
  is a pure transform: `replace` is last-enabled-wins, an ancestor replacement suppresses
  descendants, additive claims degrade to the nearest surviving ancestor
  (`tui/src/plugin/structure.ts:74-140`).
- **Built-in vs third-party**: built-ins are a static import list in `tui/src/plugin/builtins.ts:1-31`
  (all of `feature-plugins/*` plus LaTeX/Mermaid plugins; Storybook only under an env gate),
  loaded with `source:"builtin"` (`tui/src/plugin/context.tsx:313-316`); third-party plugins
  are discovered from plugin dirs + server-inventory + `config.plugins` with `source:"external"`
  (`:290-309,667-696`), discovered from the config dir plus project `.opencode` plugin dirs
  (`tui/src/plugin/discovery.ts:8-67`), with a 100 ms debounced hot-reload
  (`tui/src/plugin/context.tsx:256-268`, `tui/src/plugin/watch.ts:16-103`). Only external
  plugins appear in the plugins dialog (`tui/src/feature-plugins/system/plugins.tsx:44-138`).
- Representative built-in contributions: append `home.footer`
  (`tui/src/feature-plugins/home/footer.tsx:114`); append `sidebar.footer` using durable
  storage (`tui/src/feature-plugins/sidebar/footer.tsx:11,121`); append `app` + keymap layer
  + dialog for the "btw" prompt (`tui/src/feature-plugins/prompt/btw.tsx:28-71`); open the
  diff viewer as a panel + route + app slot
  (`tui/src/feature-plugins/system/diff-viewer.tsx:1360-1372`); stats route
  (`tui/src/feature-plugins/system/stats.tsx:186-190`).

### 1.7 Keymap

- A central `Definitions` map of `keybind(default, description)` entries, one per command id,
  `LeaderDefault = "ctrl+x"` (`tui/src/config/keybind.ts:41-301`); it doubles as the override
  schema (`:306-359`).
- The provider wraps children in OpenTUI's `KeymapProvider` with
  `createDefaultOpenTuiKeymap(renderer)` (`tui/src/context/keymap.tsx:63-77,151-164`).
- Scoping is `Keymap.createLayer(factory)`: `groupCommands` splits named (`id`) vs inline
  (`bind`) commands, `bindings: string[]` pulls in other commands' bindings, `mode` defaults
  to `"base"`, `enabled` gates the layer, `target`/`priority` scope it
  (`tui/src/context/keymap.tsx:204-275`). Modes are a stack; `current()` is the last enabled
  entry (`:41,420-453`). Session layers set `priority: 10`
  (`tui/src/routes/session/index.tsx:250-254`). `@opentui/keymap` internals are unverified.

---

## 2. opencode: component inventory

Columns are the dsh-study shape. `tui/` = `packages/tui/src`; `plugin/`, `theme/` as in
"How to read". "radian equivalent" names a radian module/function or `none`.

### 2.1 Input / composer (dsh §5.1)

| opencode component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `component/prompt/index.tsx:185` `Prompt` | the main prompt editor (Solid textarea + parts + autocomplete + ref) | `composer.rs:15` `Composer` | **yes** — radian has the equivalent; take the *draft stash* only if wanted |
| `component/prompt/autocomplete.tsx:59` `Autocomplete` | `@`-file / `/`-command completion popup with an imperative ref | `mention.rs:367` `complete` + `command.rs:550` `SlashCompletion` + `select_list.rs` | **yes** — same feature; radian's completion is `select_list`-driven |
| `component/prompt/metadata.tsx:8` `PromptMetadataRow` | the metadata row above the input (model/agent/etc.) | `footer.rs` items + `render.rs` status row | **later** — radian puts this in the footer; a per-turn metadata row is optional |
| `component/prompt/draft-stash.ts:10` `takeDraft/saveDraft` | per-session draft stash | `composer.rs:331-375` `draft_path/save_draft/take_draft/remove_draft` | **no** — radian already has it |
| `routes/session/composer/index.tsx:21` `Composer` | tabbed composer host (shell / subagents / terminals) | `none` (one composer) | **later** — the *shell* tab is the interesting idea; radian's `!`/`!!` (ADR 0033) is the radian shape, no tabs |
| `routes/session/composer/subagents-tab.tsx:25` `SubagentsTab` | lists/runs a session's subagents from the composer | `none` | **later** — belongs to §5 orchestration |
| `routes/session/composer/shell-tab.tsx:12` `ShellTab` | shell-command input/output tab | `none` (ADR 0033 `!` only) | **later** |
| `routes/session/composer/terminals-tab.tsx:9` `TerminalsTab` | lists the session's terminals | `none` | **no** — radian has no embedded terminals |
| `component/terminal-pane.tsx:24` `TerminalPane` | embeds a real terminal pane (alt screen, palette) | `none` | **no** — out of scope (a whole pty-in-tui feature) |

### 2.2 Transcript blocks (dsh §5.2)

| opencode component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `routes/session/index.tsx:145` `Session` | the session screen: transcript + input + dialogs + helpers | `main.rs` + `render.rs:1405` `draw` + `blocks.rs` | **yes** — the analogue; composition differs (Solid tree vs pure functions) |
| `routes/session/message-parts.tsx:169` `TextPart`/`ReasoningPart`/`InlineToolRow` | renders text, reasoning and inline-tool message parts | `render.rs:339` `display_lines`, `blocks.rs`, reasoning fold | **yes** — covered |
| `routes/session/rows.ts:47` `createSessionRows` | reactive session-row model + turn metrics (duration, tok/s) | `events.rs` + `blocks.rs` + `timing.rs:14` `TurnTimer` | **yes** — radian has the projection + timing |
| `routes/session/grouping/tree.ts:18` `groupEntries` | groups adjacent entries by configurable nesting paths (collapsible) | `blocks.rs:57` `Block` (flat, per entry) | **later** — collapsible *groups* (e.g. exploration) are a real feature radian lacks; not V1 |
| `routes/session/group-view.tsx:33` `SessionGroupView` | one collapsed/expanded group row | `blocks.rs` fold | **later** — pairs with `groupEntries` |
| `component/patch-diff.tsx:38` `PatchDiff` | unified patch rendering with a ref API | `tools.rs:320` `diff_role`, `tools.rs:611` `diff_body` | **yes** — covered (unified only) |
| `feature-plugins/system/diff-viewer.tsx:306` `DiffViewerContent` | the diff viewer *panel* (tree + unified/split) | `none` (diff is inline in the tool card) | **later** — a dedicated diff panel is nice once panels dock; not V1 |
| `component/error-component.tsx:10` `ErrorComponent` | top-level boundary fallback | `terminal.rs:387` panic hook + `signals.rs` | **yes** — radian's model (panic hook + clean restore) is the analogue |
| `routes/session/permission.tsx:113` `PermissionPrompt`/`SessionQuestion` | permission + structured-question prompts | `render.rs:1069-1153` approval; `question.rs:39` `QuestionForm` | **yes** — covered |
| `routes/session/dialog-execute.tsx:28` `DialogExecute` | full detail dialog for one tool part | `dialog-message.tsx` + tool card | **later** — radian shows tool detail inline |

### 2.3 Tool and diff views (dsh §5.3)

| opencode component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `routes/session/message-parts.tsx:26` `toolDisplays` | per-tool display dispatch (shell/glob/read/grep/write/edit/subagent/…) | `tools.rs:221` `renderer_for` (per-tool head/body) | **yes** — radian has the same dispatch |
| `component/one-cell-spinner.tsx:9` `OneCellSpinner` | one-cell animated spinner | `spinner.rs:87` `frame` | **yes** — covered |
| `ui/one-cell-motion.ts:13` `oneCellFrame`/`WORK_SPINNERS` | single-cell spinner frame generator + presets | `spinner.rs` `ARC/QUADRANT/BRAILLE` | **yes** — radian's spinner is the analogue (different glyph set) |
| `feature-plugins/system/diff-viewer-file-tree.tsx:29` `DiffViewerFileTree` | file tree pane for the diff viewer | `none` | **later** — with the diff panel |
| `routes/session/thinking-syntax.ts:3` `generateThinkingSyntax` | derives a syntax style for reasoning text | `theme.rs` `reasoning_*` roles | **yes** — covered (roles, not a SyntaxStyle) |

### 2.4 Markdown / code (dsh §5.4)

| opencode component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `plugin/markdown.ts` (plugin API) + `markdown.registerCodeBlockRenderer` | markdown rendering with plugin-registered code-block renderers | `markdown.rs` (pulldown-cmark) + `highlight.rs` (syntect) | **yes** — covered; the *code-block renderer extension point* is the interesting part (see §6 Q) |
| `routes/session/message-parts.tsx:11` `createSyntaxStyleMemo` | memoized syntax style per theme | `highlight.rs` + `theme.rs` syntax roles | **yes** — covered |
| `theme/*` syntax/markdown tokens | theme-driven syntax + markdown roles | `theme.rs` `md_*`, syntax roles | **yes** — covered |
| Streaming markdown sealing (dsh §5.4 row for `StreamingMarkdown.tsx`) | opencode renders via its own reconciler; **not studied here** | `render.rs` whole-entry re-layout | **no** — needs the renderer's diffing; same verdict as dsh §5.4 |

### 2.5 Dialogs and pickers (dsh §5.5)

| opencode component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `ui/dialog.tsx:34` `Dialog` + parts | the dialog shell (header/body/footer/bleed) and stack | `panel.rs:48` `Panel` | **yes** — radian's `Panel` is the analogue (panel.rs is modelled on omp's overlay) |
| `ui/dialog-select.tsx:102` `DialogSelect` | the shared filterable list dialog | `select_list.rs:90` `ListView` + `picker.rs` | **yes** — covered |
| `component/command-palette.tsx:14` `CommandPaletteDialog` | the command palette (all actions/commands) | `command.rs` tree + `select_list.rs` (no unified palette) | **later** — cheap once `panel.rs` exists; a `/help`/palette overlay is a small win |
| `component/dialog-model.tsx:13` `DialogModel` | `/model` picker with favourites-first ordering | `/model` via `command.rs:819` `model_switch` + `select_list.rs` | **yes** — covered |
| `component/dialog-session-list.tsx:27` `DialogSessionList` | session picker with a status gutter | `picker.rs:45` `Picker` | **yes** — covered |
| `component/dialog-open.tsx:38` `DialogOpen` | open-sessions dialog with reordering | `picker.rs` (flat) | **later** — reorder matters only with session tabs |
| `component/dialog-theme-list.tsx:6` `DialogThemeList` | theme picker | `/theme` via `command.rs:446` `theme_candidates` + `select_list.rs` | **yes** — covered; no live preview (see below) |
| `component/dialog-config.tsx:333` `DialogConfig` | settings editor backed by a `settings` array | `none` (config is a file, ADR 0008) | **no** — radian's settings are files/commands, deliberately |
| `component/dialog-workspaces.tsx:39` `DialogWorkspaces` | workspace picker | `workspace.rs:20` `CodingRoot` | **no** — radian's workspace is cwd-based; not needed |
| `component/dialog-mcp.tsx:36` `DialogMcp` | MCP server list/details | `none` (MCP count in footer) | **later** — a `/mcp` panel is useful but not V1 |
| `component/dialog-agent.tsx:6` / `dialog-skill.tsx:15` | agent / skill pickers | `/skills` (ADR 0025); no agent picker | **later** — with orchestration |
| `routes/session/dialog-timeline.tsx:9` `DialogTimeline` | jump-to-message timeline | `none` (transcript scroll only) | **later** — `find.rs` covers search; timeline jump is a nicety |
| `routes/session/dialog-fork.tsx:13` `DialogFork` | session fork dialog | ADR 0022 fork (command) | **yes** — radian has fork; no dialog needed |
| `feature-plugins/system/storybook/index.tsx:138` | a storybook gallery of components | `none` | **no** — dev tooling; radian uses TestBackend snapshots |
| `component/migration-overlay.tsx:11`, `ui/dialog-export-options.tsx:23`, `ui/dialog-export-result.tsx:5`, `ui/dialog-help.tsx:5` | migration overlay, export options/result, help dialog | `/export` (command); no help overlay | **later** — the help dialog is the only new part |

### 2.6 Menus / suggestions (dsh §5.6)

| opencode component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `component/prompt/autocomplete.tsx:59` | completion overlay (commands + files) | `command.rs`, `mention.rs`, `select_list.rs` | **yes** — covered |
| `feature-plugins/system/plugins.tsx:26` `PluginsDialog` | plugin manager | `none` | **no** — no dynamic plugin UI (ADR 0023) |

### 2.7 Status bar / footer (dsh §5.7)

| opencode component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `feature-plugins/sidebar/footer.tsx:116`, `feature-plugins/prompt/footer.tsx:134`, `feature-plugins/home/footer.tsx:107` | footer/sidebar/prompt footers as **plugin slots** | `footer.rs:320` `Footer`, `footer.rs:433` `lines` | **yes** — radian's footer is the analogue; note opencode's footer is a *slot*, radian's is a *config table* (ADR 0027 C2) |
| `feature-plugins/sidebar/context.tsx:44` `SidebarContext` | context-usage section | `footer.rs` context item + bar | **yes** — covered |
| `feature-plugins/sidebar/mcp.tsx:95` `SidebarMcp` | MCP status section | `footer.rs` `mcp` item | **yes** — covered |
| `mini/footer.ts:164` `RunFooter` + `footer.view.tsx:130` | the *mini* (non-fullscreen) footer state machine + view | `render.rs` (single mode) | **no** — radian is full-screen only (ADR 0009) |
| `component/devtools-bar.tsx:29` `DevToolsBar` | debug bar (runtime graph, connection indicator) | `none` | **no** — dev tooling |

### 2.8 Notifications / toasts (dsh §5.8)

| opencode component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `ui/toast.tsx:203` `ToastProvider`/`useToast`; `:20` `ToastSurface` | a queued toast surface (top-right, variant-colored, hover-pause) | `render.rs` transient notice (single) | **later** — radian has one transient note; a small toast queue is a real gap (see §5) |
| `context/attention.tsx:11-14` + `feature-plugins/system/notifications.ts:8-52` | sounds + OS notification gated on `blurred`, per-event | `render.rs` completion notice (ADR 0037) | **later** — take the *gating* (only when blurred), keep terminal notifications off by default (ADR 0013) |
| `feature-plugins/system/notifications.ts:21` | the notification plugin (session/form/permission events) | ADR 0037 background-completion notice | **yes** — the *event set* is the reusable part |

### 2.9 Scrolling, selection, copy (dsh §5.9)

| opencode component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `routes/session/index.tsx` scrollbox + `routes/session/anchors.ts:53`, `anchor-view.tsx:29` | transcript scrolling by **anchors** (entry/group anchor keys) | `render.rs` scroll (entry-index anchor, ADR 0023 U9) | **yes** — radian's model is the analogue; anchors are the same idea |
| `routes/session/mount-budget.ts:10` `rowWeight/rowsBefore/rowsAfter` | virtualizes entry mounting to a row budget | `blocks.rs:982` `line_count_cached` + ADR 0028 R7 | **yes** — covered (radian budgets lines) |
| `ui/link.tsx:18` `Link`/`clickedLink` | clickable hyperlinks with hit resolution | `none` | **later** — click targets only once mouse is a first-class model |
| `context/clipboard.tsx:10-19` `ClipboardProvider` | clipboard service (read/write) | `clip.rs:18` `ClipboardMode`, `clip.rs:53` `osc52` | **yes** — covered (OSC 52) |
| `util/selection.ts` / `selection-copy-on-select` (test `test/util/selection-copy-on-select.test.tsx`) | cell selection + copy | `select.rs:49` `Selection`, `select.rs:295` `logical_text` | **yes** — radian's anchor-based selection is the analogue and is more robust |
| `test/ui/pane-resize.test.ts` (behavior) | pane resize math | layout not present | — |

### 2.10 Mouse (dsh §5.10)

| opencode component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `ui/pane-resize.ts:4` `createPaneResize` | drag-to-resize panes | `none` | **later** — needs a pane model first |
| `component/session-tabs.tsx` mouse + `test/component/session-tabs-mouse.test.tsx` | click/hover on tabs | `terminal.rs:81` `write_mouse` (enabled) + `select.rs` | **later** — radian enables mouse and handles selection; click *targets* are the gap |
| `ui/scrollbar` (in `session-frame`/lists) | scrollbar thumbs | `select_list.rs:156` list scrollbar; none for transcript | **later** — transcript scrollbar is a real gap (see §5) |

### 2.11 Themes (dsh §5.11)

| opencode component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `context/theme.tsx:362` `useTheme` + `theme/component.ts:5` | role proxy consumed by components | `theme.rs:128` `Role`, `theme.rs:425` `Theme` | **yes** — covered; radian resolves roles in one place |
| `theme/assets/*.json` (33 themes) + `theme/discovery.ts:4` | built-in + discovered theme files | `theme/file.rs` (pi-compatible files, ADR 0029) | **yes** — covered |
| `theme/system.ts:11` `generateSystem` + live-palette probe | a theme generated from the terminal palette | `none` (ADR 0023 forbids OSC 11 probing) | **no** — deliberately not done (pty can't test it, ADR 0009/0023 U9) |
| `component/dialog-theme-list.tsx` + a preview | theme picker with preview | `/theme` list only | **later** — preview pane once panels exist |

### 2.12 Animations (dsh §5.12)

| opencode component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `ui/animation.ts:37` `spring`/`tween`/`createAnimatable` | tween/spring animatable values | `none` | **no** — needs sub-frame timing; ADR 0023 idle = zero redraw |
| `component/tab-pulse.tsx:758`, `component/title-shimmer.tsx:27`, `component/fade-in-text.tsx`, `component/shimmer-text.tsx` | tab glow, title shimmer, fade-in, shimmer text | `spinner.rs` (glyph-only, time-driven) | **no** — same reason (dsh §5.12 verdict) |
| `component/spinner.tsx:14` + `spinner-frames.ts:1` | braille spinner with color/shimmer | `spinner.rs:87` `frame` | **yes** — covered by the time-driven spinner |

### 2.13 Layout and resizing (dsh §5.13)

| opencode component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `ui/layout.ts:1-26` | split/sidebar width constants + clamp helpers | `none` (single column, ADR 0023) | **later** — the orchestration panel is the reason to add one (dsh §4) |
| `ui/pane-resize.ts` + `ui/pane-resize-handle.tsx:4` | split with a draggable divider (parent-bound drag, double-click reset) | `none` | **later** — copy the *behaviour* (parent-bound drag, reset) when splits land |
| `component/session-frame.tsx:29` | the per-session frame (content + tab rail + right pane) | `render.rs` single frame | **later** — the model for `App` split into global + per-session view (ADR 0030 slice 3) |
| `component/session-tabs.tsx:506` / `session-tabs-rail.tsx:8` | horizontal/vertical session tabs with unread/spinner | `none` | **later** — this is the multi-session UI (see §5) |
| `context/panel.tsx:15-42` | panel hosting + split-vs-fullscreen policy | `panel.rs` (overlay only) | **yes** — copy the *policy* (`min width → fullscreen`) for radian's overlay panels |

### 2.14 Design-system primitives (dsh §5.14)

opencode has **no separate design-system folder**; its primitives are the `ui/` dialogs plus
`ui/border.ts:1` (`EmptyBorder`/`SplitBorder`), `ui/file-path.tsx:14` (`FilePath`,
`truncateFilePath`), `ui/subcell.ts:32` (`octantGlyph`), `ui/select-controller.ts:5`
(selection math), and `ui/border`/`layout`. Compare dsh §1.3 (a dozen pure primitives) and
omp (§4.14): **opencode is the outlier — its reuse is at the dialog/slot level, not a
primitive kit.** The reusable primitives for radian come from dsh and omp, not opencode.

### 2.15 Screens and other (dsh §5.15)

| opencode component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `routes/home.tsx` + `feature-plugins/home/footer.tsx:107` | home/launch screen | `startup.rs:52` `StartupScreen` | **yes** — radian has a startup block |
| `context/session-tabs.tsx` / `session-tabs-model.ts:1` | several sessions at once | `none` | **later** — §5 |
| `simulation/*`, `devtools/*` | a simulation harness + devtools | `tests/` (pty + TestBackend) | **no** — different test strategy |
| `feature-plugins/system/stats.tsx:182` `StatsPoster` | session stats dashboard | `none` | **later** — a `/stats` panel is a nicety |

---

## 3. omp: the component model

### 3.0 Version: read 17.3.8, but the ADRs used 18.10.4 (important)

The installed packages are **17.3.8** (`pca/package.json:3`,
`tui/package.json:3`). The radian ADRs and the earlier omp research were written against
**18.10.4** (`docs/research/2026-10-14-omp-blocks.md:11-30`), which mid-research replaced the
installed 17.3.8. The two differ structurally:

- 18.10.4 moved the chat components into `pi-tui`: chat blocks in `tui/src/chat/`, the shell
  in `tui/src/chrome/`, render primitives in `tui/src/render/`, theme into `tui/src/theme/`
  (`2026-10-14-omp-blocks.md:22-30`).
- 17.3.8 keeps them in **`pca/src/modes/components/`** and puts the render primitives in
  **`pca/src/tui/`** (`pca/src/tui/{output-block,code-cell,status-line,file-list,tree-list}.ts`),
  theme in **`pca/src/modes/theme/`**.

**Consequence for radian:** `crates/radian-tui/src/panel.rs:10-12` cites "omp's `OverlayPanel`
(`tui/src/chrome/overlay-box.ts:218-350`)". **That file and that symbol do not exist in the
installed 17.3.8** — `grep -rln "OverlayPanel"` over `@oh-my-pi/` returns nothing and there is
no `chrome/` directory. The citation is valid only against 18.10.4. In 17.3.8 the box-drawing
chrome helpers are `pca/src/modes/components/overlay-box.ts` (109 lines: `fit:11`,
`topBorder:26`, `divider:40`, `bottomBorder:45`, `row:51`, `splitBodyWidth:68`,
`topBorderSplit:73`, `dividerSplit:93`, `splitRow:104`) — **helpers, no panel class**. This
document describes **17.3.8** and marks anything it can only learn from 18.10.4.

### 3.1 The renderer and the component contract

`tui/src/tui.ts` (5,379 lines) is a hand-written renderer:

- The contract is `export interface Component` (`tui/src/tui.ts:151`) with
  `render(width): readonly string[]` (`:158`), optional `handleInput?(data)` (`:163`),
  `wantsKeyRelease?` (`:169`), `invalidate?()` (`:175`), `setIgnoreTight?(ignore)`
  (`:179`), `dispose?()` (`:187`). **Contract: return the same array reference when
  unchanged — reference equality is the memoization proof** (`:137-149`).
- `Container` (`:538`) holds `children: Component[]` (`:541`) with `addChild`/`removeChild`/
  `clear`/`dispose`; `Container.render` concatenates children and **memoizes on the children
  array's identity** (`:824`). `TUI extends Container` (`:1162`), constructed with a
  `Terminal` (`:1481`), overriding `render` (`:1630`).
- Render scheduling is explicit: `requestRender(force?)` (`:2449`),
  `requestComponentRender(component)` for a subtree (`:2513`), `requestDirectWrite(component)`
  to rewrite one segment (`:2534`), `enableScopedInputRender` (`:2494`). Throttle floor
  `#MIN_RENDER_INTERVAL_MS = 1000/30` (`:1186`), adaptive cap 200 ms (`:1193`),
  `#scheduleRender` (`:2893-2927`), `#doRender` (`:3381`, frame composed `:3523`). Diffing is
  strategy-based (first render / clear+full / move-to-changed), `tui/README.md:511-516`.
- Differential seams beyond the previous frame: stable prefix (`:301`), native-scrollback live
  region (`:215`), width epoch (`:231`), viewport-tail fast path (`:330`). Synchronized output
  `CSI 2026` (`:77-78`).
- **Focus**: `Focusable { focused; setUseTerminalCursor? }` (`:350`); `setFocus(component)`
  overlay-aware (`:1971`); `getFocused()` (`:1997`); `CURSOR_MARKER` (`:374`).
- **Overlays**: `OverlayOptions` (`:471`), `OverlayHandle {hide/setHidden/isHidden}` (`:526`),
  `overlayStack` (`:1474`), `showOverlay` (`:2005`), `hideOverlay` (`:2058`);
  `#compositeOverlaysIntoWindow` splices overlay lines into the frame (`:3174`).
- **Mouse is on only while a fullscreen overlay owns the alt screen**
  (`tui/src/tui.ts:94`, enabled `:3403/:3446`); hover/click hit-testing is component-local
  (`TabBar.setHoverTab`, `SelectList.setHoverIndex`) — there is **no general pointer model**.
- **Resize**: `#beginResizeViewport` throwaway drag frames (`:4814`), multiplexer debounce
  (`:2779`), `#resizeRepaintsInPlace` (`:4958`).

### 3.2 Composition

Components compose by **containment**: `Box(paddingX,paddingY,bgFn?,border?)` holds children
(`tui/src/components/box.ts:38`); `Text` (`text.ts:22`), `Spacer(n)` (`spacer.ts:6`),
`TruncatedText` (`truncated-text.ts:7`, no full-width pad → copy-safe) are leaves; several
leaves **contain** others — `Loader extends Text` (`loader.ts:20`), `CancellableLoader extends
Loader` (`cancellable-loader.ts:13`), `SelectList` instantiates a `ScrollView`
(`select-list.ts:86`), `Editor` owns an autocomplete `SelectList` (`editor.ts:409`).
`Box`/`Text`/`Markdown`/`Editor` implement the native-scrollback seam interfaces. The app
mounts a `TUI` and pushes `Container`s onto it.

### 3.3 The two-layer transcript model (the key abstraction)

`docs/research/2026-10-14-omp-blocks.md` §4 already established this against 18.10.4; the
17.3.8 classes are structurally identical and I re-confirmed them:

- **Lifecycle layer — `ChatBlock`**: `abstract class ChatBlock extends Container`
  (`pca/src/modes/components/chat-block.ts:29`) with `mount(host)` (`:70`),
  `onMount`/`onCleanup` (`:40,47`), `finish()` (`:82`), `dispose()` (`:93`), and the contract
  `isTranscriptBlockFinalized()` (`:103`) → the container keeps unfinished blocks in a
  redrawable live region. Minimal host capability is `interface ChatBlockHost { requestRender() }`
  (`:7`); the app's host is `readonly #chatHost = { requestRender: () => this.ui.requestRender() }`
  (`pca/src/modes/interactive-mode.ts:761`).
- **Layout layer — `TranscriptContainer extends Container`**
  (`pca/src/modes/components/transcript-container.ts:161`): renders every child each frame
  (`:452`), reports the native-scrollback exactness seam (`:359,364`), strips plain-blank
  top/bottom edges from each child (`stripPlainBlankEdges` `:99-105`), and inserts exactly one
  blank between visible blocks, never doubling (`:589-594`). `TranscriptBlock extends Container`
  (`:657`) groups sibling rows so the container spaces the group as one block.
- **Indentation/padding is per block**, not the container's job: `Markdown(text, paddingX,
  paddingY, …)` (`tui/src/components/markdown.ts:1482`) is called `new Markdown(text,1,1,…)`
  for the user block (`pca/src/modes/components/user-message.ts:61`) and `1,0` for assistant
  text. Bands (backgrounds) are per-block `Box`es (`custom-message.ts:24`).
- The same two-layer idea is why omp reads as tidy: **container owns inter-block spacing;
  only the tool-card family has a unified gutter**. (dsh §1.3 said the same for dsh.)
- Per-block commit seams: `isTranscriptBlockFinalized`, `getTranscriptBlockVersion`,
  `getTranscriptBlockSettledRows`, `isDisplaceableBlock`, `seal`
  (`pca/src/modes/components/transcript-container.ts:18-57`).
- **`ChatTranscriptBuilder`** (`chat-transcript-builder.ts:84`) rebuilds a full container from
  persisted `SessionMessageEntry[]` for file/remote transcripts (`rebuild` `:110`, `append`
  `:120`) — no incremental reuse (`:8-12`). It pairs tool calls to results by
  `#pendingTools: Map<toolCallId, ToolExecutionComponent>`, filling the **same card**
  (`2026-10-14-omp-blocks.md` §2.1).

### 3.4 Transcript block components (17.3.8 locations)

- `AssistantMessageComponent extends Container` (`pca/src/modes/components/assistant-message.ts:181`)
  — streaming text, thinking animation, tool timers, images; `updateContent` (`:793`),
  `isTranscriptBlockFinalized` (`:455`), `getTranscriptBlockSettledRows` (`:472`).
- `UserMessageComponent extends Container` (`user-message.ts:32`) wraps its rows in OSC-133
  shell-integration markers (`:69-83`); `CollapsedSyntheticMessageComponent` (`:99`) is a lazy
  collapsed placeholder for huge synthetic inputs.
- `ToolExecutionComponent extends Container` (`tool-execution.ts:340`) — the generic tool block
  (spinner, diff preview, wait/poll cards); `ToolExecutionHandle` (`:253`), `ToolExecutionUi`
  (`:232`); shared spinner frame `:283`. `ToolActivityContainer` (`tool-activity.ts:11`) is a
  visibility wrapper proxying `setExpanded`.
- `renderDiff(diffText, options)` (`diff.ts:108`) — colored diff with intra-line highlighting.
- `ReadToolGroupComponent` (`read-tool-group.ts:329`) collapses a run of `read` calls into a tree.
- `BashExecutionComponent extends Container` (`bash-execution.ts:36`) — streaming bash block;
  shares `execution-shared.ts` (`buildExecutionFrame:27`, `buildStatusFooter:67`).
- Summaries: `CompactionSummaryMessageComponent` (`compaction-summary-message.ts:85`) plus
  handoff/branch variants (`:127,155,170`), sharing `SummaryDividerComponent` (`:15`).
- `CustomMessageComponent extends Container` (`custom-message.ts:12`) → `renderFramedMessage`
  (`message-frame.ts:54`). `ErrorBannerComponent extends Container` (`error-banner.ts:16`) is
  pinned above the editor. `formatUsageRow` (`usage-row.ts:19`).

### 3.5 Status line and footer

- `StatusLineComponent implements Component` (`pca/src/modes/components/status-line/component.ts:293`).
  The main status is rendered into the **editor's top border** via `getTopBorder(width)`
  (`:1866`); `render(width)` returns only hook-status rows (`:1880-1890`).
- Segment model: `StatusLineSegment { id; render(ctx): RenderedSegment }`
  (`status-line/types.ts:129-137`); the registry `SEGMENTS` has 24 ids — `pi, model, mode,
  path, git, pr, subagents, token_in, token_out, token_total, token_rate, cost, context_pct,
  context_total, time_spent, time, session, hostname, cache_read, cache_write, cache_hit,
  session_name, usage, collab` (`status-line/segments.ts:689-724`).
- Presets `status-line/presets.ts:3-101`, separators (powerline/slash/pipe/block/none/ascii)
  `status-line/separators.ts:8`, context thresholds `status-line/context-thresholds.ts:31,75`.
- **Width-based dropping** (`component.ts:1683`): right segments are `pop()`ed while over
  budget (`:1769-1772`), the elastic `path` segment is re-rendered shorter first (`:1773-1805`),
  then left segments are dropped right-to-left **except `path`** (`:1806-1822`).
- `FooterComponent` (`pca/src/modes/components/footer.ts:16`) is a second, simpler footer
  (pwd/branch/tokens/cost left, model/thinking right; `setExtensionStatus` `:36`).

### 3.6 Input / editor / keybindings

- `Editor` (`tui/src/components/editor.ts:409`) is the multi-line editor (wrapping, history,
  kill-ring, autocomplete, paste markers, undo; `onSubmit` `:503`,
  `setAutocompleteProvider` `:532`). omp **wraps** it: `CustomEditor` in
  `pca/src/modes/components/custom-editor.ts:25` adds per-action key overrides, magic-keyword
  highlighting, image-reference placeholders, and the `->`/`=>` **yield-queue** shorthand
  (`pca/src/modes/queue-input.ts`, `parseQueueShorthand`).
- `Input` (`tui/src/components/input.ts:27`) is the single-line input. `SelectList`
  (`select-list.ts:86`) hosts a `ScrollView`. **There is no composer-shape registry in
  17.3.8** — the 8-shape `composer/registry.ts` (incl. `rail`, the `bar` model) is an
  18.10.4 addition cited by `docs/research/2026-10-14-omp-design-blocks.md` §4.1. In 17.3.8
  the editor shape is mobile via `CustomEditor`.
- **Keymap**: `keys.ts` parses (`parseKey:563`, `matchesKey:550`, Kitty support `:372`);
  `keybindings.ts` holds `TUI_KEYBINDINGS` defaults (`:57`) and `KeybindingsManager.matches`
  (`:242,288`); the app adds `KEYBINDINGS = { ...TUI_KEYBINDINGS, "app.*": {defaultKeys,
  description} }` (`pca/src/config/keybindings.ts:78`) with a module augmentation so action
  ids are typed (`:62-64`), and `KeybindingsManager` (`:567`) plus `formatKeyHint` (`:732`).
- **Autocomplete**: `AutocompleteProvider` (`tui/src/autocomplete.ts:196`),
  `CombinedAutocompleteProvider` (slash + file/`@`) (`:401`). omp adds
  `prompt-action-autocomplete.ts`, `emoji-autocomplete.ts`, `github-ref-autocomplete.ts`,
  `internal-url-autocomplete.ts` (`pca/src/modes/`).
- **Bracketed paste** `BracketedPasteHandler` (`tui/src/bracketed-paste.ts:69`, 64 MiB cap);
  `StdinBuffer` splits partial escape sequences across chunks with timeouts
  (`tui/src/stdin-buffer.ts:383,349-372`); `KillRing` 60-entry (`kill-ring.ts:10`).
- **Key-hint rendering** is a helper, not a component: `keyHint(action, description)` =
  `dim(key) + muted(" " + description)` (`pca/src/modes/components/keybinding-hints.ts:30`),
  with `editorKey:11`, `appKey:18`, `appKeyHint:43`, `rawKeyHint:54`.

### 3.7 Theming

- A global singleton `theme` (`pca/src/modes/theme/theme.ts:80`) switched by
  `setThemeInstance` (`:237`) with `onThemeChange` (`:302`). The class `Theme`
  (`theme-class.ts:131`) is what components call: `theme.fg/bg/bold/icon/sep/status/thinking`,
  `getFgAnsi`, `getContrastFgAnsi`, `styledSymbol`.
- `tui-adapters.ts` adapts `Theme` to the pi-tui component themes: `highlightCode:82`,
  `getSymbolTheme:90`, `getMarkdownTheme:145`, `getSelectListTheme:202`, `getEditorTheme:227`,
  `getSettingsListTheme:246` (`pca/src/modes/theme/tui-adapters.ts`).
- Symbols: `SymbolPreset = "unicode"|"nerd"|"ascii"` (`symbols.ts:5`), the 270-key table
  (`docs/research/2026-10-14-omp-design.md` §1.2). Shimmer: `shimmerText:303` (`shimmer.ts`).
- **99 bundled theme JSONs** under `pca/src/modes/theme/defaults/` (`ls | wc -l` = 99), plus
  `dark.json`/`light.json`; schema in `theme/schema.ts` + `theme/theme-schema.json`.
- The `bar`/`rail` composer's status attachment, and the composer-shape registry, are 18.10.4
  (`2026-10-14-omp-design-blocks.md` §4.1, §6); 17.3.8 attaches status at the top border.

### 3.8 How extensions contribute UI

omp has **no slot registry**. Extensions get a **data/placement contract**
`ExtensionUIContext` (`pca/src/extensibility/extensions/types.ts:254-369`) that can:
`select` (`:258`), `confirm` (`:265`), `input` (`:268`), `askDialog` (`:271`), `notify`
(`:277`), `onTerminalInput` (`:280`), `setStatus` (`:283`), `setWorkingMessage` (`:286`),
**`setWidget` above/below the editor** (`:289`), `setFooter` (`:292`), `setHeader` (`:295`),
`setTitle` (`:298`), **`custom()` — an overlay/editor component** (`:301`),
`setEditorText`/`pasteToEditor`/`getEditorText` (`:312-323`), `editor()` (`:326`),
`addAutocompleteProvider` (`:339`), `setEditorComponent` (`:348`), theme access
(`:353-362`), `getToolsExpanded`/`setToolsExpanded` (`:365-368`). Widgets are placed with
`ExtensionWidgetOptions { placement: "aboveEditor"|"belowEditor" }` (`:220-224`) and are
capped at `MAX_WIDGET_LINES = 10` (`pca/src/modes/controllers/extension-ui-controller.ts:310-346`).
Modal surfaces replace the editor slot one at a time (`#dialogActive`/`#dialogQueue`,
`:69-73,563-1031`). The Extension Control Center (`/extensions`) is itself a component:
`ExtensionDashboard` (`pca/src/modes/components/extensions/extension-dashboard.ts:66`),
`ExtensionList` (`extension-list.ts:35`), `InspectorPanel` (`inspector-panel.ts:13`).
*(Compare opencode §1.6: opencode lets a plugin inject arbitrary components into named slots;
omp's `custom()`/`setWidget` is the closest, but the rest is data+placement — closer to
radian's ADR 0027 C3 model.)*

### 3.9 Overlays, panels, mouse, resize in the app

- Main view = a fixed vertical stack appended in order: transcript, pending, todo, subagent,
  btw, omfg, error-banner, model-cycle, deferred-command, working status, status line, hook
  widget above, editor, hook widget below (`pca/src/modes/interactive-mode.ts:1076-1092`);
  focus is a single owner, the editor (`:1093`).
- Everything modal is an overlay: `this.ui.showOverlay(overlay, {anchor, width, maxHeight,
  margin, fullscreen, mouseTracking})` (`:3084-3091`). `BtwPanelComponent extends Container`
  (`pca/src/modes/components/btw-panel.ts:33`) builds a rounded panel from `DynamicBorder`
  (`dynamic-border.ts:14`) + `Spacer` + `Text`/`Markdown` and rebuilds via
  `requestComponentRender` (`:111-127`); its key hints are a themed footer line (`:129`).
  `plan-review-overlay.ts` is a full overlay with sidebar/body/footer built from the
  `overlay-box` helpers, its `render(width)` at `:1160`.
- `renderSubagentHudLines(sessions, columns)` (`interactive-mode.ts:465-509`) draws a
  "Subagents" HUD pinned at a live-region seam (`SUBAGENT_HUD_VISIBLE_LIMIT = 8` `:451`;
  injected `:2356`).

### 3.10 What I could not verify (omp)

- The `OverlayPanel` symbol and `tui/src/chrome/overlay-box.ts:218-350` — **not present in the
  installed 17.3.8** (§3.0). The `panel.rs` citation is only meaningful against 18.10.4.
- `pca/src/modes/theme/mermaid-cache.ts` exports (only its import was seen).
- The full method surface of `theme-class.ts` (only the class line and call sites were read).
- Anything about running omp, its tests, or its CI (not run/read).

---

## 4. omp: component inventory

Columns are the dsh-study shape. `tui/` = `@oh-my-pi/pi-tui/src` 17.3.8; `pca/` =
`@oh-my-pi/pi-coding-agent/src` 17.3.8. Rows already covered by
`docs/research/2026-10-14-omp-design*.md` say so and are cited, not re-derived.

### 4.1 Input / composer (dsh §5.1)

| omp component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `tui/src/components/editor.ts:409` `Editor` | multi-line editor (wrap/history/kill-ring/autocomplete/undo) | `composer.rs:15` `Composer` | **yes** — radian has the equivalent; take the *paste markers / undo* ideas only if needed |
| `pca/src/modes/components/custom-editor.ts:25` `CustomEditor` | per-action keys, magic-keyword highlight, image refs, `->` queue shorthand | `composer.rs` + `keys.rs:429` `Keymap` | **later** — the *magic-keyword highlight* is a nicety; radian's keymap is the analogue |
| `pca/src/modes/queue-input.ts` `parseQueueShorthand` | `->`/`=>` "yield queue" message shorthand | `none` | **later** — small, but radian has no queue-as-message concept |
| `tui/src/components/input.ts:27` `Input` | single-line input | `composer.rs` (multi-line) | **no** — radian's composer is multiline |
| `tui/src/autocomplete.ts:196/401` `AutocompleteProvider`/`CombinedAutocompleteProvider` | slash + file/`@` completion | `command.rs`, `mention.rs`, `select_list.rs` | **yes** — covered |
| 8 composer shapes (`bar`/`rail`, …) | 18.10.4-only (not in 17.3.8); radar's `bar` is modelled on it | `render.rs:709` `InputStyle::{Bar,Lines,Box}` | — see `2026-10-14-omp-design-blocks.md` §4.1; radian already implements `bar` |
| `tui/src/bracketed-paste.ts:69`, `stdin-buffer.ts:383`, `kill-ring.ts:10` | paste buffering, escape-sequence splitting, kill ring | `events.rs:90` `input_event` + terminal input thread | **yes** — radian handles paste/split; the *kill ring* is a nicety (later) |

### 4.2 Transcript blocks (dsh §5.2) — mostly covered by the omp blocks study

| omp component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `pca/src/modes/components/chat-block.ts:29` `ChatBlock` | the lifecycle block abstraction | `blocks.rs:57` `Block` (layout) + `app.rs` entry model | **yes** — the *finalized/live* split is the reusable idea (ADR 0028 R7 live region) |
| `transcript-container.ts:161,657` `TranscriptContainer`/`TranscriptBlock` | container owns inter-block spacing; groups rows | `blocks.rs:763` `block_gap`, `blocks.rs:814` `gaps` | **yes** — radian already implemented this (ADR 0028) |
| `assistant-message.ts:181` `AssistantMessageComponent` | streaming text + thinking + tool timers + images | `render.rs:339` `display_lines`, `blocks.rs` | **yes** — covered |
| `user-message.ts:32` `UserMessageComponent` | user bubble with OSC-133 markers; collapsed synthetic variant | `blocks.rs` user block + `render.rs` band | **yes** — covered (OSC-133 not needed) |
| `tool-execution.ts:340` `ToolExecutionComponent` | one card per tool, result filled in | `tools.rs:32` `ToolCard`, `tools.rs:221` `renderer_for` | **yes** — covered (ADR 0028/3) |
| `read-tool-group.ts:329` `ReadToolGroupComponent` | collapses a run of `read` into one card | `none` | **later** — a nice compaction; not V1 |
| `bash-execution.ts:36` + `execution-shared.ts:27,67` | bash block (frame + footer, `!`/`!!` color by context) | ADR 0033 (`!`/`!!`) | **yes** — the `!`/`!!` shape is already decided (omp study §5) |
| `diff.ts:108` `renderDiff` | colored diff with intra-line highlight | `tools.rs:320` `diff_role`; no intra-line highlight | **later** — intra-line highlight is high-cost/low-value (ADR 0035 agreed) |
| `compaction-summary-message.ts:85` | compaction as a **rule line** (expandable) | `blocks.rs` compaction block | **yes** — covered (ADR 0035 V2) |
| `error-banner.ts:16` `ErrorBannerComponent` | error banner pinned above the editor | `render.rs` notices + approval area | **later** — a pinned error banner vs a notice |
| `message-frame.ts:54` `renderFramedMessage` | shared framed-message renderer for extension cards | `panel.rs` + `blocks.rs` cards | **later** — relevant only with extension messages |
| `ttsr-notification.ts:15` `TtsrNotificationComponent` | a rule-violation notification block | `none` | **no** — omp-specific (time-traveling stream rules) |
| `compaction`/handoff/branch summaries `:127,155,170`; `usage-row.ts:19` | summary variants; usage row | `blocks.rs`; `footer.rs` usage items | **later** — usage row in transcript is optional |

### 4.3 Tool and diff views (dsh §5.3)

| omp component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `pca/src/tui/output-block.ts` `renderOutputBlock` | bordered output block primitive | `blocks.rs` `Card`/`Frame`/`Pad`, `tools.rs` | **yes** — radian has the card frame |
| `pca/src/tui/code-cell.ts` | a code cell (bordered code block for `read`) | `blocks.rs` code block + `highlight.rs` | **yes** — covered |
| `pca/src/modes/components/diff.ts:108` | the tool diff renderer | `tools.rs:611` `diff_body` | **yes** — covered |
| 18.10.4 `ToolCard` (`tui/src/render/tool-card.ts`) | unified tool-card topology (`framed`/`plain`, 7 phases) | `tools.rs:32` `ToolCard`, `blocks.rs:263` `Card` | — see `2026-10-14-omp-design.md` §9.3 row 1; radian already has a card model |

### 4.4 Markdown / code (dsh §5.4)

| omp component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `tui/src/components/markdown.ts:1482` `Markdown` | marked-based renderer with a streaming lex cache | `markdown.rs` (pulldown-cmark) + `highlight.rs` | **yes** — covered; `docs/research/2026-10-14-omp-design-markdown.md` §2 is the element-by-element basis |
| `pca/src/modes/markdown-prose.ts:1` `maskNonProse` | masks code/markup so magic-keyword matching never fires in prose | `none` | **later** — relevant only if radian adds magic keywords |
| `pca/src/tui/width-aware-text.ts`, `tui/src/utils.ts:297` `visibleWidth` | width-aware layout + cached visible width | `blocks.rs` `unicode-width`; CJK handled | **yes** — covered (ADR 0028 R9) |
| `tui/src/latex-block.ts:1437`, `latex-to-unicode.ts:1783` | LaTeX math (block + inline) | `none` | **no** — 3,663 lines; ADR 0035 decided "show as source" for V1 |
| `tui/src/kitty-graphics.ts:21`, `components/image.ts:475` | Kitty/iTerm2 images | `none` (text only) | **no** — no graphics protocol |

### 4.5 Dialogs and pickers (dsh §5.5)

| omp component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `pca/src/modes/components/overlay-box.ts:11-104` | shared rounded box chrome helpers (`topBorder`/`divider`/`row`/`splitRow`) | `panel.rs:48` `Panel` | **yes** — radian's `Panel` is the analogue (cites omp) |
| `ask-dialog.ts:392` `AskDialogComponent` | multi-question ask dialog (tabs/multi-select/recommended) | `question.rs:39` `QuestionForm`, `render.rs:1950` `question_panel` | **yes** — covered (ADR 0024) |
| `model-picker.ts:69` / `model-hub.ts:173` / `model-browser.ts:349` | bottom-anchored picker / fullscreen `/models` hub / reusable browser | `/model` via `command.rs:819` + `select_list.rs` | **yes** — covered; the *browser + hub split* is a pattern, not a need |
| `session-selector.ts:750` `SessionSelectorComponent` | session resume/fork picker with ranked search | `picker.rs:45` `Picker` | **yes** — covered |
| `theme-selector.ts:10`, `thinking-selector.ts:11` | theme / thinking SelectLists | `/theme`, `/thinking` via `command.rs` + `select_list.rs` | **yes** — covered |
| `settings-selector.ts:540` `SettingsSelectorComponent` | tabbed settings editor with a live status-line preview | `none` (config is a file) | **no** — radian's settings are files/commands (ADR 0008) |
| `history-search.ts:151` `HistorySearchComponent` | ctrl+r prompt-history search | `find.rs:27` `FindState` (in-transcript, not prompt history) | **later** — prompt-history search is a real, small gap |
| `copy-selector.ts:53` `CopySelectorComponent` | fullscreen `/copy` tree + preview | `select.rs` + `clip.rs:53` `osc52` (selection copy) | **later** — the *tree of copy targets* is richer than radian's selection |
| `plan-review-overlay.ts:160` + `plan-toc.ts:16` | fullscreen plan review with ToC/annotations | `none` | **later** — radian has no plan-review step |
| `tree-selector.ts:984`, `plugin-selector.ts:24`, `login-dialog.ts:11`, `move-overlay.ts:155` | session tree, plugin marketplace, OAuth login, `/move` overlay | `picker.rs`; `login.rs:53` `LoginWizard`; `workspace.rs` | **yes/later** — login is covered by a wizard; tree/marketplace are not needed |
| `extensions/extension-dashboard.ts:66` + `extension-list.ts:35` + `inspector-panel.ts:13` | the `/extensions` control center | `none` | **no** — no dynamic plugin UI (ADR 0023) |
| `move-overlay.ts:155` `MoveOverlay` | `/move` path input with directory autocomplete | `mention.rs` (file completion) | **no** — radian's cwd is fixed per session |

### 4.6 Menus / suggestions (dsh §5.6)

| omp component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `tui/src/autocomplete.ts:401` `CombinedAutocompleteProvider` | slash + file/`@` suggestions | `command.rs` + `mention.rs` + `select_list.rs` | **yes** — covered |
| `pca/src/modes/prompt-action-autocomplete.ts`, `emoji-autocomplete.ts`, `github-ref-autocomplete.ts`, `internal-url-autocomplete.ts` | extra autocomplete sources | `none` | **later** — emoji/github-ref are niceties |
| `pca/src/modes/components/keybinding-hints.ts:30` `keyHint` | the shared dim-key/muted-description formatter | `panel.rs:23` `Hint` | **yes** — radian has the hint primitive; keep it one function |

### 4.7 Status bar / footer (dsh §5.7)

| omp component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `status-line/component.ts:293` `StatusLineComponent` | the status line, rendered into the editor's top border | `footer.rs:320` `Footer` + `render.rs` status row | **yes** — covered; `root`'s `bar` status row is the analogue |
| `status-line/segments.ts:689-714` `SEGMENTS` | 24 named segments | `footer.rs:752` `item_placeholders` | **yes** — covered |
| `status-line/presets.ts:3-101` | 7 presets | `footer.rs:165` `Preset` (deck/full/pi/classic) | **yes** — covered |
| `status-line/separators.ts:8` | 7 separator styles | `footer.rs` separator/separator_style | **yes** — covered (ADR 0027 C2) |
| `status-line/context-thresholds.ts:31` | context % color thresholds (normal/warning/purple/error) | `footer.rs` context thresholds (ADR 0027 C2) | **yes** — covered; the *4-level* split is a small add |
| `footer.ts:16` `FooterComponent` | a second footer (pwd/branch/tokens/cost) | `footer.rs` | **yes** — covered |
| `component.ts:1866` `getTopBorder` | status embedded in the editor's top border | `render.rs` bar status row above the input | **yes** — radian's `bar` status row is the analogue (ADR 0035 decision 1) |

### 4.8 Notifications / toasts (dsh §5.8)

| omp component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `pca/src/modes/components/ttsr-notification.ts:15` | a rule-violation notification block | `none` | **no** — omp-specific |
| `extensibility/extensions/types.ts:277` `notify` + `tui/src/desktop-notify.ts:34` `DesktopNotifier` | extension notify → Linux D-Bus toast | `none` | **later** — take the *gating*, off by default (ADR 0013) |
| `pca/src/modes/components/error-banner.ts:16` | a pinned error banner | `render.rs` notices | **later** — the banner shape |
| (dsh §5.8 `task-completion` gating) | fire only when idle, not working, no pending, no goal, blurred | ADR 0037 background-completion notice | **yes** — take the gating logic |

### 4.9 Scrolling, selection, copy (dsh §5.9)

| omp component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `tui/src/components/scroll-view.ts:55` `ScrollView` | a fixed-height viewport over pre-rendered lines + optional scrollbar | `render.rs` scroll + `select_list.rs` list scrollbar | **yes** — radian has both; a **transcript** scrollbar is the gap (later) |
| `tui/src/components/select-list.ts:86` `SelectList` | fuzzy/type-to-filter list with a hosted `ScrollView` | `select_list.rs:90` `ListView` | **yes** — covered (ADR 0035) |
| `copy-selector.ts:53` + `kill-ring.ts:10` | copy targets / kill ring | `select.rs:295` `logical_text` + `clip.rs` | **later** — see §4.5 |
| `tui/src/tui.ts:301` stable-prefix / `:215` live region | differential render seams that make scrolling cheap | `render.rs` line cache (ADR 0023 U9) | **yes** — radian's width-keyed line cache is the analogue |

### 4.10 Mouse (dsh §5.10)

| omp component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `tui/src/mouse.ts:34` `parseSgrMouse`, `:56` `routeSgrMouseInput` | SGR mouse parse + dispatch (wheel/motion/click/release) | `terminal.rs:81` `write_mouse` + `select.rs:75` `locate` | **later** — radian handles selection; wheel/click targets are the gap |
| `tui/src/mouse.ts:102` `MouseRoutable` + `select-list-mouse-routing.ts:11` | route mouse to a list with a top-border offset | `select_list.rs` (no mouse) | **later** — list hover/click |
| `tui/src/tui.ts:94` | mouse tracking is only on while a fullscreen overlay holds the alt screen | `terminal.rs` enables mouse globally | **no** — radian's full-screen model differs |

### 4.11 Themes (dsh §5.11)

| omp component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `pca/src/modes/theme/theme.ts:80` + `theme-class.ts:131` | global theme singleton + the class components call | `theme.rs:425` `Theme` | **yes** — covered (ADR 0029) |
| `pca/src/modes/theme/tui-adapters.ts:139` | adapts `Theme` to component themes (`getSymbolTheme`, `getMarkdownTheme`, …) | `theme.rs` role resolution | **yes** — covered |
| `pca/src/modes/theme/symbols.ts:5` (270 keys × 3 presets) | the symbol table | `footer.rs:34` `Glyphs` (footer only) | **later** — a scoped `symbols.rs` was recommended in `2026-10-14-omp-design.md` §9.1 row 3; still relevant |
| `pca/src/modes/theme/defaults/` (99 JSONs) | bundled themes | `theme/file.rs` (pi-compatible, ADR 0029) | **yes** — covered |

### 4.12 Animations (dsh §5.12)

| omp component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `tui/src/components/loader.ts:20` `Loader` | spinner + message, adaptive 30 fps tick, direct-write | `spinner.rs:87` `frame` + `render.rs` status row | **yes** — covered (`Working…`, ADR 0027/10) |
| `pca/src/modes/theme/shimmer.ts:303` `shimmerText` | animated gradient text | `none` | **no** — needs sub-frame timing |
| `pca/src/modes/controllers/streaming-reveal.ts:6` | smooth streaming reveal (grapheme stepping) | `render.rs` streaming accumulation | **no** — radian draws whole lines; ADR 0023 idle = zero redraw |
| `tui/src/components/tab-bar.ts` tab animation | tab hover/active states | `none` | **no** — no tabs |

### 4.13 Layout and resizing (dsh §5.13)

| omp component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `tui/src/components/box.ts:38` `Box` | padding/background/border container | `panel.rs` frame + `blocks.rs` `Pad` | **yes** — covered |
| `pca/src/modes/interactive-mode.ts:1076-1092` | the main vertical stack | `render.rs:1405` `draw` (fixed layout) | **yes** — covered |
| `tui/src/tui.ts:2005` `showOverlay` + `:381` `OverlayAnchor` | anchored overlays (9 anchors), compositing | `render.rs` overlay slots | **yes** — radian's overlay layers are the analogue |
| **no sidebar/split in the app view** | omp is single-column + overlays | radian single-column (ADR 0023) | — the §4.14 hub is omp's multi-view answer |
| `tui/src/tui.ts:4958` `#resizeRepaintsInPlace` | resize that repaints in place vs clear | `events.rs` `Resize` → `App` | **yes** — covered |

### 4.14 Several sessions / subagents / tasks (dsh §5.15)

| omp component (file:line) | What it does | radian equivalent | Recommend |
|---|---|---|---|
| `pca/src/modes/session-observer-registry.ts:37` `SessionObserverRegistry` | tracks main + all subagents, sorts main-first then by group | `none` (ADR 0030 slice 2 observer is future) | **yes** — the roster data model to copy |
| `pca/src/modes/interactive-mode.ts:465-509` `renderSubagentHudLines` | a HUD of up to 8 running subagents in the main view | `none` | **later** — a HUD vs a panel |
| `pca/src/modes/components/agent-hub.ts:147` `AgentHubOverlayComponent` | one overlay, two views: roster/tree table + per-agent chat | `none` | **yes** — the model for a `/tasks` panel |
| `agent-hub-projection.ts:162` `projectAgentTree`, `agent-hub-renderer.ts:13` | pure data→roster + pure string rendering | `none` | **yes** — copy the **pure projection + pure render** split (matches radian's architecture) |
| `agent-transcript-viewer.ts:140` `AgentTranscriptViewer` | fullscreen read-only per-agent transcript (tails JSONL) | `none` | **yes** — this is exactly radian's ADR 0030 slice-11 detail view |
| `agents-hub.ts:204` `AgentsHubComponent` | fullscreen `/agents` hub (sidebar + list + property strips) | `none` | **later** — richer than V1 needs |
| `running-subagent-badge.ts:7` | counts running subagents (status badge) | `none` | **yes** — cheap; the same idea as dsh's counts row |
| `background-tan-message.ts:21` | a one-line dispatch breadcrumb in the transcript | `none` | **later** |
| `pca/src/modes/components/session-selector.ts:750` | session picker | `picker.rs:45` `Picker` | **yes** — covered |
| opencode `session-tabs.tsx:506` (see §2.13) | several sessions as tabs | `none` | **later** — tabs vs a panel is an decision (§6) |

---

## 5. What radian already has, and the gaps that matter most

The earlier `docs/research/2026-10-14-omp-design.md` §9 said several ADR 0028/0035 items were
**not implemented on `main` at `af49086`**. **That is stale for this worktree (`19bbd0b`).**
Re-checked directly here:

### 5.1 What radian has today (module by module)

- **Chrome primitives** — `panel.rs` (ADR 0035): `Panel` with `row`/`divider`/
  `right_title`/`hint` and `lines()` (`panel.rs:48-93`); already used by the login wizard,
  project-trust, `/access`, `/logout`, the approval box, the ask panel and the picker
  (`render.rs:760,864,905,937,1593,1981`; `picker.rs:255`). `select_list.rs` (ADR 0035):
  `Item` (`:16`), `ListView` (`:90`), `view()` (`:109`), scroll window + scrollbar. `spinner.rs`:
  `Kind::{Pulse,Arc,Quadrant,Braille}` with a **time**-driven `frame` (`spinner.rs:15,87`;
  pulse default, ADR 0027).
- **Transcript** — `blocks.rs`: `Block` (`:57`), `Card`/`Frame`/`Pad` (`:263,283,304`),
  `BodyLine` (`:324`), `block_gap` (`:763`), `gaps` (`:814`), `is_call_result_pair` (`:802`),
  reasoning fold, mermaid, tables. `tools.rs`: `ToolCard` (`:32`), `ToolFold` (`:68`),
  `renderer_for` per tool (`:221`), `diff_role` (`:320`), `ToolDisplay` (`:780`).
- **Markdown** — `markdown.rs` (pulldown-cmark), `highlight.rs` (syntect); `theme.rs` has the
  Markdown roles incl. `md_heading_mark` (`theme.rs:497,687`; ADR 0035) and task-list
  checkboxes (`blocks.rs:384`).
- **Input** — `composer.rs` (`Composer` `:15`, `rows` `:308`, draft stash `:331-375`);
  `render.rs` `InputStyle::{Bar(default),Lines,Box}` (`:709-729`, ADR 0035 V3 `bar` done).
- **Dialogs / pickers** — `picker.rs` (sessions), `question.rs` (`QuestionForm` `:39`),
  `login.rs` (`LoginWizard` `:53`), trust/access/logout panels in `render.rs`, plus
  `select_list`-driven `/model`, `/theme`, `/thinking`, `/skills`, completion.
- **Menus / completion** — `command.rs` (`Command` tree `:197`, `SlashCompletion` `:550`,
  `fuzzy_score` `:757`), `mention.rs` (`Attachment` `:54`, `complete` `:367`).
- **Footer / status** — `footer.rs` (`Footer` `:320`, `lines` `:433`, `Preset` deck/full/pi/
  classic `:165`, `Glyphs` `:34`, `widget_line` contributor slot `:1636`; ADR 0027 revs 1–3).
- **Selection / copy / search** — `select.rs` (`Selection` `:49`, `locate` `:75`, `run_at`
  `:245`, `logical_text` `:295`, `highlight_rows` `:335`); `clip.rs` (OSC 52, `:18,53`);
  `find.rs` (`FindState` `:27`, `compute` `:128`).
- **Terminal / host plumbing** — `terminal.rs` (`TtyGuard` `:179`, mouse `:81`, panic hook
  `:387`), `signals.rs`, `events.rs` (`UiEvent` `:24`, `input_event` `:90`), `timing.rs`
  (`TurnTimer` `:14`), `theme.rs` (`Role` `:128`, `Theme` `:425`), `workspace.rs`,
  `profile.rs` (`Tier` `:21`, `build_access` `:107`), `startup.rs` (`StartupScreen` `:52`).

So the "shareable primitives" the owner wants **partly exist**: `panel.rs` and
`select_list.rs` are the first two (ADR 0035 revs 1–2), and `spinner.rs`/`timing.rs`/
`clip.rs`/`select.rs` are shared utilities. The work is to **grow this layer**, not start it.

### 5.2 The gaps that matter most (ranked)

**1. No dockable panel host, no layout (sidebar / split / pane).** Today a "panel" is a
full-width modal overlay via `panel.rs` (ADR 0023 U8 supplement: "先做浮层"). There is no
`PanelHost`/layout tree, so nothing can be *docked* or shown *beside* the transcript, and no
second view can persist while the user types. opencode has exactly this
(`session-frame.tsx:29`, `context/panel.tsx:15-42`, `ui/layout.ts`, `pane-resize.ts`) and omp
gets a similar effect with overlays plus a HUD (`interactive-mode.ts:1076-1092`,
`:465-509`). This one gap gates the orchestration view, a context viewer, a diff panel, and
"panels are shareable" generally. It is already scheduled (ADR 0023 U8, ADR 0030 slice 11),
but it is the load-bearing piece.

**2. No multi-session / subagent / task view.** radian has no roster, no per-task detail, no
session switching in the TUI. opencode has session tabs (`session-tabs.tsx:506`), a
composer subagents tab (`subagents-tab.tsx:25`), and a subagent inspector
(`mini/footer.subagent.tsx`); omp has the observer registry (`session-observer-registry.ts:37`),
the agent-hub overlay (`agent-hub.ts:147`), and the read-only agent transcript viewer
(`agent-transcript-viewer.ts:140`). The pieces radian should copy are the **pure
projection + pure render** split (`agent-hub-projection.ts:162`,
`agent-hub-renderer.ts:13`) and the **read-only detail over committed facts** — which is
exactly ADR 0030's slice-2 observer. This is the owner's second stated goal and the highest
user-visible payoff.

**3. Notifications are one transient note; there is no toast surface and no gated
terminal notification.** opencode has a queued toast surface (`ui/toast.tsx:203`) plus an
attention/notification service gated on `blurred` (`context/attention.tsx:11-14`,
`feature-plugins/system/notifications.ts:8-52`); dsh has `task-completion`
gating (dsh §1.5) and `terminal-notification.ts`. radian has a transient note
(`render.rs:1325`) and ADR 0037's background-completion notice, but no queue, no levels, and
no optional OSC/BEL path. Two of the three reference TUIs independently built this; it is a
small, high-polish gap.

**Lower, but real:** (a) a `/help` / command-palette overlay (opencode
`dialog-help.tsx:5`, `command-palette.tsx:14`; cheap once `panel.rs` exists); (b) a
**transcript** scrollbar (lists have one in `select_list.rs:156`, the transcript does not —
dsh §5.9 reached the same gap); (c) prompt-history search / copy tree (omp
`history-search.ts:151`, `copy-selector.ts:53`); (d) collapsible transcript **groups**
(opencode `grouping/tree.ts:18`); (e) a diff **panel** vs the inline card
(opencode `diff-viewer.tsx:306`).

**Explicitly not gaps** (already decided/implemented, do not redo): block spacing and the
tool card (ADR 0028 revs 1–3 done in `blocks.rs`/`tools.rs`), the `bar` input style
(ADR 0035 V3 done in `render.rs`), `md_heading_mark` and task lists (ADR 0035 done),
the spinner (ADR 0027 revs 2/9/10 done in `spinner.rs`), and the footer presets/contributor
slot (ADR 0027 revs 1–3 done in `footer.rs`).

---

## 6. Open questions for the owner (with a recommendation each)

**Q1. How far to formalize the shared component layer?** Build a `radian-tui::chrome` module
holding the missing primitives as pure functions, or keep adding them next to their screens?
**Recommend:** keep growing the existing modules (`panel.rs`, `select_list.rs`, …) into one
**`pub(crate) chrome` module** of pure functions — a `StatusIcon` (state → glyph+role, with the
ASCII fallback that omp's `formatStatusIcon` and dsh's `StatusIcon.tsx:24` both have), a
`Badge`, a `KeyValueRow`, a `Divider`, and a shared `HintLine`. **No trait, no registry**
(ADR 0023 forbids dynamic UI). This continues ADR 0035 revs 1–2 rather than restarting them.

**Q2. Do we build the dockable panel host + layout now, or keep overlays?** Overlays only, or
an ADR 0023 U8 layout tree with sidebars/splits?
**Recommend:** **overlays first, then the `PanelHost` as a thin abstraction over the existing
layer stack**, deferring the split/sidebar tree to ADR 0030 slice 11. Copy opencode's **policy**
(a panel whose min width does not fit falls back to fullscreen, `context/panel.tsx:15-16`) and
its **resize behaviour** (parent-bound drag, double-click reset, `ui/pane-resize.ts:49-60`),
not its machinery.

**Q3. Do we copy the orchestration view?** A roster panel + a read-only detail view + switching,
or wait for ADR 0030's observer?
**Recommend:** **yes, but data-driven by the slice-2 observer** (ADR 0030 R5). Copy omp's
**pure projection + pure render** split and its **read-only viewer over committed facts**
(`agent-hub-projection.ts:162`, `agent-transcript-viewer.ts:140`); copy opencode/omp's
**waiting/elapsed/token/tool** information set and dsh's **counts row**; keep radian's "never
guess the parent" rule. Do not block on the fact tail (dsh §6 Q4 reached the same conclusion).

**Q4. Do we add a toast/notice surface, and terminal notifications?**
**Recommend:** **yes to a small toast surface** (levels + TTL + queue, on top of the existing
transient note) because two of three reference TUIs built one; **terminal notifications stay
off by default**, gated on a config key, reusing the `blurred`/idle gating (ADR 0013).

**Q5. Which multi-session UI: tabs, a sidebar, or a panel?** opencode uses tabs + sidebar;
omp uses an overlay hub.
**Recommend:** start with the **overlay panel** (`/tasks`) and a **footer item**, per ADR 0023
U8 and the dsh study's recommendation; reserve tabs/sidebar for ADR 0030 slice 11. Tabs and a
panel are both defensible — this is the one place the two references disagree, so it is the
owner's call.

**Q6. Do we add a command palette / `/help` overlay?** **Recommend:** **later** — it is cheap
once `panel.rs` + `select_list.rs` are general, but it is not load-bearing. When built, use
opencode's `DialogHelp` (`ui/dialog-help.tsx:5`) as the shape.

**Q7. A transcript scrollbar?** **Recommend:** **later** — promote `select_list.rs`'s thumb
(`:156`) to a shared `ScrollView`-style helper used by both, rather than a transcript-only bar.
(dsh §5.9 recommended the same.)

**Q8. Do we copy composer tabs / a shell-input mode?** **Recommend:** **no tabs** — radian's
`!`/`!!` (ADR 0033) is the radian shape and covers the need; the opencode `shell-tab` /
`TerminalsTab` are out of scope.

**Q9. Do we adopt animations beyond the spinner?** **Recommend:** **no** — shimmer/tween/
streaming-reveal all need sub-frame timing, and radian's deliberate "idle = zero redraw"
(ADR 0023 U2) is the better default. The time-driven `spinner.rs` is the right amount.

**Q10. Which omp version do we model?** The ADRs were written against 18.10.4
(`2026-10-14-omp-blocks.md:11-30`) but the installed packages are 17.3.8, and
`panel.rs:10-12` cites an 18.10.4 path (`tui/src/chrome/overlay-box.ts:218-350`) that does not
exist here.
**Recommend:** **do not chase a version.** The already-decided behaviours (composer `bar`,
container-owned spacing, the panel shape) are implemented and correct regardless of which omp
release inspired them. **Fix the two stale citations**: update `panel.rs:10-12` and the ADR
references so they name the omp version they mean, and note that the composer-shape registry
and `ToolCard` are 18.10.4-only.

---

## Appendix: provenance and files read

- **opencode 2.0.30**, TUI `@opencode/tui` 2.0.30, at
  `/private/tmp/.../opencode-src/packages`. Read: `tui/src/{app.tsx,index.tsx,logo.ts}`;
  `tui/src/context/{theme,keymap,panel,interactivity,route,clipboard,attention,panel,…}.tsx`;
  `tui/src/ui/*`; `tui/src/component/*` and `component/prompt/*`;
  `tui/src/routes/session/*` and `composer/*` and `grouping/*`; `tui/src/plugin/*`;
  `tui/src/feature-plugins/*`; `tui/src/mini/*`; `tui/src/theme/*` and `theme/assets/*.json`;
  `plugin/src/tui/*`; `theme/src/tui/*`. Not read: the `@opentui/*` implementation (absent).
- **omp 17.3.8**: `@oh-my-pi/pi-tui` and `@oh-my-pi/pi-coding-agent` under
  `<home>/.bun/install/global/node_modules/@oh-my-pi/`. Read: `tui/src/tui.ts`,
  `tui/src/{keys,keybindings,autocomplete,mouse,stdin-buffer,bracketed-paste,kill-ring,
  editor-component,terminal,terminal-capabilities,symbols,fuzzy,utils,kitty-graphics,
  deccara,desktop-notify,tmux,latex-block,latex-to-unicode,loop-watchdog,ttyid}.ts` and
  `tui/src/components/*`; `pca/src/modes/{interactive-mode,types,queue-input,markdown-prose,
  session-observer-registry,running-subagent-badge}.ts`; `pca/src/modes/components/*` (the
  transcript, status-line, selectors, agent hub, extensions); `pca/src/modes/controllers/*`;
  `pca/src/modes/theme/*` and `defaults/`; `pca/src/tui/*`; `pca/src/config/keybindings.ts`;
  `pca/src/extensibility/extensions/types.ts`.
- **radian**: the `crates/radian-tui/src` modules and `crates/radian-tui/README.md`; ADRs
  0023, 0024, 0027, 0028, 0029, 0030, 0033, 0035; and the four
  `docs/research/2026-10-14-omp-*.md` documents plus
  `docs/research/2026-10-16-dsh-tui-study.md`.
- **Version note (load-bearing):** the earlier omp research and the radian ADRs describe
  **18.10.4**; the installed packages are **17.3.8** and differ structurally (`chrome/`,
  `tui/src/chat/`, `tui/src/render/`, the composer-shape registry, and the `ToolCard`
  abstraction are 18.10.4-only). Claims in this document are against **17.3.8** unless the
  text says 18.10.4 or cites a `2026-10-14-omp-*.md`.
- **Not verified:** opencode's `@opentui/*` internals; opencode's tests/CI; omp's tests/CI;
  running either TUI; the `OverlayPanel` symbol (absent in 17.3.8).
