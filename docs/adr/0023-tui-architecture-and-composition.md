# ADR 0023: TUI Architecture and Composition

Status: accepted

## Decisions

### U1 Four logical layers

| Layer | Owns | Never owns |
|---|---|---|
| TUI Foundation (`radian-tui`) | Event loop, Action and Effect, layer stack, panels and layout, themes and keys | Agent lifecycle, session-fact authority |
| Host Interface (`radian-host`, observers in `radian-session`) | Session Facade, read-only projections, command senders, pending-request queue | UI layout |
| Runtime (`radian-runtime`) | Inbox, driver, journal, tool sessions, approval routing | TUI components |
| Workspace Extension (optional) | Panel and workspace-view composition | Core and runtime implementation |

TUI **never owns** agent session lifecycles. The driver belongs to runtime. TUI only holds a Facade.

### U2 Event-driven loop

The UI thread is synchronous (terminal, `TtyGuard`, panic path unchanged). It blocks on **one** `UiEvent` channel. Events fall in four classes by source:

| Class | Content | Promise |
|---|---|---|
| Facts and control | `TranscriptAppended`, `ToolStarted` and `Finished`, `RunFinished`, runtime events, approval and question requests, command results | **Never lost**: unbounded `std::sync::mpsc`, producers never block (ADR 0010 K3) |
| Dirty notices | "some session projection changed" | **Mergeable and droppable**. Each projection carries an `AtomicBool` pending flag and a monotonic `AtomicU64` revision. A producer **first** writes the projection and bumps revision, **then** `swap(true)`. It sends `UiEvent::Dirty` only when the flag was `false`. The UI **first** `swap(false)` (Acquire) **then** reads the projection (Release and lock) |
| Terminal input | Keys, paste, resize, terminal gone | Ordered; the input thread forwards. Consecutive resizes merge into the last one. The input thread keeps `poll(2)` hangup detection and never blocks directly in `event::read` |
| Process and host state | Signals, driver-task ends, command results | All are events. No source ever polls |

- Streaming deltas **never enter the fact channel**: `EventSink` appends `ModelTextDelta` and `ModelReasoningDelta` into that session projection streaming buffer, then sends a dirty notice.
- Each loop turn handles at most N fact events (N comes from benchmarks), then draws one frame when dirty. **Idle means zero redraws**: no events and no live timed display means zero redraws. A time-varying display wakes only with `recv_timeout` when needed. Timed displays have a roster: Ctrl-C quit hint, selection-drag auto-scroll, spinner breath, background-job `jobs` age. They wake only while active (`jobs` age shortens the wait only when jobs run and the footer shows that item: it takes the smaller of 250 ms and the next age-text change; only a text change redraws). Stopped means back to the 250 ms check beat.
- The input thread checks stdin every 250 ms with non-consuming `poll(2)`. Terminal loss exits cleanly through `TerminalGone`. The input thread never blocks exit, even when stuck inside crossterm.

### U3 One-way flow of Action and Effect

```text
input ──key table──▶ Action ─┐
                        ├─▶ App::update(…) ─▶ UpdateResult { dirty, effects } ─▶ loop runs effects ─▶ new events
host events ───────────────┘
```

- `Action` is **interaction intent** and the only state-change entry. Host events are another input. They never replace `Action`.
- `update` is pure: it touches no runtime and does no I/O. External actions (send message, steer, cancel, answer request, run `/compact`, run `/export`) return as `Effect`. The loop runs them.
- Most behavior tests are therefore pure `update` tests. Pty tests stay only for terminal state, real key protocols, and recovery paths.

### U4 Layer stack

One `Vec<Layer>`, each layer marking a level:

| Level | Examples | Input rule |
|---|---|---|
| Modal | Approval, structured questions, `/memory` confirm | Must resolve explicitly before it disappears. Always stays **above** Overlay in the stack |
| Overlay | Search box, `@` completion list, session selector, inspector (`jobs \| tools \| mcp \| hooks`) | Temporary float. **Never** pushed above any Modal |
| Surface | Transcript plus composer, workspace pages | Long-lived. Receives inputs unconsumed above |

Invariants: (1) Ctrl-C runs before the whole stack (cancels when running, exits when idle; pending approvals then resolve as rejected). (2) Approval and confirm keys `y`, `a`, `n`, and `Esc` **never rebind**; form-layer keys rebind. `picker` merges into the stack.

### U5 Session Facade is a facade, not an owner

```rust
trait SessionFacade {
    fn id(&self) -> &SessionId;
    fn capabilities(&self) -> &[SessionCapability];   // Observe, SendMessage, Steer, Cancel, AnswerRequest
    fn projection(&self) -> ProjectionRead<'_>;        // read-only; with revision
    fn command(&self, cmd: SessionCommand) -> Result<(), CommandRefused>;
}
```

- The UI holds several `Arc<dyn SessionFacade>` (default coding mode holds exactly one). Inbox, driver, journal, tool sessions, and approval routing belong to runtime and host.
- `SessionProjection` moves out of `App`: turn state, usage, budget, tier, MCP counts, streaming accum buffers, and recent tool calls. `App` keeps only UI state.
- The Facade **declares** capabilities, and the host **rechecks** on command execution. The UI never judges by implementation type what it can do.
- Two implementations exist: (a) in-process, built by `radian-host::open_session`; (b) **read-only JSONL observer** (U6). No remote protocol is designed.

### U6 Read-only observer

When observing another process session: only `Observe` capability exists. No writer lock, no inbox, no send, steer, or cancel exists. **It renders only committed facts**: ADR 0019 pending segments read as "uncommitted" and hide. It never derives live sandbox or approval state from another process JSONL. It reuses the `radian-session` validating reader and reads **incrementally** by byte offset (`JournalTail`): it saves `TranscriptFold` state, last `seq`, and byte offset, then continues from the offset next time. Pending-segment state lives inside too. A file-identity change (inode or length regress) triggers re-read. An incomplete trailing line reads as torn tail. One reader exists per observer; reads merge then send a dirty notice.

### U7 Pending-request queue lives at the host

Approval and structured questions share one arrival-order queue. Elements carry `origin: SessionId`. Completion still uses oneshot (the permission-layer race, the UI-exit "abandoned", and silent late-answer drops stay unchanged). The queue gives stable order and source, and completes all its requests with abort results when its session closes. The UI shows only the head. The status bar shows the pending count. Approval fail-closed semantics (ADR 0012) stay unchanged by one word.

### U8 Panels and layout tree; only built-in compile-time panels

- `Panel` trait: `id`, `title`, `min_size`, `update(&projections, event)`, `handle(Action) -> Handled|Pass`, `draw(frame, area, theme)`. The registry is `Vec<Box<dyn PanelFactory>>`, registered by the host at compile time. **No** third-party dynamic UI plugins exist. No JS, Wasm, or dynamic libraries exist.
- Layout is a small tree: `Leaf(PanelId) | Split { dir, ratio, children } | Tabs { children, active }`. Focus is one path in the tree. Resize recomputes from the tree. Narrow terminals downgrade `Split` to `Tabs`. Implementation must handle minimum sizes, illegal ratios, empty nodes, and focus restore.
- **Default coding TUI is fixed single column** and reads no layout configuration. Complex layout lives only in the optional extension, and the extension saves its own layout. Layout owns no business data. Invisible panels cost zero: panels outside the layout never `update` and never `draw`.
- The first panel shape is **float**: one command or key (`/panel <name>`). The panel covers the transcript fullscreen. Esc closes it. One shows at a time. It uses the existing layer stack. First panels are Context, Tools and MCP, Git, Usage, and Timeline, all with real data.
- Panels, sessions, and optional workspace views share one "switch view" key and `/` command family. They never build separate ones.

### U9 Transcript component and configuration

- Row cache keys by width. A width change never remeasures all synchronously: the old total serves as estimate, and per-frame time slices remeasure. During remeasure the status-bar `N/M` marks as approximate.
- The scroll anchor is `(entry_index, offset_within_entry)`. Relayout keeps visible-message identity and relative position. Bottom-follow keeps the bottom anchor. `TranscriptReplaced` rebuilds entries and invalidates anchors: follow-state returns to bottom; non-follow-state returns to top with a notice.
- Configuration is **one file** `~/.radian/config/config.toml` under `[tui]` (ADR 0008 and ADR 0020). Display policy, footer items, and theme files all live under it. The theme is the only color source. **No** OSC 11 background-detection Auto theme exists (a terminal-answered capability is untestable under pty).

### U10 `/new` starts a new session in place

- `run()` runs one session per iteration: preamble and picker run once. Each iteration does resolve, open, wire, screen, loop, then teardown. `/new` is one loop-leave way (with quit as `LoopEnd`), not an effect: `update` sets a flag and executes at once, and the loop exits directly. Reject conditions: in-turn (same as `/fork`), session ended (same as `/fork`), running background jobs (notice points at `/jobs kill`). After rejection the session never changes.
- The old session ends cleanly: stops the driver and joins (same failure semantics as quit), saves the draft (by old session id), and releases the writer lock. The terminal **never** restores; guard and terminal reuse across iterations. Old-session background producers (git, usage, footer command) and the queue forwarder abort handles at teardown. They never leak into the new-session channel.
- The new session rebuilds `SessionView`: transcript, footer counts, input history, `layers`, completion, `contributions`, and `/loop` repeat state all reset. Global chrome (theme, keys) reapplies from the same config and stays. `SessionCreated.config_snapshot` is the effective startup routing (resolved provider and model, plus `access`, same as `radian exec`). Carried values backfill with `ConfigChanged` to disk, and the journal is the next-resume authority.
- Carried values are last-effective values, not configuration defaults: model and provider route (a queued switch beats the applied one, meaning latest user intent), thinking (same), access tier (wins the remembered bit: beats the CLI flag; project narrowing still applies), auto (equals explicit `--yes`, backfills).
- Process and host-state events carry session ownership (`DriverStopped{session_id}`): one channel serves two sessions in order. An old-session graceful close never reads as a new-session end. The loop ignores events outside this session.

### U11 `/btw` side questions

- `/btw <question>` is one **extra** model request. It only answers the current context and never enters the transcript: the host takes the next formal-request shape (the same shape as driver-issued system prompt, effective history, tool definitions, and `settings`, byte-identical, from the same shape function; `/context` also renders through it). It appends one user message at the end (the question plus one fixed note: answer only from current context, use no tools). It sends through the session current model route and thinking level (`/model` switch moves side requests and the next formal round to the same route; credential resolution follows the same path with no extra unredacted channel). The same prefix keeps prompt-cache hits (ADR 0018).
- It never runs tools: when the model still returns tool calls, they never run. The answer end adds one line "no tools ran". The permission layer never engages because no tools run.
- On screen only: the answer streams in one Overlay layer (same level as session selector and inspector, U4). Esc cancels the in-flight request or closes the overlay. No transcript, no journal, and no durable events exist (ADR 0010 K1 live). Each question stands alone and never replays prior side questions. This process keeps completed side answers for overlay review; exit drops them.
- Concurrency: `/btw` works while a turn runs, and the main turn stays unaffected. Only one side request runs at a time. A second `/btw` refuses with a notice. `/btw` without a question reopens the latest answer.
- Usage: the overlay last row shows this-request tokens and cost (at call-time route price). It **never** counts into session totals and **never** writes a `ModelUsage` record (ADR 0007 J5 requires records immediately after assistant messages, and none exists here). Side-request errors show only in the overlay and never trigger the ADR 0017 turn-failure path.
- Only TUI has it. `radian exec` has none.

## Frozen List

1. Default coding TUI is single column and reads no layout configuration.
2. Only built-in compile-time panels exist. No third-party dynamic UI plugins exist.
3. Workspace is an optional extension and never enters base-TUI business layers.
4. Layout owns no business data.
5. TUI never owns agent session lifecycles.
6. Dirty notices merge and drop; fact events never drop; `RunFinished` always arrives.
7. Observers only read and only see committed facts.
8. Input thread keeps `poll(2)` hangup detection and signal fallback. Terminal loss is one `UiEvent`, never a hang.

## No Changes to `radian-core`

Each item already depends on Core facts: `EventSink::emit` is synchronous and non-blocking, the full `AgentEvent` set, `ModelTextDelta` and `ModelReasoningDelta` are only events, usage, tool state, and `RunFinished` are all events, `TranscriptAppended` is ordered, ADR 0019 segment rules live in the `radian-session` reader, and approval lives in `radian-permissions` hooks. When implementation finds a required Core change, that need is a new decision under this ADR.

## Constraints and Costs

- The fact channel is unbounded: turn structure gives the memory bound. A runaway tool puts `ToolProgress` in dirty notices, not the fact channel.
- During time-sliced remeasure `N/M` is an estimate.
- An observer never sees another side pending segments, so "the other side compacts" stays invisible until commit.
- Idle costs one `poll(2)` syscall per 250 ms with usually no frame. With running jobs and a footer `jobs` item, only an age-text change draws one frame.

## Verification

`tests/update.rs` (each effect and result input), `tests/input.rs`, `tests/command.rs`, `tests/approval.rs`, `tests/btw.rs` (`/btw` effect, reject, Esc, reopen, streaming and completion inputs), `tests/purity.rs` (`app.rs` holds no `std::fs`, `MemoryService`, `tokio`, or `mention::expand`). `projection.rs` order unit tests. `tests/virtual.rs` window tests. Pty tests cover idle zero-redraw, signals, flood draw fairness, terminal loss and orphans, `--block-input` and `--freeze-draw`, `/btw` overlay end to end (request-body prefix sameness, session file with no side questions). `/new` coverage: `command.rs` parse and HELP tests, `tests/update.rs` accept and reject tests, pty one-round, `/new`, one-round tests (two session files, second `SessionCreated` carrying route and levels, `ConfigChanged` backfill, `Session::history` replay, `/sessions` listing both) plus reject and lock-release tests. `tests/perf_render.rs` and `benchmarks/perf-baseline.md` record P95 and long-list metrics.
