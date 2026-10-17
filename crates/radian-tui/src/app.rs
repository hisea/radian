//! The UI state and the one function that changes it: [`App::update`] (ADR 0023 U3).
//!
//! `update` is **pure**: it never does I/O, never touches the runtime, never reads a file or a
//! directory, and never calls into a service. Everything that needs the outside world comes
//! back as an [`Effect`], which the event loop in `cli.rs` runs; the result returns as another
//! [`Input`]. `tests/purity.rs` asserts the structure (no `std::fs`, no `MemoryService`, no
//! `tokio`, no `mention::expand`/`complete` call in this file).
//!
//! A key press is resolved to an [`Input`] by [`App::key_input`] (the keymap plus the current
//! layer: approval prompt, `/memory` confirmation, find box), and [`App::on_key`] is the thin
//! wrapper that resolves and updates.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::{AgentEvent, BudgetReport, CompactionOutcome, ContentPart, Message, UsageSource};
use radian_coding::PromptError;
use radian_host::memory::commands::MemoryCommand;
use radian_host::{PendingRequest, PendingResponse, PendingView};
use radian_permissions::{ApprovalOutcome, ApprovalRequest};
use radian_session::RuntimeEvent;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::blocks::{Block, ReasoningMode};
use crate::clip::ClipboardMode;
use crate::clip::MAX_CLIPBOARD_BYTES;
use crate::command::{self, PendingCommand};
use crate::composer::Composer;
use crate::find::FindState;
use crate::keys::{Action, Keymap};
use crate::login::{LoginKey, LoginOutcome, LoginWizard};
use crate::mention::{Completion, ExpandError, Expansion};
use crate::picker::{Outcome as PickerOutcome, Picker};
use crate::projection::{SessionProjection, StreamSnapshot};
use crate::question::{FormKey, FormOutcome, QuestionForm};
use crate::render::InputStyle;
use crate::select::{self, Anchor, Selection};
use crate::startup::StartupScreen;
use crate::theme::Theme;

mod approval;
mod config;
mod find;
mod input;
mod layers;
mod mouse;
mod overlay;
mod state;
mod update;

pub use input::{Input, MemoryResult, PickerData, SlashCompletion};
use input::{PendingExpand, PendingSend};
pub use mouse::{AUTO_SCROLL_TICK, DOUBLE_CLICK_WINDOW, MOUSE_WHEEL_LINES, MouseScrollDirection};
use mouse::{LastClick, SelectDrag};
pub use overlay::{
    AccessChoice, AccessWizard, ApprovalScroll, Layer, Modal, Overlay, PendingApproval, Tier,
    TrustWizard,
};
pub use state::{
    CompactionInfo, Entry, EntryKind, Startup, budget_status_text, budget_text, mcp_text, text_of,
    usage_text,
};
use state::{bash_context_text, bash_entry, bash_entry_from_context, is_user_input};

// ADR 0023 U5: the turn state is a **session fact**, so it lives in the projection
// (`radian-host`) and `App` only reads it. Re-exported here so the existing `app::TurnState`
// path keeps working.
pub use radian_host::projection::TurnState;

/// How long the first idle Ctrl-C arms "press again to quit" (ADR 0023 U4, owner
/// 2026-10-14). A second Ctrl-C inside the window quits; the window closing, or any other
/// key, disarms it and the hint disappears.
pub const QUIT_WINDOW: Duration = Duration::from_secs(2);

/// How long a `/fork` waits for the last turn's boundary to reach the journal (the runtime
/// `TurnFinished`) before giving up. The fork reads the parent file, so forking before the
/// append would copy an empty history. Long enough for an fsync, short enough to tell the
/// user the session is still writing.
pub const FORK_COMMIT_WAIT: Duration = Duration::from_secs(5);

/// An external action. `update` returns these; the event loop runs them. An effect that has a
/// result reports it back as an [`Input`] (`ExpandMentions` → `MentionsExpanded`, and so on).
#[derive(Debug, Clone)]
pub enum Effect {
    /// Start a new turn: `Inbox::followup`.
    SendFollowup(Message),
    /// Redirect the running turn: `Inbox::steer`.
    Steer(Message),
    /// Cancel the running turn.
    Cancel,
    /// Leave the app.
    Quit,
    /// Answer the pending request `request_id` (ADR 0023 U7). The loop runs it through the
    /// session's facade, which re-validates the `AnswerRequest` capability.
    AnswerRequest {
        request_id: u64,
        response: PendingResponse,
    },
    /// Expand the `@` references in `text` (reads files).
    ExpandMentions { text: String },
    /// Complete the `@` reference at the cursor (reads a directory).
    CompleteMention { text: String, cursor: usize },
    /// List one level's candidates in the host (I/O): the loop answers with
    /// [`Input::CompletionsReady`].
    FetchCompletions(command::Fetch),
    /// Run a `/memory` command through the shared executor (`confirmed` skips the prompt).
    RunMemoryCommand {
        command: MemoryCommand,
        confirmed: bool,
    },
    /// Run a manual `/compact` on the runtime, between turns.
    Compact { instructions: Option<String> },
    /// A `/model` target to vet: the loop resolves the credential and the new model's budget
    /// (ADR 0026 D5/D7) and reports back as `Input::ModelChecked`.
    ModelSwitch { provider: String, model: String },
    /// Compact the context to make room for a switch (D7): the loop runs the same compaction
    /// path as `/compact`, targeting the **new** model's budget.
    CompactForSwitch {
        provider: String,
        model: String,
        budget: u64,
    },
    /// Queue a model route for the next turn boundary (D5). The change is a route only.
    QueueModel { provider: String, model: String },
    /// A `/thinking` level to clamp against the current route's map (ADR 0026 D11); the loop
    /// reports back as `Input::ThinkingChecked`.
    SetThinking { level: String },
    /// Queue a thinking level for the next turn boundary (D11): a level name only.
    QueueThinking { level: String },
    /// Run a `/sessions`, `/export` or `/fork` request.
    Session(command::SessionOutcome),
    /// Load this workspace's sessions and open the picker overlay (`/sessions`). The result is
    /// [`Input::PickerOpened`]; loading is I/O, so it is the loop's job (ADR 0023 U3).
    OpenPicker { query: Option<String> },
    /// Run the `/login` verification request (ADR 0026 D3): one `GET <base>/models`. The
    /// answer is [`Input::LoginVerified`].
    LoginVerify {
        name: String,
        protocol: &'static str,
        base_url: String,
        key: String,
    },
    /// Write the `/login` entry into `credentials/auth.json` (D3; only auth.json is written).
    /// The answer is [`Input::LoginSaved`].
    LoginSave {
        name: String,
        key: String,
        env: Vec<(String, String)>,
    },
    /// Remove one stored credential (`/logout`, after the confirmation). The answer is
    /// [`Input::LogoutDone`].
    Logout { provider: String },
    /// Sign in with ChatGPT, browser flow (ADR 0026): bind the loopback listener,
    /// try to open the authorize URL (and report it either way, as
    /// [`Input::LoginOAuthUrlReady`]), wait for the redirect, exchange, save. The terminal
    /// outcome is [`Input::LoginSaved`] — the same event the API-key `Save` path ends on.
    LoginOAuthBrowser,
    /// Sign in with ChatGPT, device-code flow: ask for a user code, report it
    /// ([`Input::LoginOAuthDeviceReady`]), poll, exchange, save. Ends on [`Input::LoginSaved`].
    LoginOAuthDevice,
    /// Switch the session's auto-approval (ADR 0026 D9). The loop applies it through the
    /// facade, which flips the hooks' live policy, audits the switch and queues the durable
    /// `ConfigChanged` record for the next resume.
    SetApprovalPolicy { on: bool },
    /// Switch the session's permission tier (ADR 0038 slice 2). The loop calls the facade
    /// (`set_access`), which rebuilds the profile and the sandbox through the host's builder;
    /// a failed probe is a refusal. `remember` also writes the per-project record. The answer
    /// is [`Input::AccessApplied`].
    SetAccess {
        tier: crate::profile::Tier,
        remember: bool,
    },
    /// Expand `/skill:<name> [args]` (ADR 0025 S3): the host reads the body and the loop sends
    /// the folded `<skill …>` text as one user message (`steer` picks steer vs follow-up). The
    /// answer is [`Input::SkillExpanded`].
    ExpandSkill {
        name: String,
        args: String,
        steer: bool,
    },
    /// List the discovered skills (`/skills`, ADR 0025 S3) as notices. The answer is
    /// [`Input::SessionDone`].
    ListSkills,
    /// `/jobs` (ADR 0037 decision 4): list the session's background jobs, or stop one/all. The
    /// loop runs it (the registry lives in the host) and answers with [`Input::SessionDone`].
    Jobs(command::JobsRequest),
    /// Write the transcript selection to the system clipboard (`tui.clipboard` picks the
    /// transport). The loop runs it and answers with [`Input::ClipboardDone`]: OSC 52 has no
    /// receipt, and `pbcopy` can fail — the loop owns both facts, `update` only owns the text.
    Copy { text: String },
    /// `/theme <name>` (ADR 0008 revision): load the theme and write `tui.theme` back to the
    /// config file. Both are I/O, so the loop runs them and answers with
    /// [`Input::ThemeReady`].
    Theme { name: String },
    /// Apply a startup trust choice (ADR 0031 T3): the loop writes `trust.json` through the
    /// facade and journals one `ProjectTrustChanged` per write. The answer is
    /// [`Input::TrustResolved`].
    SetTrust { choice: radian_host::trust::Choice },
    /// Run a `!`/`!!` command in the sandbox (ADR 0033). The loop spawns it and answers with
    /// [`Input::BashDone`].
    RunBash {
        command: String,
        exclude_from_context: bool,
    },
    /// Enqueue the `Ran \`cmd\`` message for `!` (ADR 0033) as an input that does **not** wake
    /// an idle driver (ADR 0011's `inject`), so the next turn carries it and no turn starts now.
    Inject(Message),
    /// Append one durable `UserBash` audit record (ADR 0033). `!!` emits it too.
    AuditUserBash {
        command: String,
        exclude_from_context: bool,
        exit_code: Option<i32>,
        cancelled: bool,
    },
    /// Cancel the running `!`/`!!` command (Esc, ADR 0033).
    CancelBash,
}

/// Effects are compared by **meaning** in the tests. `MemoryCommand` (a clap enum) has no
/// `PartialEq`, so two memory effects are equal when their parsed form is identical.
impl PartialEq for Effect {
    fn eq(&self, other: &Self) -> bool {
        use Effect::*;
        match (self, other) {
            (SendFollowup(a), SendFollowup(b)) | (Steer(a), Steer(b)) => a == b,
            (Cancel, Cancel) | (Quit, Quit) => true,
            (SetApprovalPolicy { on: a }, SetApprovalPolicy { on: b }) => a == b,
            (
                SetAccess {
                    tier: at,
                    remember: ar,
                },
                SetAccess {
                    tier: bt,
                    remember: br,
                },
            ) => at == bt && ar == br,
            (
                AnswerRequest {
                    request_id: a,
                    response: ao,
                },
                AnswerRequest {
                    request_id: b,
                    response: bo,
                },
            ) => a == b && ao == bo,
            (ExpandMentions { text: a }, ExpandMentions { text: b }) => a == b,
            (
                CompleteMention {
                    text: a,
                    cursor: ac,
                },
                CompleteMention {
                    text: b,
                    cursor: bc,
                },
            ) => a == b && ac == bc,
            (
                RunMemoryCommand {
                    command: a,
                    confirmed: ac,
                },
                RunMemoryCommand {
                    command: b,
                    confirmed: bc,
                },
            ) => ac == bc && format!("{a:?}") == format!("{b:?}"),
            (Compact { instructions: a }, Compact { instructions: b }) => a == b,
            (FetchCompletions(a), FetchCompletions(b)) => a == b,
            (OpenPicker { query: a }, OpenPicker { query: b }) => a == b,
            (Session(a), Session(b)) => a == b,
            (
                LoginVerify {
                    name: an,
                    protocol: ap,
                    base_url: ab,
                    key: ak,
                },
                LoginVerify {
                    name: bn,
                    protocol: bp,
                    base_url: bb,
                    key: bk,
                },
            ) => an == bn && ap == bp && ab == bb && ak == bk,
            (
                LoginSave {
                    name: an,
                    key: ak,
                    env: ae,
                },
                LoginSave {
                    name: bn,
                    key: bk,
                    env: be,
                },
            ) => an == bn && ak == bk && ae == be,
            (LoginOAuthBrowser, LoginOAuthBrowser) | (LoginOAuthDevice, LoginOAuthDevice) => true,
            (Logout { provider: a }, Logout { provider: b }) => a == b,
            (Copy { text: a }, Copy { text: b }) => a == b,
            (SetTrust { choice: a }, SetTrust { choice: b }) => a == b,
            (
                ExpandSkill {
                    name: an,
                    args: aa,
                    steer: as_,
                },
                ExpandSkill {
                    name: bn,
                    args: ba,
                    steer: bs,
                },
            ) => an == bn && aa == ba && as_ == bs,
            (ListSkills, ListSkills) => true,
            (Jobs(a), Jobs(b)) => a == b,
            (Theme { name: a }, Theme { name: b }) => a == b,
            (Inject(a), Inject(b)) => a == b,
            (CancelBash, CancelBash) => true,
            (
                RunBash {
                    command: ac,
                    exclude_from_context: ae,
                },
                RunBash {
                    command: bc,
                    exclude_from_context: be,
                },
            ) => ac == bc && ae == be,
            (
                AuditUserBash {
                    command: ac,
                    exclude_from_context: ae,
                    exit_code: aix,
                    cancelled: aca,
                },
                AuditUserBash {
                    command: bc,
                    exclude_from_context: be,
                    exit_code: bix,
                    cancelled: bca,
                },
            ) => ac == bc && ae == be && aix == bix && aca == bca,
            _ => false,
        }
    }
}

/// What one [`App::update`] produced.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UpdateResult {
    /// Whether the state the renderer reads may have changed. A false positive only costs a
    /// frame; the event loop's zero-redraw rule (ADR 0023 U2) is about *no input at all*.
    pub dirty: bool,
    /// The external actions the loop must run, in order.
    pub effects: Vec<Effect>,
}

impl UpdateResult {
    /// Nothing happened.
    fn none() -> Self {
        Self::default()
    }

    /// The state changed.
    fn dirty() -> Self {
        Self {
            dirty: true,
            effects: Vec::new(),
        }
    }

    /// The state changed and the loop has work to do.
    fn effect(effect: Effect) -> Self {
        Self {
            dirty: true,
            effects: vec![effect],
        }
    }

    /// The loop has work to do, but nothing the renderer reads changed.
    fn quiet(effect: Effect) -> Self {
        Self {
            dirty: false,
            effects: vec![effect],
        }
    }
}

/// The **per-session view** (ADR 0030 slice ⓪): the state that belongs to exactly one
/// session — its transcript and the entries' caches, its projection handle, its scroll and
/// selection, its pending turn state, and its mode/tier markers. [`App`] owns one of these
/// today; ADR 0030 slice ⑥ holds several and switches between them, so nothing here is
/// shared. Chrome the whole TUI shares (theme, keymap, footer, the input composer, the layer
/// stack and the terminal-level flags) stays on [`App`].
pub struct SessionView {
    /// The session's id.
    pub session_id: String,
    /// The model this session runs, and its display name (ADR 0027 C2 `{display_name}`): the
    /// descriptor/catalog name when known, else the id. Set once at startup.
    pub model_name: String,
    pub model_display: Option<String>,
    /// The provider's name for the footer's `{provider}` placeholder. Set once at startup.
    pub provider: Option<String>,
    /// The session's working directory (the footer's `{path_home}`) and the git branch the
    /// host read for it. Both are startup facts; the branch is refreshed by the host.
    pub cwd: Option<PathBuf>,
    pub git_branch: Option<String>,
    /// The model's catalog unit prices (ADR 0027). The footer's `cost` reads the
    /// **projection's** accumulated fee, because the fee is per call at the price in force
    /// then (a `/model` switch does not re-price earlier calls).
    pub model_price: Option<radian_host::ModelPrice>,
    /// The spinner's origin (ADR 0027): `now - started` is the tick the spinner
    /// frame is a function of. Set at construction; tests set it to inject time.
    pub spinner_origin: Instant,
    /// The local turn timer behind `speed`/`elapsed`.
    pub timer: crate::timing::TurnTimer,
    /// The applied thinking level (ADR 0026 D11), for the footer's `reasoning` item. A queued
    /// switch shows the queued level until the driver applies it.
    pub thinking_level: Option<String>,
    pub entries: Vec<Entry>,
    /// How many entries are currently `live` (fed by stream deltas, not yet a committed
    /// message). Maintained so `push_message` scans the transcript for live entries only
    /// when there are some: a resumed session pushes every message with none, and the old
    /// unconditional `retain` was O(n^2) there (resume-perf A).
    live_count: usize,
    /// This session's submitted inputs, oldest first (sent messages and `/…` commands). Not
    /// persisted; a resume rebuilds it from the journal's user messages.
    pub history: Vec<String>,
    /// `Some(i)` while browsing `history[i]`; `None` while editing the current line.
    pub history_cursor: Option<usize>,
    /// The line that was being edited when browsing started, restored by `Down` past the end.
    pub history_stash: String,
    /// The first visible transcript line. `usize`: a long session passes 65,535 wrapped lines
    /// (dogfooding D9), and a `u16` silently wrapped there.
    pub scroll: usize,
    /// True while the view is pinned to the newest line. Scrolling up clears it; scrolling
    /// back to the bottom, or new content while it is set, pins it again.
    pub follow: bool,
    /// The session facts (ADR 0023 U5): turn, usage, budget, tier, MCP counts, the last tool
    /// call and the streaming buffer. `SessionView` holds a **read handle** only; the host's
    /// `SessionFacade` owns the projection, and every fact is written there, never here.
    pub projection: Arc<SessionProjection>,
    /// The transient footer note ("tool … running", "retrying …", "compacting context", a
    /// copied-selection count). The tool note is derived from [`SessionView::running_tools`], so
    /// it can never name a tool that is not running (ADR 0027).
    pub note: Option<String>,
    /// The tools running right now, in start order: `(call_id, name)`. A batch can run several,
    /// and they settle in **completion order**, which may differ from start order (agent-core's
    /// `ToolFinished` contract), so the list — not a single name — is what the note follows: it
    /// names the last call that is still running.
    running_tools: Vec<(String, String)>,
    /// The driver stopped (a fatal error, a panic or a graceful close): the session is over.
    /// The reason is shown in the status bar and `/fork` is refused. The TUI stays so the
    /// reason is readable; the user quits with the ordinary keys.
    pub session_ended: Option<String>,
    /// Whether the last turn's boundary is on disk (the runtime `TurnFinished` was seen). A
    /// `/fork` requested while a turn is still being written waits for it (see
    /// [`SessionView::pending_fork`]).
    pub turn_committed: bool,
    /// A `/fork` waiting for the turn boundary to reach the journal, with its deadline.
    pub pending_fork: Option<Instant>,
    /// A compaction is running (ADR 0018): the input rule's status line shows `Compacting`
    /// (ADR 0027). Set on `CompactionStarted`, cleared when it finishes, fails or
    /// is skipped.
    pub compacting: bool,
    /// The startup screen: the leading block of entries (wordmark,
    /// collapsed information, safety lines, Tip). `None` before the host sets it.
    pub startup_screen: Option<StartupScreen>,
    /// How many leading entries are the startup block, so a toggle replaces exactly it and
    /// leaves the history alone.
    startup_len: usize,
    /// The in-session auto-approval state (ADR 0026 D9). The status bar shows `AUTO` while
    /// it is on; the approval box's `!` key and `/auto` switch it. The live policy itself
    /// is the hooks' — the loop applies it through [`Effect::SetApprovalPolicy`] — this is
    /// the UI's mirror, so `update` stays pure.
    pub auto: bool,
    /// Where the tier in force came from (ADR 0038 decision 3), for `/access` with no argument
    /// and the footer's `(project)` marker. Set at startup from the resolution, and updated by
    /// a run-time switch. A UI/startup label, not a session fact (the tier itself is in the
    /// projection).
    pub access_source: crate::profile::AccessSource,
    /// `/access`'s second line when the sandbox is `inherited` (ADR 0016):
    /// the startup profile line, so the command says what the startup line says instead of
    /// leaving the user to guess what `workspace` is enforced by. `None` for every other
    /// session, where the tier's sandbox is described by the `workspace` facts sentence.
    pub sandbox_note: Option<String>,
    /// The queued switch, shown as `next: <provider/model>` until the driver applies it.
    pub next_model: Option<String>,
    /// The queued thinking level (D11), cleared when the driver applies it.
    pub next_thinking: Option<String>,
    /// The `!`/`!!` command currently running (ADR 0033); `Some` blocks a second one.
    pub bash_running: Option<String>,
    /// A `!`/`!!` command submitted while a turn was running: it starts when the turn ends.
    pub queued_bash: Option<(String, bool)>,
    /// Whether this session has a shell (`!`) at all: false on the `safe` tier (ADR 0012).
    pub bash_available: bool,
    /// The exact `Ran \`cmd\`` text we displayed live as a [`EntryKind::Bash`] block for a `!`
    /// command. When that message is admitted to the transcript, `push_message` skips it so
    /// the block is not shown twice; a resume has no pending text and renders it as a block.
    bash_context_pending: Option<String>,
    pub total_lines: usize,
    pub view_height: u16,
    pub max_scroll: usize,
    /// The workspace root, so `Tab` can complete `@` file references. `None` until the host
    /// sets it; without it `Tab` does nothing.
    pub workspace: Option<PathBuf>,
    /// The transcript width the renderer last used, so a find recomputes on a resize.
    pub transcript_width: usize,
    /// The transcript selection, anchored to entries and offsets ([`Selection`]) — never to
    /// cells — so new output, scrolling and reflow leave it where it was. `Some` also while
    /// the drag is still moving; the renderer highlights it each frame.
    pub selection: Option<Selection>,
    /// The live left-button gesture, from press to release.
    selecting: Option<SelectDrag>,
    /// The last press, for double/triple-click detection.
    last_click: Option<LastClick>,
    /// The revision and token counts of the compaction that just finished (ADR 0028 R5): the
    /// summary message that follows takes them for its one-liner. Empty on a resume replay.
    /// The last `CompactionFinished`'s numbers, handed to the summary message that follows it
    /// (ADR 0018). `(revision, tokens_before, tokens_after, deterministic, dropped)`.
    pending_compaction: Option<(u64, u64, u64, bool, u64)>,
}

/// The whole TUI's state: the global chrome every session view shares, plus exactly one
/// [`SessionView`] today (ADR 0030 slice ⓪). The chrome is what is set once from config and
/// the terminal (theme, keymap, footer, the input composer, the layer stack, the queue count
/// and the terminal-level flags); the session's transcript, projection, scroll/selection and
/// turn state live in [`SessionView`] so a later slice can hold several and switch.
pub struct App {
    /// The machine's hostname (ADR 0027 `host`), read once at startup.
    pub hostname: Option<String>,
    /// Whether automatic compaction is on (`context`'s `{auto}` marker), set once at startup.
    pub auto_compact: bool,
    /// The loop's current instant (ADR 0027). The timer never reads the wall clock;
    /// the loop stamps `now` once per iteration and tests set it to inject time.
    pub now: Instant,
    /// The footer (ADR 0027 C2): its preset, glyphs and rows. Set from `[tui.footer]` at
    /// startup; the default is the two-row `pi` preset.
    pub footer: crate::footer::Footer,
    /// The contributor store (ADR 0027 C3). The background tasks write it; the renderer reads a
    /// snapshot, and the `git_branch`/`worktree`/`status` items come from it.
    pub contributions: std::sync::Arc<radian_host::ContributionStore>,
    /// The footer facts a background `command` item reads on stdin (ADR 0027
    /// decision 4). The loop publishes a snapshot as the facts change; the command producers read
    /// it, so the draw path never runs a command and the command never blocks a frame.
    pub footer_context: std::sync::Arc<radian_host::FooterContext>,
    /// `[tui.widgets]`: contributor items on a line above/below the input box (ADR 0027 C3).
    pub widgets: radian_host::config::TuiWidgetsConfig,
    /// The input composer: text plus a cursor (ADR 0009 Phase 4).
    pub input: Composer,
    pub quit: bool,
    /// How many requests the queue holds (ADR 0023 U7), for the status bar.
    pub pending: usize,
    /// When the first idle Ctrl-C armed "press again to quit": the
    /// deadline a second Ctrl-C must beat. `None` means not armed. Pure state; the wall clock
    /// arrives on [`Input::Cancel`], so `update` never reads the clock.
    pub quit_pending: Option<Instant>,
    /// The last request the UI answered: the queue's head stays that request for a moment, and
    /// re-showing it would flash a form the user just submitted.
    last_answered: Option<u64>,
    /// The layer stack (ADR 0023 U4): [`Layer::Surface`] at index 0, then overlays and modals
    /// above it. Only [`App::push_overlay`]/[`App::push_modal`] and the matching removals
    /// change it, so the tier rules hold by construction.
    layers: Vec<Layer>,
    /// Whether this process has a memory root. `/memory` runs through the loop, which owns the
    /// service; without one the command is refused with the same notice as before.
    pub memory_enabled: bool,
    /// The send waiting for its `@` expansion (see [`Effect::ExpandMentions`]).
    pending_expand: Option<PendingExpand>,
    /// The `/…` completion on screen: the range a candidate replaces and the candidates, best
    /// first. `None` when the line is not a slash command or the level has nothing to offer.
    /// (The overlay's *rows* live in the layer stack, next to the `@` list's.)
    pub slash_completion: Option<SlashCompletion>,
    /// The `@` completion the menu is showing (its range and raw candidates), so the highlighted
    /// row can be accepted. `None` unless the `@` list is up.
    pub mention_completion: Option<(std::ops::Range<usize>, Vec<String>)>,
    /// The highlighted row of the completion menu (slash or `@`). Reset to the first row
    /// whenever the menu is (re)built, so typing keeps the highlight on the best match.
    pub completion_cursor: usize,
    /// The host fetch the current completion waits for: a result is applied only when it
    /// answers *this* fetch, so a stale answer cannot land on a newer line.
    pending_fetch: Option<command::Fetch>,
    /// The color theme (config `tui.theme`, `NO_COLOR`).
    pub theme: Theme,
    /// `tui.brand` (ADR 0027): the brand text, or `None` for the glyph set's
    /// default. The footer's `brand` item and the startup banner both read it.
    pub brand: Option<String>,
    /// `tui.spinner` (ADR 0027): the running indicator. `pulse` by default.
    pub spinner_kind: crate::spinner::Kind,
    /// The key bindings (config `tui.keybindings`), defaulting to the pre-config key table.
    pub keymap: Keymap,
    /// How the input area is drawn (config `tui.input_style`), defaulting to `lines`.
    pub input_style: InputStyle,
    /// The startup phase (ADR 0023 U4): [`Startup::Picking`] while the pre-session `--pick`
    /// overlay is the whole point of the loop, [`Startup::Session`] otherwise.
    pub startup: Startup,
    /// Set when the picker resolves during [`Startup::Picking`]. The pre-session loop leaves
    /// as soon as it is `Some`; the caller reads it with [`App::take_picker_outcome`].
    picker_outcome: Option<PickerOutcome>,
    /// Whether mouse reporting is on (`tui.mouse`, default on). With it off the terminal
    /// never sends a mouse event and every mouse method here is a no-op: none of the
    /// selection/copy features exist (the belt behind the terminal's suspenders).
    mouse_enabled: bool,
    /// How a copy reaches the system clipboard (config `tui.clipboard`, default OSC 52).
    pub clipboard: ClipboardMode,
    /// How the reasoning channel displays (ADR 0028 R3, `tui.display.reasoning`). Part of
    /// the block/layout cache keys: toggling re-lays the reasoning entries out.
    pub reasoning_mode: ReasoningMode,
    /// How tool card bodies fold (ADR 0028 R4, `tui.display.tool_output` +
    /// `tool_preview_lines`). Also part of the layout cache keys.
    pub tool_display: crate::tools::ToolDisplay,
    /// `tui.display.mermaid` (ADR 0028 R8), default true: draw a closed ```mermaid block as a
    /// diagram. `false` renders every code block as source. Part of `DisplayModes`, so it is
    /// in the layout cache key.
    pub mermaid_enabled: bool,
    /// `tui.display.highlight` (ADR 0028 R2), default true: highlight a closed code block whose
    /// info string names a common language. `false` renders every code block as plain text.
    /// Part of `DisplayModes`, so it is in the layout cache key.
    pub highlight_enabled: bool,
    /// The one session view this process holds today (ADR 0030 slice ⓪). A later slice
    /// (ADR 0030 slice ⑥) keeps several and switches between them.
    pub session: SessionView,
}

impl SessionView {
    /// Records a call that started (`AgentEvent::ToolStarted`). The note names it: the last
    /// call to start is the one a user watching the footer most likely wants named. O(1) and
    /// without a uniqueness scan: the only writer is the core's one `ToolStarted` per call, and
    /// [`SessionView::finish_tool`] removes *every* entry of the id it is given, so a repeated
    /// start could not leave a piece behind. (A scan would make a flood of facts quadratic —
    /// `--flood-facts` sends 20 000 — and starve the loop's draw fairness.)
    pub(crate) fn start_tool(&mut self, call_id: &str, name: &str) {
        self.running_tools
            .push((call_id.to_owned(), name.to_owned()));
        self.note_for_running_tools();
    }

    /// Records a settled call (`AgentEvent::ToolFinished`). The note follows the calls that are
    /// still running: it names the last one of them, and goes when the last of the batch does.
    /// A note another writer owns (a copy, a retry) is left alone — only the tools' own note is
    /// rewritten.
    pub(crate) fn finish_tool(&mut self, call_id: &str) {
        let note_is_the_tools = self.note == self.tool_note();
        self.running_tools.retain(|(id, _)| id != call_id);
        if note_is_the_tools {
            self.note_for_running_tools();
        }
    }

    /// Forgets every running tool at a model-request boundary, where nothing can still be
    /// running, dropping the note when it was the tools'.
    pub(crate) fn forget_running_tools(&mut self) {
        if self.note == self.tool_note() {
            self.note = None;
        }
        self.running_tools.clear();
    }

    /// The note the tools own: `tool <name> running` for the last still-running call, `None`
    /// when none runs.
    fn tool_note(&self) -> Option<String> {
        self.running_tools
            .last()
            .map(|(_, name)| format!("tool {name} running"))
    }

    /// Rewrites the note from the running set. Called only when the note is the tools'.
    fn note_for_running_tools(&mut self) {
        self.note = self.tool_note();
    }
}

impl App {
    // ---------- in-transcript search (Phase 5) ----------

    /// Records the transcript width the renderer used. A width change re-wraps the transcript,
    /// so an open find box recomputes its matches — once per resize, never per frame.
    pub fn set_transcript_width(&mut self, width: usize) {
        if self.session.transcript_width == width {
            return;
        }
        self.session.transcript_width = width;
        // The wordmark drops to its one-line fallback below a width; rebuilding is cheap and
        // only happens when the width actually changed.
        if self.session.startup_screen.is_some() {
            self.rebuild_startup_entries();
        }
        if self.find_state().is_some() {
            self.find_recompute();
        }
    }

    /// Sets the startup screen and builds its leading entry block.
    pub fn set_startup(&mut self, startup: StartupScreen) {
        self.session.startup_screen = Some(startup);
        self.rebuild_startup_entries();
    }

    /// `Ctrl-P` (the `toggle_startup` action): fold or unfold the startup information.
    pub fn toggle_startup(&mut self) -> UpdateResult {
        let Some(startup) = self.session.startup_screen.as_mut() else {
            return UpdateResult::none();
        };
        startup.expanded = !startup.expanded;
        self.rebuild_startup_entries();
        UpdateResult::dirty()
    }

    /// The leading startup block, rebuilt from [`StartupScreen`]: the wordmark (the wide form
    /// when it fits the current width), the collapsed line or the full lines, the safety
    /// lines (always shown), and the Tip.
    fn rebuild_startup_entries(&mut self) {
        let Some(startup) = self.session.startup_screen.clone() else {
            self.session
                .entries
                .drain(0..self.session.startup_len.min(self.session.entries.len()));
            self.session.startup_len = 0;
            return;
        };
        let mut block: Vec<Entry> = startup
            .banner_for_width(self.session.transcript_width)
            .into_iter()
            .map(|line| Entry::new(EntryKind::Banner, line))
            .collect();
        if startup.expanded {
            block.push(Entry::new(
                EntryKind::Startup,
                "- startup information (Ctrl-P to fold)",
            ));
            for line in &startup.lines {
                block.push(Entry::new(EntryKind::Startup, line.clone()));
            }
        } else {
            block.push(Entry::new(
                EntryKind::Startup,
                format!("+ {}   (Ctrl-P to expand)", startup.summary),
            ));
        }
        for line in &startup.safety {
            block.push(Entry::new(EntryKind::Warning, line.clone()));
        }
        if let Some(tip) = &startup.tip {
            block.push(Entry::new(EntryKind::Tip, tip.clone()));
        }
        let block_len = block.len();
        let replace = self.session.startup_len.min(self.session.entries.len());
        self.session.entries.splice(0..replace, block);
        self.session.startup_len = block_len;
    }

    /// The single state-change entry point (ADR 0023 U3). Pure: no I/O, no runtime, no
    /// service. Anything that needs the outside world comes back as an [`Effect`].
    ///
    /// The `/…` completion follows the input text, so it is recomputed here whenever an input
    /// changed the line (one place, instead of every editing arm).
    pub fn update(&mut self, input: Input) -> UpdateResult {
        let before = self.input.text();
        let mut result = self.update_inner(input);
        if self.input.text() != before {
            result.effects.extend(self.refresh_slash());
            result.dirty = true;
        }
        result
    }

    /// Starts a send. The `@` references are expanded by the loop first (an effect), and the
    /// result becomes the follow-up or steer effect. Whether it steers is decided here, because
    /// the turn state is app state.
    fn begin_send(&mut self, text: String) -> UpdateResult {
        let kind = if self.turn() == TurnState::Running {
            PendingSend::Steer
        } else {
            PendingSend::Followup
        };
        self.pending_expand = Some(PendingExpand {
            kind,
            text: text.clone(),
        });
        UpdateResult::effect(Effect::ExpandMentions { text })
    }

    /// The hardcoded answer to a `/memory` confirmation: `y` runs the command (through the
    /// loop), `n`/`Esc` cancels it.
    fn confirm_command(&mut self, confirmed: bool) -> UpdateResult {
        let Some(Modal::ConfirmCommand(pending)) =
            self.remove_modal(|modal| matches!(modal, Modal::ConfirmCommand(_)))
        else {
            return UpdateResult::none();
        };
        // A `/model` confirm (D7): y compacts first, n cancels the switch. The modal's y/n are
        // hardcoded (ADR 0023 U4), exactly like the approval's.
        if let Some((provider, model, budget)) = pending.switch {
            if !confirmed {
                self.push_notice("cancelled; the model is unchanged");
                return UpdateResult::dirty();
            }
            return UpdateResult::effect(Effect::CompactForSwitch {
                provider,
                model,
                budget,
            });
        }
        if !confirmed {
            self.push_notice("cancelled");
            return UpdateResult::dirty();
        }
        UpdateResult::effect(Effect::RunMemoryCommand {
            command: pending.command,
            confirmed: true,
        })
    }

    /// ADR 0017: a failed turn is shown in the transcript; the session stays open.
    fn apply_runtime_event(&mut self, event: &RuntimeEvent) -> UpdateResult {
        if let RuntimeEvent::TurnFinished { .. } = event {
            // The turn boundary is on disk: a `/fork` that was waiting for it can run now.
            self.session.turn_committed = true;
            if self.session.pending_fork.take().is_some() {
                return UpdateResult::effect(Effect::Session(command::SessionOutcome::Fork));
            }
        }
        if let RuntimeEvent::ConfigChanged { change, .. } = event {
            // The driver applied the queued change: the status bar's `next:` promise is done.
            self.session.next_model = None;
            self.session.next_thinking = None;
            // Owner addendum (ADR 0027 slice ①): an applied model switch must follow
            // through to the footer's `model` item, or it keeps showing the startup model. The
            // session's model facts follow the applied route the way startup leaves them: the
            // model id, the provider from the route, and `model_display = None` so
            // `{display_name}` falls back to the id. Startup passes `None` for the display name
            // too (`cli.rs`'s `set_footer_facts`); no catalog name is resolved today.
            if let Some(route) = radian_host::ModelRoute::from_value(change) {
                self.session.model_name = route.model;
                self.session.provider = Some(route.provider);
                self.session.model_display = None;
            }
        }
        if let RuntimeEvent::TurnFailed { message, .. } = event {
            self.session.entries.push(Entry::new(
                EntryKind::Notice,
                format!("turn failed: {message}"),
            ));
            // ADR 0018's fallback: a recognizable context overflow gets one extra line, since
            // automatic compaction did not keep the request inside the window. The host only
            // has the text, so the prefix is what identifies it.
            if radian_host::is_context_overflow(message) {
                self.session.entries.push(Entry::new(
                    EntryKind::Notice,
                    radian_host::CONTEXT_OVERFLOW_HELP.to_string(),
                ));
            }
            return UpdateResult::dirty();
        }
        UpdateResult::none()
    }

    /// A `/skill:<name>` expansion finished (ADR 0025 S3). On success the message is sent like
    /// a typed one (steer when a turn was running); on failure the notice is shown and nothing
    /// is sent — an unknown skill is not a message.
    fn skill_expanded(&mut self, result: Result<String, String>, steer: bool) -> UpdateResult {
        match result {
            Ok(text) => {
                let message = Message::user(text);
                UpdateResult::effect(if steer {
                    Effect::Steer(message)
                } else {
                    Effect::SendFollowup(message)
                })
            }
            Err(error) => {
                self.push_notice(error);
                UpdateResult::dirty()
            }
        }
    }

    /// The `@` expansion finished. On success the send effect is produced, with the notices the
    /// expansion reported; on failure the original text goes back into the input box, so
    /// nothing is sent and the user can fix the message.
    fn mentions_expanded(&mut self, result: Result<Expansion, ExpandError>) -> UpdateResult {
        let Some(pending) = self.pending_expand.take() else {
            return UpdateResult::none();
        };
        match result {
            Ok(expansion) => {
                for warning in &expansion.warnings {
                    self.session
                        .entries
                        .push(Entry::new(EntryKind::Notice, warning.clone()));
                }
                let attached = expansion.attached();
                if !attached.is_empty() {
                    self.session.entries.push(Entry::new(
                        EntryKind::Notice,
                        format!(
                            "attached {} file(s): {}",
                            attached.len(),
                            attached.join(", ")
                        ),
                    ));
                }
                // ADR 0018: the message carries the text and each attachment as
                // separate parts, so Core can keep the user's words verbatim.
                let message = crate::mention::user_message(&expansion.text, &expansion.attachments);
                let effect = match pending.kind {
                    PendingSend::Followup => Effect::SendFollowup(message),
                    PendingSend::Steer => Effect::Steer(message),
                };
                UpdateResult::effect(effect)
            }
            Err(error) => {
                self.session
                    .entries
                    .push(Entry::new(EntryKind::Notice, format!("not sent: {error}")));
                self.restore_input(pending.text);
                UpdateResult::dirty()
            }
        }
    }

    /// The `@` completion finished: apply it, or clear the list when there is nothing.
    fn completion_ready(&mut self, completion: Option<Completion>) -> UpdateResult {
        match completion {
            Some(completion) if !completion.candidates.is_empty() => {
                self.apply_completion(completion);
            }
            _ => self.completion_clear(),
        }
        UpdateResult::dirty()
    }

    /// Recomputes the `/…` completion for the current input. Pure: a level that needs the host
    /// returns the effect that fetches its candidates.
    fn refresh_slash(&mut self) -> Vec<Effect> {
        if !self.input.text().starts_with('/') {
            self.slash_completion = None;
            self.pending_fetch = None;
            return Vec::new();
        }
        match command::complete_in(command::tree(), &self.input.text(), self.input.cursor()) {
            Some(completion) => match completion.level_result {
                command::Level::Ready(candidates) => {
                    self.pending_fetch = None;
                    self.set_slash(completion.range, candidates);
                    Vec::new()
                }
                command::Level::Needs(fetch) => {
                    // The candidates need the host: ask for them and keep no list until they
                    // arrive (a list from the previous level would be misleading).
                    self.slash_completion = None;
                    self.completion_clear();
                    self.pending_fetch = Some(fetch.clone());
                    vec![Effect::FetchCompletions(fetch)]
                }
                command::Level::Free => {
                    self.slash_completion = None;
                    self.completion_clear();
                    self.pending_fetch = None;
                    Vec::new()
                }
            },
            None => {
                self.slash_completion = None;
                self.completion_clear();
                self.pending_fetch = None;
                Vec::new()
            }
        }
    }

    /// Shows the `/…` candidates: the overlay rows, and the state Tab/Enter accepts from.
    fn set_slash(&mut self, range: std::ops::Range<usize>, candidates: Vec<command::Candidate>) {
        if candidates.is_empty() {
            // Nothing to offer at this level: an empty list must not stay "open", or the next
            // Enter would be swallowed accepting a candidate that does not exist (and the
            // line could never be submitted).
            self.slash_completion = None;
            self.completion_clear();
            return;
        }
        self.completion_set(candidates.clone());
        self.slash_completion = Some(SlashCompletion { range, candidates });
    }

    /// Accepts the best `/…` candidate: it completes the line and never executes. The text
    /// change recomputes the next level (an effect when that level needs the host).
    fn accept_slash_candidate(&mut self) -> UpdateResult {
        let Some(pending) = self.slash_completion.take() else {
            return UpdateResult::none();
        };
        // The highlighted row (the first by default); the cursor is clamped because the list
        // can shrink between a key and its handling.
        let index = self.completion_cursor.min(pending.candidates.len() - 1);
        let Some(candidate) = pending.candidates.get(index) else {
            return UpdateResult::none();
        };
        // A trailing space, so the next level can be typed (or completed) right away.
        let insert = format!("{} ", candidate.insert);
        self.replace_range(pending.range.clone(), &insert);
        self.completion_clear();
        // The next level's list is recomputed by `update` (it sees the text change), so this
        // returns no effect of its own.
        UpdateResult::dirty()
    }

    /// The host answered [`Effect::FetchCompletions`]: show the candidates, unless the line has
    /// moved on since (then the answer is stale and is dropped).
    fn completions_ready(
        &mut self,
        fetch: command::Fetch,
        candidates: Vec<command::Candidate>,
    ) -> UpdateResult {
        if self.pending_fetch.as_ref() != Some(&fetch) {
            return UpdateResult::none();
        }
        self.pending_fetch = None;
        let Some(completion) =
            command::complete_in(command::tree(), &self.input.text(), self.input.cursor())
        else {
            return UpdateResult::none();
        };
        if completion.level_result != command::Level::Needs(fetch.clone()) {
            return UpdateResult::none();
        }
        let candidates = command::rank(candidates, fetch.partial());
        // The `Ready` levels' rule holds here too (one predicate, `command::is_exact_candidate`):
        // a candidate that is already exactly what was typed is not a completion to accept —
        // arming it would make Enter accept (inserting the same word, or a *different* model when
        // a longer id shares the prefix) instead of submitting the finished line.
        if command::is_exact_candidate(&candidates, fetch.partial()) {
            self.completion_clear();
            return UpdateResult::dirty();
        }
        self.set_slash(completion.range, candidates);
        UpdateResult::dirty()
    }

    /// A `/memory` command finished: show its output, or arm the confirmation prompt.
    fn memory_command_done(&mut self, result: MemoryResult) -> UpdateResult {
        match result {
            MemoryResult::Output(lines) => {
                for line in lines {
                    self.push_notice(line);
                }
            }
            MemoryResult::NeedsConfirmation { command, prompt } => {
                self.session
                    .entries
                    .push(Entry::new(EntryKind::Notice, format!("{prompt} [y/N]")));
                self.push_modal(Modal::ConfirmCommand(PendingCommand {
                    command,
                    prompt,
                    switch: None,
                }));
            }
            MemoryResult::Failed(message) => self.push_notice(message),
        }
        UpdateResult::dirty()
    }

    /// A manual `/compact` finished (ADR 0018).
    fn compact_done(&mut self, result: Result<CompactionOutcome, String>) -> UpdateResult {
        match result {
            Ok(CompactionOutcome::Compacted {
                tokens_before,
                tokens_after,
                ..
            }) => {
                self.push_notice(format!(
                    "compaction: {tokens_before} → {tokens_after} tokens"
                ));
            }
            Ok(CompactionOutcome::NothingToCompact) => {
                self.push_notice("compaction: nothing to compact");
            }
            Err(message) => self.push_notice(format!("compaction failed: {message}")),
        }
        UpdateResult::dirty()
    }

    /// True while the input box is editable: no approval or command prompt is up, so the
    /// cursor belongs in the input box and keys type into it.
    pub fn input_editable(&self) -> bool {
        !self.has_modal()
    }

    /// `Up` on the first input line: the previous history entry. Starts browsing by stashing
    /// the current line, so `Down` past the newest entry restores it. False with no history.
    pub fn history_up(&mut self) -> bool {
        if self.session.history.is_empty() {
            return false;
        }
        let next = match self.session.history_cursor {
            None => {
                self.session.history_stash = self.input.text();
                self.session.history.len() - 1
            }
            Some(0) => return false,
            Some(i) => i - 1,
        };
        self.session.history_cursor = Some(next);
        self.input.set_text(&self.session.history[next]);
        true
    }

    /// `Down` on the last input line: the next history entry, or the stashed draft at the end.
    pub fn history_down(&mut self) -> bool {
        let Some(i) = self.session.history_cursor else {
            return false;
        };
        if i + 1 < self.session.history.len() {
            self.session.history_cursor = Some(i + 1);
            self.input.set_text(&self.session.history[i + 1]);
        } else {
            self.session.history_cursor = None;
            let stash = std::mem::take(&mut self.session.history_stash);
            self.input.set_text(&stash);
        }
        true
    }

    /// Prefills the history from a resumed session's user messages (ADR 0006 §5.5: the
    /// source rides on the message). Only what the user sent, in order.
    pub fn prefill_history(&mut self, messages: &[Message]) {
        for message in messages {
            if let Message::User { content, meta } = message
                && matches!(meta.source, None | Some(agent_core::InputSource::User))
            {
                self.session.history.push(text_of(content));
            }
        }
    }

    /// Applies the wizard's outcome: stay, or hand the loop an effect / close. The "what
    /// next" line on success is the loop's (it knows the save succeeded).
    fn login_outcome(&mut self, outcome: LoginOutcome) -> UpdateResult {
        match outcome {
            LoginOutcome::Stay => {}
            LoginOutcome::Cancel => {
                self.remove_modal(|modal| matches!(modal, Modal::Login(_)));
                self.push_notice("login cancelled; nothing was saved");
            }
            LoginOutcome::Verify {
                name,
                protocol,
                base_url,
                key,
            } => {
                return UpdateResult::effect(Effect::LoginVerify {
                    name,
                    protocol,
                    base_url,
                    key,
                });
            }
            LoginOutcome::Save { name, key, env } => {
                return UpdateResult::effect(Effect::LoginSave { name, key, env });
            }
            LoginOutcome::BeginOAuthBrowser => {
                return UpdateResult::effect(Effect::LoginOAuthBrowser);
            }
            LoginOutcome::BeginOAuthDevice => {
                return UpdateResult::effect(Effect::LoginOAuthDevice);
            }
        }
        UpdateResult::dirty()
    }

    /// A `!`/`!!` command (ADR 0033): one at a time, and while a turn is running it waits for
    /// the turn to end rather than racing it. The `safe` tier has no shell at all.
    fn begin_bash(&mut self, command: String, exclude_from_context: bool) -> UpdateResult {
        if !self.session.bash_available {
            self.push_notice(
                "the `safe` tier has no shell (ADR 0012); `!` needs a shell-capable tier",
            );
            return UpdateResult::dirty();
        }
        if self.session.bash_running.is_some() {
            self.push_notice("a shell command is already running; Esc cancels it first");
            return UpdateResult::dirty();
        }
        if self.turn() == TurnState::Running {
            self.session.queued_bash = Some((command, exclude_from_context));
            self.push_notice("a turn is running; the command runs when it ends");
            return UpdateResult::dirty();
        }
        self.session.bash_running = Some(command.clone());
        UpdateResult::effect(Effect::RunBash {
            command,
            exclude_from_context,
        })
    }

    /// A `!`/`!!` command finished: show its block, and for `!` enqueue the `Ran \`cmd\``
    /// message for the next turn (never starting one, ADR 0011). The audit record is emitted
    /// for both `!` and `!!`.
    fn bash_done(
        &mut self,
        command: String,
        exclude_from_context: bool,
        result: Result<radian_coding::UserBash, String>,
    ) -> UpdateResult {
        self.session.bash_running = None;
        let mut effects = Vec::new();
        let (exit_code, cancelled) = match &result {
            Ok(run) => (run.exit_code, run.cancelled),
            Err(_) => (None, false),
        };
        match result {
            Ok(run) => {
                self.session
                    .entries
                    .push(bash_entry(&command, exclude_from_context, &run));
                if !exclude_from_context {
                    let text = bash_context_text(&command, &run);
                    // Shown live; the same message coming back through the transcript is
                    // skipped so the block is not drawn twice.
                    self.session.bash_context_pending = Some(text.clone());
                    effects.push(Effect::Inject(Message::user(text)));
                }
            }
            Err(error) => self.session.entries.push(Entry::new(
                EntryKind::Notice,
                format!("!{command}: {error}"),
            )),
        }
        effects.push(Effect::AuditUserBash {
            command,
            exclude_from_context,
            exit_code,
            cancelled,
        });
        UpdateResult {
            dirty: true,
            effects,
        }
    }

    /// Runs one `/…` line (and a `!`/`!!` line, which shares the parse). Parsing is pure;
    /// everything that touches the world becomes an effect, and its result comes back as
    /// another [`Input`]. The line is not sent anywhere.
    fn run_slash_command(&mut self, line: &str) -> UpdateResult {
        match command::parse(line, self.memory_enabled) {
            command::Outcome::Notices(notices) => {
                for notice in notices {
                    self.push_notice(notice);
                }
                UpdateResult::dirty()
            }
            command::Outcome::Memory(command) => UpdateResult::effect(Effect::RunMemoryCommand {
                command,
                confirmed: false,
            }),
            // `/compact` is asynchronous: the loop runs it on the runtime between turns and the
            // result comes back as `Input::CompactDone`.
            command::Outcome::Compact(instructions) => {
                UpdateResult::effect(Effect::Compact { instructions })
            }
            command::Outcome::Sessions(query) => {
                // `/sessions` opens the picker overlay (ADR 0023 U4): listing reads the session
                // directory, so it is an effect the loop runs.
                UpdateResult::effect(Effect::OpenPicker { query })
            }
            command::Outcome::Export(request) => {
                UpdateResult::effect(Effect::Session(command::SessionOutcome::Export(request)))
            }
            command::Outcome::Fork => {
                // ADR 0022 §1: fork only when the session is idle. The decision is app state,
                // so it is made here rather than in the loop.
                if self.session.session_ended.is_some() {
                    self.push_notice(
                        "the session has ended; there is nothing to fork (`/fork` needs a live \
                         session)",
                    );
                    return UpdateResult::dirty();
                }
                if self.turn() == TurnState::Running {
                    self.push_notice(
                        "finish the current turn before forking (`/fork` needs an idle session)",
                    );
                    return UpdateResult::dirty();
                }
                // The turn ended (idle) but its boundary may not be on disk yet: the fork
                // reads the parent file, so it would copy an empty history. Wait for the
                // journaled `TurnFinished` (bounded by `FORK_COMMIT_WAIT`).
                if !self.session.turn_committed {
                    self.session.pending_fork = Some(self.now + FORK_COMMIT_WAIT);
                    return UpdateResult::dirty();
                }
                UpdateResult::effect(Effect::Session(command::SessionOutcome::Fork))
            }
            command::Outcome::Theme(name) => {
                // ADR 0008 revision: the loop loads the theme (I/O) and writes `tui.theme`
                // back to the config file; the notice says which of those happened.
                UpdateResult::effect(Effect::Theme { name })
            }
            command::Outcome::Thinking(level) => {
                UpdateResult::effect(Effect::SetThinking { level })
            }
            // `/usage` (ADR 0027): the snapshot is already in the store —
            // the background source wrote it — so this reads it and prints; no effect, and no
            // network of its own.
            command::Outcome::Usage => {
                for line in self.usage_lines() {
                    self.push_notice(line);
                }
                UpdateResult::dirty()
            }
            command::Outcome::Model(switch) => UpdateResult::effect(Effect::ModelSwitch {
                provider: switch.provider,
                model: switch.model,
            }),
            // `/login`: the wizard is app state (pure); its network and file work are effects
            // the loop runs (ADR 0023 U3). An explicit `/login <id>` skips the choice step.
            command::Outcome::Login(provider) => {
                let step = match provider {
                    None => crate::login::LoginStep::ChooseProvider { cursor: 0 },
                    Some(id) if id.starts_with("no-such-provider:") => {
                        let name = id.trim_start_matches("no-such-provider:").to_owned();
                        self.push_notice(format!(
                            "unknown provider {name:?}: /login takes a built-in provider or \
                             `custom`"
                        ));
                        return UpdateResult::dirty();
                    }
                    // The two ChatGPT OAuth choices (ADR 0026) are not descriptors,
                    // so they do not go through `for_provider`: pushing the modal and kicking
                    // off the effect happens together, exactly as choosing them from the
                    // picker does in `LoginWizard::on_key`.
                    Some(id) if id == radian_host::CHATGPT_PROVIDER_ID => {
                        self.push_modal(Modal::Login(Box::new(LoginWizard::new(
                            crate::login::LoginStep::OAuthBrowserWaiting { url: None },
                        ))));
                        return UpdateResult::effect(Effect::LoginOAuthBrowser);
                    }
                    Some(id) if id == radian_host::CHATGPT_DEVICE_LOGIN => {
                        self.push_modal(Modal::Login(Box::new(LoginWizard::new(
                            crate::login::LoginStep::OAuthDeviceWaiting {
                                user_code: String::new(),
                                verification_url: String::new(),
                            },
                        ))));
                        return UpdateResult::effect(Effect::LoginOAuthDevice);
                    }
                    Some(id) => {
                        // for_provider builds the matching first step (D3).
                        self.push_modal(Modal::Login(Box::new(LoginWizard::for_provider(&id))));
                        return UpdateResult::dirty();
                    }
                };
                self.push_modal(Modal::Login(Box::new(LoginWizard::new(step))));
                UpdateResult::dirty()
            }
            // `/logout <provider>`: confirm first (`y`/`n`), then remove (D3).
            command::Outcome::Logout(provider) => {
                if provider.is_empty() {
                    self.push_notice("usage: /logout <provider>");
                    return UpdateResult::dirty();
                }
                if let Some(extra) = provider.strip_prefix("__extra__") {
                    self.push_notice(format!(
                        "unexpected extra name {extra:?}; usage: /logout <provider>"
                    ));
                    return UpdateResult::dirty();
                }
                self.push_modal(Modal::ConfirmLogout(provider));
                UpdateResult::dirty()
            }
            command::Outcome::AutoToggle(on) => {
                // ADR 0026 D9: the in-session switch. `/auto on` needs no confirmation (it is
                // typed, not one keypress), and a no-op switch is still recorded — the audit
                // line says who switched, so the journal keeps every command.
                self.session.auto = on;
                if on {
                    self.push_notice(
                        "auto: tool calls are approved without asking for the rest of this \
                         session; deny rules and the sandbox still apply",
                    );
                } else {
                    self.push_notice("auto off: tool asks prompt again");
                }
                UpdateResult::effect(Effect::SetApprovalPolicy { on })
            }
            // `/access` (ADR 0038 slice 2): a query with no argument, otherwise a switch. Only
            // `full` needs confirmation (it is "no sandbox"); the model has no tool for this.
            command::Outcome::Access(name) => match name {
                None => {
                    let current = self
                        .session
                        .projection
                        .read()
                        .tier()
                        .map(str::to_owned)
                        .unwrap_or_else(|| "unknown".to_owned());
                    self.push_notice(format!(
                        "access: {current} (from {})",
                        self.session.access_source.label()
                    ));
                    // ADR 0016: an inherited sandbox says so here too,
                    // rather than letting `workspace` read as radian's own rules.
                    if let Some(note) = self.session.sandbox_note.clone() {
                        self.push_notice(note);
                    }
                    UpdateResult::dirty()
                }
                Some(name) => {
                    let tier = match crate::profile::parse_tier(&name) {
                        Ok(tier) => tier,
                        Err(message) => {
                            self.push_notice(message);
                            return UpdateResult::dirty();
                        }
                    };
                    // The projection's tier may carry a source suffix (`full (project)`); the
                    // tier name is the first word.
                    let current = self
                        .session
                        .projection
                        .read()
                        .tier()
                        .map(str::to_owned)
                        .unwrap_or_default();
                    let current_tier = current.split_whitespace().next().unwrap_or("");
                    if current_tier == tier.as_str() {
                        self.push_notice(format!("access is already {}", tier.as_str()));
                        return UpdateResult::dirty();
                    }
                    if tier == crate::profile::Tier::FullAccess {
                        // `full` means no sandbox: confirm before opening it (ADR 0038
                        // decision 2). The panel's default focus is `cancel`.
                        self.push_modal(Modal::AccessFull(Box::default()));
                        return UpdateResult::dirty();
                    }
                    UpdateResult::effect(Effect::SetAccess {
                        tier,
                        remember: false,
                    })
                }
            },
            // `/skill:` (ADR 0025 S3): the host reads the body and the expanded text becomes a
            // user message. Whether it steers is decided here, like a typed send.
            command::Outcome::Skill { name, args } => {
                let steer = self.turn() == TurnState::Running;
                UpdateResult::effect(Effect::ExpandSkill { name, args, steer })
            }
            // `!`/`!!` (ADR 0033): executed by the loop in the sandbox. `!` also enqueues its
            // `Ran \`cmd\`` message for the model, but only once it has run.
            command::Outcome::Bash {
                command,
                exclude_from_context,
            } => self.begin_bash(command, exclude_from_context),
            // `/skills`: the host lists the discovered skills as notices.
            command::Outcome::Skills => UpdateResult::effect(Effect::ListSkills),
            command::Outcome::Jobs(request) => UpdateResult::effect(Effect::Jobs(request)),
            command::Outcome::Message => UpdateResult::none(),
        }
    }

    /// `/usage`'s lines (ADR 0027): the current provider's snapshot,
    /// every window of it. With no provider, no usage source for the routed provider, or no
    /// snapshot yet, one line says so instead.
    fn usage_lines(&self) -> Vec<String> {
        let Some(provider) = self.session.provider.as_deref() else {
            return vec!["usage: no provider is routed yet".to_owned()];
        };
        if !radian_host::has_usage_source(provider) {
            return vec![format!("usage: {provider} has no usage source")];
        }
        let Some(contribution) = self.contributions.get(radian_host::USAGE_ITEM) else {
            return vec![format!("usage: no snapshot yet for {provider}")];
        };
        let Some(snapshot) = contribution.usage else {
            return vec![format!("usage: no snapshot yet for {provider}")];
        };
        if snapshot.provider != provider {
            return vec![format!("usage: no snapshot yet for {provider}")];
        }
        command::usage_notices(&snapshot, contribution.stale)
    }

    /// The `/model` decision (ADR 0026 D5/D7). The loop has already refused a provider without
    /// a credential; here the app decides between queueing directly and asking first.
    fn model_checked(
        &mut self,
        provider: String,
        model: String,
        tokens: Option<u64>,
        budget: Option<u64>,
    ) -> UpdateResult {
        match (tokens, budget) {
            (Some(t), Some(b)) if t > b => {
                // D7: the switch does not fit, so compaction comes first — with the user's yes.
                let prompt = format!(
                    "switching to {provider}/{model} needs a compact first: about {t} tokens, \
                     over the new model's budget {b}. compact and switch? [y] compact and switch \
                     [n] cancel"
                );
                // The payload's `command` is unused on this path: `confirm_command` routes
                // on `switch` first, so `y` can never run a memory command. `Purge` is the
                // one variant a refusal can never reach.
                let command = radian_host::memory::commands::MemoryCommand::List {
                    global: false,
                    project: false,
                    all: false,
                    json: false,
                };
                self.push_modal(Modal::ConfirmCommand(PendingCommand {
                    command,
                    prompt,
                    switch: Some((provider, model, b)),
                }));
                UpdateResult::dirty()
            }
            _ => self.queue_model(provider, model),
        }
    }

    /// Queues the route (D5) and says what happens next. The cache warning is the accepted
    /// cost of a switch (ADR 0026 已接受的代价).
    fn queue_model(&mut self, provider: String, model: String) -> UpdateResult {
        self.session.next_model = Some(format!("{provider}/{model}"));
        self.push_notice(format!(
            "next turn: {provider}/{model}; the first turn after a switch will not hit the cache"
        ));
        UpdateResult::effect(Effect::QueueModel { provider, model })
    }

    /// The clamped `/thinking` (D11): queue it, and say so when the clamp moved the level.
    fn thinking_checked(
        &mut self,
        requested: String,
        applied: Option<String>,
        supported: Vec<String>,
    ) -> UpdateResult {
        let Some(applied) = applied else {
            self.push_notice("this model supports no thinking level; nothing changed");
            return UpdateResult::dirty();
        };
        if applied != requested {
            self.push_notice(format!(
                "{requested} → {applied} (this model supports {})",
                supported.join("/")
            ));
        }
        self.session.next_thinking = Some(applied.clone());
        self.push_notice(format!("next turn: thinking {applied}"));
        UpdateResult::effect(Effect::QueueThinking { level: applied })
    }

    /// The pre-switch compaction finished (D7): only success queues the switch.
    fn switch_compacted(
        &mut self,
        provider: String,
        model: String,
        result: Result<CompactionOutcome, String>,
    ) -> UpdateResult {
        match result {
            Ok(_) => {
                self.push_notice(format!("compacted; switching to {provider}/{model}"));
                self.queue_model(provider, model)
            }
            Err(error) => {
                self.push_notice(format!(
                    "the switch was not queued: the compaction failed ({error})"
                ));
                UpdateResult::dirty()
            }
        }
    }

    /// Pushes a notice entry. Public so the event loop can show a `/compact` result.
    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.session
            .entries
            .push(Entry::new(EntryKind::Notice, text));
    }

    /// Applies a live event. Deltas build `live` entries; the committed message
    /// (`TranscriptAppended`) replaces them, so a dropped delta can never leave a duplicate.
    /// Returns the effects the event implies (clearing an approval prompt abandons its reply
    /// channel, which the loop owns).
    pub fn apply_event(&mut self, event: &AgentEvent) -> Vec<Effect> {
        let mut effects = Vec::new();
        match event {
            // ADR 0023 U5: the turn, budget, usage and tool facts are written to the session
            // projection by the event sink (`ProjectionSink`), never here. `App` handles only
            // the transcript, the note and the effects.
            AgentEvent::TurnStarted => {
                // The turn is being written now; a `/fork` must wait for its `TurnFinished`.
                self.session.turn_committed = false;
            }
            // ADR 0027: the local turn timer starts with the model request and
            // stops with its response. The token count is the model's reported output.
            AgentEvent::ModelRequestStarted { .. } => {
                // A model request starts only after the batch before it ended (the turn loop is
                // request -> batch -> request), so nothing in the running set can still be
                // running here. A call left in it is a `ToolStarted` whose `ToolFinished` never
                // came — the core discards the results of the calls already running when a
                // journal append fails (`agent-core/src/turn.rs`, the `journal_failed` path) —
                // and the note must not outlive the batch it belongs to.
                self.session.forget_running_tools();
                self.session.timer.request_started(self.now);
            }
            AgentEvent::ModelResponseReceived { usage, .. } => {
                self.session
                    .timer
                    .response_finished(self.now, usage.as_ref().and_then(|usage| usage.output));
            }
            // ADR 0017: a transient failure is being retried. The failed attempt's
            // streamed text is discarded (it was live-only), and the status line names the retry.
            AgentEvent::ModelRetrying {
                attempt,
                max,
                delay,
                reason,
            } => {
                self.drop_live();
                self.session.note = Some(format!(
                    "retrying {attempt}/{max} in {}s: {reason}",
                    delay.as_secs()
                ));
            }
            AgentEvent::ToolStarted { call_id, name, .. } => {
                self.session.start_tool(call_id, name);
            }
            AgentEvent::ToolFinished { call_id, .. } => {
                // The tool is done. The note follows the calls that are **still** running: it
                // moves off the settled one when that was the call it named, and empties when
                // the last tool of the batch finishes, so the footer never names a settled tool
                // (the next model request finds it clear).
                self.session.finish_tool(call_id);
            }
            AgentEvent::TranscriptAppended { message, .. } => self.push_message(message),
            // ADR 0018: compaction is visible to the user as it happens.
            AgentEvent::CompactionStarted { tokens_before, .. } => {
                self.session.note = Some("compacting context".to_string());
                self.session.compacting = true;
                self.session.entries.push(Entry::new(
                    EntryKind::Notice,
                    format!("context over the budget ({tokens_before} tokens); compacting…"),
                ));
            }
            AgentEvent::CompactionFinished {
                revision,
                tokens_before,
                tokens_after,
                deterministic,
                dropped,
                ..
            } => {
                self.session.note = None;
                self.session.compacting = false;
                // ADR 0018, decision 3: a deterministic fallback says so and names
                // how many messages it dropped; a model summary keeps the token line.
                let text = if *deterministic {
                    format!("compacted without a summary ({dropped} dropped)")
                } else {
                    format!("context compacted ({tokens_before} → {tokens_after} tokens)")
                };
                self.session
                    .entries
                    .push(Entry::new(EntryKind::Notice, text));
                // The summary message arrives right after this event (ADR 0018): the numbers
                // are handed to it, since the journaled message does not carry them.
                self.session.pending_compaction = Some((
                    *revision,
                    *tokens_before,
                    *tokens_after,
                    *deterministic,
                    *dropped,
                ));
            }
            AgentEvent::CompactionFailed { reason } => {
                self.session.note = None;
                self.session.compacting = false;
                self.session.entries.push(Entry::new(
                    EntryKind::Notice,
                    format!("compaction failed: {reason}"),
                ));
            }
            // A skip means nothing was summarized (ADR 0018): the flag must not stick.
            AgentEvent::CompactionSkipped { .. } => {
                self.session.compacting = false;
            }
            AgentEvent::RunFinished { status: _ } => {
                // The turn is over. A pending approval is not cleared here: the permission
                // layer raced the wait against the turn's cancellation, so its waiter is gone,
                // the queue drops the element and wakes the UI (ADR 0023 U7). Driving the modal
                // from the queue alone is what makes "the waiter gave up" and "the turn ended"
                // the same event.
                self.session.note = None;
                self.session.running_tools.clear();
                self.session.compacting = false;
                // ADR 0033: a `!` submitted behind the turn starts now.
                if let Some((command, exclude_from_context)) = self.session.queued_bash.take() {
                    effects.extend(self.begin_bash(command, exclude_from_context).effects);
                }
            }
            _ => {}
        }
        effects
    }

    /// Sets the live entry of `kind` to `text` (creating it when it does not exist yet). The
    /// stream arrives as a snapshot, so this replaces rather than appends. The reasoning
    /// channel's clock starts at its **first** delta (ADR 0028), from
    /// the loop's injected instant (`self.now`): a redraw never reads the wall clock.
    fn set_live(&mut self, kind: EntryKind, text: &str) {
        match self
            .session
            .entries
            .iter_mut()
            .rev()
            .find(|entry| entry.live && entry.kind == kind)
        {
            Some(entry) => {
                if kind == EntryKind::Reasoning && entry.reasoning_started.is_none() {
                    entry.reasoning_started = Some(self.now);
                }
                entry.text = text.to_owned();
            }
            None => {
                let mut entry = Entry::new(kind, text);
                entry.live = true;
                if kind == EntryKind::Reasoning {
                    entry.reasoning_started = Some(self.now);
                }
                self.session.entries.push(entry);
                self.session.live_count += 1;
            }
        }
    }

    /// The live reasoning channel's clock at the commit (ADR 0028): its
    /// first delta's instant and this one, the entry's end. `None` when this message does not
    /// replace a live reasoning entry — a resume has no live view, so a resumed thinking block
    /// has no duration. Must run **before** [`App::drop_live`], which is the entry's end.
    fn take_live_reasoning_clock(&mut self) -> Option<(Instant, Instant)> {
        // The same guard as `drop_live`: a resume pushes every message with no live entry, and
        // an unconditional reverse scan per message was the O(n^2) family `drop_live`'s count
        // fixed (resume-perf A).
        if self.session.live_count == 0 {
            return None;
        }
        let now = self.now;
        let entry = self
            .session
            .entries
            .iter_mut()
            .rev()
            .find(|entry| entry.live && entry.kind == EntryKind::Reasoning)?;
        let started = entry.reasoning_started?;
        entry.reasoning_ended = Some(now);
        Some((started, now))
    }

    /// Discards every live (streaming) entry. A committed message replaces them
    /// (`push_message`), and so does a retry: the failed attempt's deltas are live-only
    /// (ADR 0010/0017 revision 1). The scan runs only when one exists (resume-perf A).
    fn drop_live(&mut self) {
        if self.session.live_count > 0 {
            let mut scanned = 0u64;
            self.session.entries.retain(|entry| {
                scanned += 1;
                !entry.live
            });
            self.session.live_count = 0;
            #[cfg(test)]
            LIVE_SCANS.with(|count| count.set(count.get() + scanned));
        }
    }

    /// Appends a committed message as transcript entries. Used for live commits and for the
    /// history of a resumed session.
    pub fn push_message(&mut self, message: &Message) {
        // The committed message is the truth; discard the partial live view of it. The scan
        // runs **only when a live entry exists**: a resume pushes every message before any
        // stream has started, and an unconditional `retain` over a 66k-entry transcript per
        // message was O(n^2) (resume-perf A). The count is the number of live entries, so a
        // live entry is dropped wherever it sits - the stream can leave one behind a
        // compaction notice (they are not always a suffix of `entries`).
        // The live reasoning channel's clock, taken before the drop (ADR 0028
        // decision 4): the committed entry below carries its duration, so `◻ thinking · 3.2s`
        // survives the live-to-committed handoff. A resume has no live entry: `None`.
        let reasoning_clock = self.take_live_reasoning_clock();
        self.drop_live();
        // ADR 0033: a `!` command's `Ran \`cmd\`` context message is a Bash block, not user
        // prose. The one we already drew live is skipped so it is not shown twice; a resumed
        // one (no pending text) is drawn as a block.
        if let Message::User { .. } = message {
            let text = message.text();
            if self.session.bash_context_pending.as_deref() == Some(text.as_str()) {
                self.session.bash_context_pending = None;
                return;
            }
            if let Some(entry) = bash_entry_from_context(&text) {
                self.session.entries.push(entry);
                return;
            }
        }
        match message {
            Message::User { content, .. } => {
                // A compaction summary (ADR 0028 R5) is a `User` message with a
                // `compaction_summary` origin (ADR 0018); it shows as one line, not as a wall
                // of user text. Core's readers supply the summary and the verbatim user
                // words - nothing is parsed out of the body here.
                if agent_core::compaction::is_compaction_summary(message) {
                    let mut info = CompactionInfo {
                        summary: agent_core::compaction::compaction_summary_text(message)
                            .unwrap_or_default(),
                        user_messages: agent_core::compaction::compaction_user_messages(message)
                            .into_iter()
                            .map(|message| message.text)
                            .collect(),
                        ..CompactionInfo::default()
                    };
                    // The live path knows the revision and the token counts from the
                    // `CompactionFinished` event that preceded this message; a resume replay
                    // does not, and omits them.
                    if let Some((revision, before, after, deterministic, dropped)) =
                        self.session.pending_compaction.take()
                    {
                        info.revision = Some(revision);
                        info.tokens_before = Some(before);
                        info.tokens_after = Some(after);
                        info.deterministic = deterministic;
                        // `dropped` is 0 for a model summary; only a deterministic compaction
                        // names it (ADR 0018, decision 3).
                        info.dropped = deterministic.then_some(dropped);
                    }
                    let mut entry = Entry::new(EntryKind::Compaction, info.display_text());
                    entry.compaction = Some(Box::new(info));
                    self.session.entries.push(entry);
                    return;
                }
                // ADR 0018: an attachment is its own part, so the display renders
                // it as `[file: P, N lines]` rather than joining its body into the message.
                self.session.entries.push(Entry::new(
                    EntryKind::User,
                    crate::mention::display_parts(content),
                ));
            }
            Message::Assistant {
                content,
                tool_calls,
                reasoning,
                interrupted,
                ..
            } => {
                let reasoning = text_of(reasoning);
                if !reasoning.is_empty() {
                    let mut entry = Entry::new(EntryKind::Reasoning, reasoning);
                    // The duration measured live, carried onto the committed entry (ADR 0028
                    // revision 5 decision 4). It rides only here: a resumed message has no
                    // clock, so its thinking block shows no duration.
                    if let Some((started, ended)) = reasoning_clock {
                        entry.reasoning_started = Some(started);
                        entry.reasoning_ended = Some(ended);
                    }
                    self.session.entries.push(entry);
                }
                let mut entry = Entry::new(EntryKind::Assistant, text_of(content));
                entry.interrupted = *interrupted;
                self.session.entries.push(entry);
                // One card per call (ADR 0028 R4): each carries its own name and arguments,
                // and its text is the same header the renderer will show, so the head stays
                // selectable and copyable character for character.
                for call in tool_calls {
                    let card = crate::tools::ToolCard::new(
                        call.id.clone(),
                        call.name.clone(),
                        call.arguments.clone(),
                    );
                    // The call-time head is the tool name and the renderer's title (ADR 0028
                    // revision 5 decision 1); the meta and the status glyph are the block
                    // layer's, drawn from the card and the entry's state. The pairing refreshes
                    // the title once the result is in (a `git_diff` title is terse until then).
                    let mut entry =
                        Entry::new(EntryKind::ToolCall, crate::tools::call_text(&card, ""));
                    entry.tool = Some(Box::new(card));
                    // A call with no result yet is a whole card of its own (ADR 0028 revision
                    // 2); the pairing below turns it into the head of the merged card.
                    entry.card = crate::blocks::Card::Solo;
                    self.session.entries.push(entry);
                }
            }
            Message::Tool { result, .. } => {
                // The result pairs with its call **by id** (the same key the live and the
                // resume paths carry), so a batch of calls pairs correctly whatever order
                // their results arrive in.
                //
                // The pairing is a **single** scan from the back: the call is the most recent
                // matching card, and the index it found is reused for the insertion below.
                // (A second front `position` for the index walked the whole transcript per
                // result — the same O(n^2) family as the `retain` resume-perf A fixed.)
                let mut paired: Option<(String, serde_json::Value)> = None;
                let mut call_index: Option<usize> = None;
                let len = self.session.entries.len();
                for (rev, entry) in self.session.entries.iter_mut().rev().enumerate() {
                    #[cfg(test)]
                    TOOL_PAIR_SCANS.with(|count| count.set(count.get() + 1));
                    if entry.kind == EntryKind::ToolCall
                        && entry
                            .tool
                            .as_ref()
                            .is_some_and(|card| card.call_id == result.call_id)
                    {
                        entry.result = Some(result.is_error);
                        if let Some(card) = entry.tool.as_mut() {
                            card.structured = result.structured.clone();
                            // The head's title can only be final now (a `git_diff`'s counts
                            // come from the result text), so the entry's text - what renders
                            // and copies - is regenerated from the same function the layout
                            // uses. The result's own row count is part of the head's collapsed
                            // meta (`· N lines`), and only this entry can see the result text.
                            let display = crate::tools::result_display_text(card, &result.text());
                            card.result_lines = Some(crate::tools::body_line_count(&display));
                            // ADR 0024: the call entry draws the whole ask block
                            // (its result entry is blank), so it keeps the result's own text
                            // for the declined/cancelled status line inside the box.
                            entry.text = if card.name == "ask_user" {
                                result.text()
                            } else {
                                crate::tools::call_text(card, &result.text())
                            };
                            paired = Some((card.name.clone(), card.arguments.clone()));
                        }
                        call_index = Some(len - 1 - rev);
                        break;
                    }
                }
                let (name, arguments) = paired.unwrap_or_default();
                let mut card = crate::tools::ToolCard::new(result.call_id.clone(), name, arguments);
                card.structured = result.structured.clone();
                // The body's text **is** the entry's text (the mapping/search invariant): a
                // `patch` shows the synthesized diff, a `write` the content written, every
                // other tool its own result text.
                let display = crate::tools::result_display_text(&card, &result.text());
                let mut entry = Entry::new(EntryKind::ToolResult, display);
                entry.is_error = result.is_error;
                entry.error_code = result.error_code.clone();
                entry.tool = Some(Box::new(card));
                entry.card = crate::blocks::Card::Solo;
                // Rule 4: the result sits right after **its own** call, so a batch reads
                // call/result, call/result instead of every call then every result. The
                // entries are the UI's own order; the journal keeps the message order, so a
                // resume replay runs this same code and lands on the same order. A result
                // whose call is gone (a torn transcript) is appended.
                match call_index {
                    Some(index) => {
                        // The call heads the merged card and this result closes it (ADR 0028
                        // revision 2): the drawing merges them, the data stays two entries.
                        self.session.entries[index].card = crate::blocks::Card::Head;
                        entry.card = crate::blocks::Card::Tail;
                        self.session.entries.insert(index + 1, entry);
                        // The insertion shifts every later entry: a selection anchored to one
                        // of them moves with it.
                        self.shift_anchors(index + 1);
                    }
                    None => self.session.entries.push(entry),
                }
            }
        }
    }
}

// Test-only: how many entries `push_message` has walked to drop live ones. A unit test
// asserts it does not grow when no live entry exists, which is the property the O(n^2) fix
// rests on (it is not wall-clock based). Thread-local so parallel tests do not interfere.
#[cfg(test)]
thread_local! {
    pub(crate) static LIVE_SCANS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

// Test-only: how many entries the tool-result pairing has walked. A unit test asserts it stays
// linear in the transcript when every result pairs with the most recent call, which is the
// property the O(n^2) fix rests on (no wall clock). Thread-local for the same reason as above.
#[cfg(test)]
thread_local! {
    pub(crate) static TOOL_PAIR_SCANS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The `@`-mention slice needs exactly one `App` method. Kept in its own block at the end of
/// the file so it merges cleanly with the composer work that owns the rest of the input
/// handling.
impl App {
    /// Puts `text` back into the input box. An expansion failure must not lose the message
    /// the user typed.
    pub fn restore_input(&mut self, text: String) {
        self.input.set_text(&text);
    }

    /// Records the workspace root, so `Tab` can complete `@` file references against it.
    pub fn set_workspace(&mut self, workspace: impl Into<PathBuf>) {
        self.session.workspace = Some(workspace.into());
    }

    /// The footer's startup facts (ADR 0027 C2): the working directory, the model's display
    /// name and provider, and the applied thinking level.
    pub fn set_footer_facts(
        &mut self,
        cwd: Option<PathBuf>,
        model_display: Option<String>,
        provider: Option<String>,
        thinking_level: Option<String>,
    ) {
        self.session.cwd = cwd;
        self.session.model_display = model_display;
        self.session.provider = provider;
        self.session.thinking_level = thinking_level;
    }

    /// The background-refreshed git branch (ADR 0027 C2: the draw path never runs git). The
    /// host's `GitContributor` is the normal writer now; this setter remains for a host that
    /// has a first value without the contributor.
    pub fn set_git_branch(&mut self, branch: Option<String>) {
        self.session.git_branch = branch;
    }

    /// The machine's hostname (ADR 0027), read once at startup.
    pub fn set_hostname(&mut self, hostname: Option<String>) {
        self.hostname = hostname;
    }

    /// The model's catalog unit prices (ADR 0027). Written through to the
    /// projection, which is what prices each arriving call: the footer reads the accumulated
    /// fee from there, because a call keeps the price it arrived under even after a switch.
    pub fn set_model_price(&mut self, price: Option<radian_host::ModelPrice>) {
        self.session.model_price = price.clone();
        self.session.projection.set_price(price);
    }

    /// Whether automatic compaction is on (ADR 0027).
    pub fn set_auto_compact(&mut self, on: bool) {
        self.auto_compact = on;
    }

    /// The loop's current instant (ADR 0027): the timer never reads the clock.
    pub fn set_now(&mut self, now: Instant) {
        self.now = now;
    }

    /// The spinner's current frame (ADR 0027/13): a function of the injected clock,
    /// so a test advances it with `set_now`. Whether the pulse interpolates comes from the theme
    /// (and the glyph set); a frame is a pure function of its inputs, which is what makes
    /// "redraw only when it changed" a comparison of frames.
    pub fn spinner_frame(&self) -> crate::spinner::Frame {
        let tick = self
            .now
            .saturating_duration_since(self.session.spinner_origin);
        crate::spinner::frame(
            self.spinner_kind,
            self.footer_glyphs(),
            self.theme.pulse_can_breathe(),
            tick,
        )
    }

    /// How long the spinner's current frame stays (ADR 0027/13): the breath's step
    /// while the pulse interpolates, its 500 ms two-colour switch when it cannot, and the
    /// rotating kinds' 250 ms. The loop wakes at this interval while a spinner is visible.
    pub fn spinner_interval(&self) -> Duration {
        crate::spinner::frame_interval(
            self.spinner_kind,
            self.footer_glyphs(),
            self.theme.pulse_can_breathe(),
        )
    }

    /// The style for a spinner frame: the pulse breathes between the accent and the dim level
    /// when both ends are RGB (ADR 0027); otherwise it is revision 10's two-colour
    /// switch, and the rotating frames stay on the accent.
    pub fn spinner_style(&self, frame: crate::spinner::Frame) -> ratatui::style::Style {
        // One definition, shared with the transcript's running tool head (`crate::render`).
        crate::render::spinner_style(&self.theme, frame)
    }

    /// The input rule's status text (ADR 0027): the most specific running state, or
    /// `None` when the app is idle (the rule is then plain). The states are the ones the UI
    /// already has: a modal waiting for the user, a running turn, a running `!`.
    pub fn spinner_status(&self) -> Option<&'static str> {
        if matches!(self.top_modal(), Some(Modal::Approval(_))) {
            return Some("Waiting for approval");
        }
        if matches!(self.top_modal(), Some(Modal::Question(_))) {
            return Some("Waiting for your answer");
        }
        // A running compaction is the most specific state (ADR 0018).
        if self.session.compacting {
            return Some("Compacting");
        }
        if self.turn_running() {
            return Some("Working");
        }
        if self.session.bash_running.is_some() {
            return Some("Running a command");
        }
        None
    }

    /// Whether a turn is running (the projection's own state, ADR 0023 U5). The `bar` status
    /// row shows `esc to interrupt` exactly then: Esc cancels a running turn (ADR 0009).
    pub fn turn_running(&self) -> bool {
        self.turn() == TurnState::Running
    }

    /// Whether any spinner is on screen (ADR 0027): the input rule's status line,
    /// a live reasoning line, or a running tool card. Only then does the loop animate.
    pub fn spinner_visible(&self) -> bool {
        self.spinner_status().is_some()
            || self.session.entries.iter().any(|entry| entry.live)
            || self
                .session
                .entries
                .iter()
                .any(|entry| entry.kind == EntryKind::ToolCall && entry.result.is_none())
    }

    /// Whether this session has a shell for `!` (ADR 0033): false on the `safe` tier, which
    /// has no sandbox at all (ADR 0012).
    pub fn set_bash_available(&mut self, available: bool) {
        self.session.bash_available = available;
    }

    /// The contributor store the renderer reads (ADR 0027 C3).
    pub fn set_contributions(
        &mut self,
        contributions: std::sync::Arc<radian_host::ContributionStore>,
    ) {
        self.contributions = contributions;
    }

    /// Installs the shared footer-facts handoff the loop publishes into and a background
    /// `command` item reads (ADR 0027).
    pub fn set_footer_context(&mut self, context: std::sync::Arc<radian_host::FooterContext>) {
        self.footer_context = context;
    }

    /// The footer facts a `command` item may read (ADR 0027). Only facts
    /// the footer already displays: the model, the cwd, the branch, the context proportion, the
    /// token totals, the fee and the task counts. A missing fact is `None`, never a fabricated
    /// number (v5 §5). The git branch prefers the background contributor's value and falls back
    /// to the host's one-shot read.
    pub fn footer_snapshot(&self) -> radian_host::FooterSnapshot {
        let session = self.session.projection.read();
        let branch = self
            .contributions
            .get("git_branch")
            .map(|contribution| {
                contribution
                    .segments
                    .iter()
                    .map(|segment| segment.text.as_str())
                    .collect::<String>()
            })
            .filter(|branch| !branch.is_empty())
            .or_else(|| self.session.git_branch.clone());
        radian_host::FooterSnapshot {
            model: Some(self.session.model_name.clone()),
            cwd: self
                .session
                .cwd
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            branch,
            context_percent: crate::footer::context_fraction(&session)
                .map(|fraction| fraction * 100.0),
            tokens_input: session.usage_input(),
            tokens_output: session.usage_output(),
            tokens_cache_read: session.usage_cache_read(),
            cost_usd: session
                .cost_micros()
                .map(|micros| micros as f64 / 1_000_000.0),
            // ADR 0043 decision 1 deleted the task registry, so nothing produces a task count;
            // the fields stay (ADR 0027's document shape) and are null.
            tasks_running: None,
            tasks_total: None,
        }
    }

    /// Publishes the current footer facts for the background `command` items (ADR 0027 revision
    /// 14 decision 4). A pure state read plus one lock write; the command reads the latest
    /// snapshot when it runs, so the UI never waits for it.
    pub fn publish_footer_context(&self) {
        self.footer_context.set(self.footer_snapshot());
    }

    /// The applied thinking level, once the driver reports it (ADR 0026 D11).
    pub fn set_thinking_level(&mut self, level: Option<String>) {
        self.session.thinking_level = level;
    }

    /// The footer's glyph set, for the items that draw an icon.
    pub fn footer_glyphs(&self) -> crate::footer::Glyphs {
        self.footer.glyphs()
    }

    /// The brand mark (ADR 0027): `tui.brand` when set, else the glyph set's
    /// default. The footer's `brand` item and the startup banner's one-line fallback share it,
    /// so the two can never disagree.
    pub fn brand_mark(&self) -> &str {
        self.brand
            .as_deref()
            .unwrap_or_else(|| crate::footer::brand_mark(self.footer_glyphs()))
    }

    /// Applies `[tui.footer]` (ADR 0027 C2). Returns the footer's warnings (unknown item,
    /// placeholder, style), shown once at startup.
    pub fn apply_footer(&mut self, config: &radian_host::config::TuiFooterConfig) -> Vec<String> {
        let (footer, warnings) = crate::footer::Footer::from_config(config);
        self.footer = footer;
        warnings
    }

    /// Applies one `@` completion (Phase 4):
    ///
    /// * one candidate: insert it; a directory keeps its trailing `/`, a file gets a trailing
    ///   space so the next word can be typed;
    /// * several: extend the word to their longest common prefix; a second `Tab` already at
    ///   that prefix shows the candidate list (the renderer draws `self.completion`).
    ///
    /// Reading the directory happens in the loop; this is only the state change.
    fn apply_completion(&mut self, completion: Completion) {
        if completion.candidates.len() == 1 {
            let candidate = &completion.candidates[0];
            let insert = if candidate.ends_with('/') {
                candidate.clone()
            } else {
                format!("{candidate} ")
            };
            self.replace_range(completion.range, &insert);
            self.completion_clear();
            return;
        }
        let chars: Vec<char> = self.input.text().chars().collect();
        let typed: String = chars[completion.range.clone()].iter().collect();
        if completion.common_prefix.chars().count() > typed.chars().count() {
            self.replace_range(completion.range, &completion.common_prefix);
            self.completion_clear();
            return;
        }
        // The menu is up: keep the raw candidates so the highlighted row can be accepted.
        // `completion_set` clears the previous menu (including this field), so it is set after.
        let raw = completion.candidates.clone();
        self.completion_set(
            completion
                .candidates
                .iter()
                .map(|candidate| crate::command::Candidate {
                    insert: candidate.clone(),
                    description: String::new(),
                    current: false,
                })
                .collect(),
        );
        self.mention_completion = Some((completion.range, raw));
    }

    /// Replaces the char range `range` with `replacement`, leaving the cursor right after the
    /// replacement and the rest of the text unchanged.
    fn replace_range(&mut self, range: std::ops::Range<usize>, replacement: &str) {
        let chars: Vec<char> = self.input.text().chars().collect();
        let mut text: Vec<char> = Vec::with_capacity(chars.len() + replacement.len());
        text.extend_from_slice(&chars[..range.start]);
        text.extend(replacement.chars());
        let after = chars.len() - range.end;
        text.extend_from_slice(&chars[range.end..]);
        self.input.set_text(&text.into_iter().collect::<String>());
        for _ in 0..after {
            self.input.left();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0027: the spinner frame follows the injected clock, not the text, and
    /// the configured kind decides the glyphs.
    #[test]
    fn the_spinner_frame_follows_the_injected_clock() {
        let mut app = App::new("m", "s");
        app.spinner_kind = crate::spinner::Kind::Arc;
        let base = app.session.spinner_origin;
        app.set_now(base);
        assert_eq!(app.spinner_frame().glyph, "◜");
        app.set_now(base + std::time::Duration::from_millis(250));
        assert_eq!(app.spinner_frame().glyph, "◝");
        app.set_now(base + std::time::Duration::from_millis(500));
        assert_eq!(app.spinner_frame().glyph, "◞");

        // The default pulse keeps its glyph and only changes its colour: with a non-RGB theme
        // (revision 13 decision 3) it is revision 10's two-colour switch, every 500 ms.
        app.spinner_kind = crate::spinner::Kind::Pulse;
        app.theme = crate::theme::Theme::mono();
        app.set_now(base);
        assert_eq!(app.spinner_frame().step, crate::spinner::PULSE_ACCENT_STEP);
        app.set_now(base + std::time::Duration::from_millis(500));
        assert_eq!(app.spinner_frame().step, crate::spinner::PULSE_DIM_STEP);
        // An RGB theme breathes instead: the step advances every 125 ms.
        app.theme = crate::theme::Theme::dark();
        app.set_now(base);
        assert_eq!(app.spinner_frame().step, crate::spinner::PULSE_DIM_STEP);
        app.set_now(base + app.spinner_interval());
        assert!(
            app.spinner_frame().step > crate::spinner::PULSE_DIM_STEP,
            "the second sample is already brighter"
        );
        assert_eq!(app.spinner_interval(), crate::spinner::pulse_step());
    }

    /// ADR 0018 + ADR 0027: the compaction events drive the `Compacting` status.
    #[test]
    fn the_compacting_status_follows_the_compaction_events() {
        let mut app = App::new("m", "s");
        app.session.projection.set_turn(TurnState::Running);
        assert_eq!(app.spinner_status(), Some("Working"));

        app.apply_event(&AgentEvent::CompactionStarted {
            revision: 1,
            tokens_before: 100,
            keep_target: 50,
            trigger: agent_core::CompactionTrigger::Budget,
        });
        assert_eq!(app.spinner_status(), Some("Compacting"));

        app.apply_event(&AgentEvent::CompactionFinished {
            revision: 1,
            tokens_before: 100,
            tokens_after: 40,
            below_target: false,
            deterministic: false,
            dropped: 0,
        });
        assert_eq!(app.spinner_status(), Some("Working"));

        // A failed or skipped compaction clears it too.
        app.apply_event(&AgentEvent::CompactionStarted {
            revision: 2,
            tokens_before: 100,
            keep_target: 50,
            trigger: agent_core::CompactionTrigger::Manual,
        });
        app.apply_event(&AgentEvent::CompactionFailed {
            reason: "boom".to_string(),
        });
        assert_eq!(app.spinner_status(), Some("Working"));
        app.apply_event(&AgentEvent::CompactionStarted {
            revision: 3,
            tokens_before: 100,
            keep_target: 50,
            trigger: agent_core::CompactionTrigger::Manual,
        });
        app.apply_event(&AgentEvent::CompactionSkipped {
            reason: "already fits".to_string(),
        });
        assert_eq!(app.spinner_status(), Some("Working"));
    }

    /// ADR 0018, decision 3: a deterministic compaction shows "compacted without a
    /// summary (N dropped)", and the summary entry's folded line says "without a summary".
    #[test]
    fn a_deterministic_compaction_is_shown_without_a_summary() {
        let mut app = App::new("m", "s");
        app.apply_event(&AgentEvent::CompactionFinished {
            revision: 2,
            tokens_before: 900,
            tokens_after: 120,
            below_target: false,
            deterministic: true,
            dropped: 7,
        });
        assert!(
            app.session
                .entries
                .iter()
                .any(|entry| entry.text == "compacted without a summary (7 dropped)"),
            "the notice names the count: {:?}",
            app.session
                .entries
                .iter()
                .map(|e| &e.text)
                .collect::<Vec<_>>()
        );

        let message = agent_core::compaction::compacted_history(
            "[context compacted without a summary: 7 messages dropped, 0 tool results truncated]",
            &[],
            &[],
        )
        .unwrap();
        app.push_message(&message[0]);
        let info = app
            .session
            .entries
            .iter()
            .find_map(|entry| entry.compaction.as_deref())
            .expect("the summary entry");
        assert!(info.deterministic);
        assert_eq!(info.dropped, Some(7));
        assert!(
            info.line().contains("without a summary") && info.line().contains("7 dropped"),
            "{}",
            info.line()
        );
    }

    /// ADR 0017: a retry names the attempt on the status line and discards the
    /// failed attempt's partial stream.
    #[test]
    fn a_retrying_event_drops_the_live_block_and_names_the_attempt() {
        let mut app = App::new("m", "s");
        app.set_live(EntryKind::Assistant, "half an answer");
        assert_eq!(app.session.live_count, 1);

        app.apply_event(&AgentEvent::ModelRetrying {
            attempt: 1,
            max: 3,
            delay: std::time::Duration::from_secs(4),
            reason: "the stream ended before the response was complete".into(),
        });

        assert_eq!(
            app.session.live_count, 0,
            "the failed attempt's partial text must be gone"
        );
        assert!(app.session.entries.iter().all(|entry| !entry.live));
        assert_eq!(
            app.session.note.as_deref(),
            Some("retrying 1/3 in 4s: the stream ended before the response was complete")
        );
    }

    fn a_run(exit_code: Option<i32>, stdout: &str) -> radian_coding::UserBash {
        radian_coding::UserBash {
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_code,
            signal: None,
            timed_out: false,
            cancelled: false,
        }
    }

    /// ADR 0033: a `!` line parses to a `RunBash` effect, and the command is remembered so a
    /// second one is refused while it runs.
    #[test]
    fn a_bang_routes_to_a_run_bash_effect() {
        let mut app = App::new("m", "s");
        let result = app.run_slash_command("!echo hi");
        assert_eq!(
            result.effects,
            vec![Effect::RunBash {
                command: "echo hi".to_owned(),
                exclude_from_context: false,
            }]
        );
        assert_eq!(app.session.bash_running.as_deref(), Some("echo hi"));
        // A second one is refused (one at a time).
        let second = app.run_slash_command("!echo again");
        assert!(second.effects.is_empty());
        assert!(
            app.session
                .entries
                .last()
                .unwrap()
                .text
                .contains("already running")
        );
    }

    /// ADR 0012: the `safe` tier has no shell, so `!` never runs there.
    #[test]
    fn a_bang_on_the_safe_tier_does_not_run() {
        let mut app = App::new("m", "s");
        app.set_bash_available(false);
        let result = app.run_slash_command("!echo hi");
        assert!(result.effects.is_empty());
        assert!(app.session.bash_running.is_none());
        assert!(
            app.session.entries.last().unwrap().text.contains("safe"),
            "{}",
            app.session.entries.last().unwrap().text
        );
    }

    /// ADR 0033: while a turn runs, `!` waits for it to end rather than racing it.
    #[test]
    fn a_bang_queues_behind_a_running_turn() {
        let mut app = App::new("m", "s");
        app.session.projection.set_turn(TurnState::Running);
        let result = app.run_slash_command("!echo hi");
        assert!(result.effects.is_empty());
        assert_eq!(
            app.session.queued_bash,
            Some(("echo hi".to_owned(), false)),
            "queued, not run"
        );
        // The event sink writes the Idle fact before the app sees the event; the test does
        // the same.
        app.session.projection.set_turn(TurnState::Idle);
        let effects = app.apply_event(&AgentEvent::RunFinished {
            status: agent_core::RunStatus::Completed,
        });
        assert!(
            matches!(effects.as_slice(), [Effect::RunBash { command, .. }] if command == "echo hi"),
            "{effects:?}"
        );
        assert_eq!(app.session.bash_running.as_deref(), Some("echo hi"));
    }

    /// ADR 0016: `/access` with no argument says what the startup line
    /// says when the sandbox is one radian inherited from an outer sandbox, so `workspace` does
    /// not read as radian's own rules. Mutation: drop the note (this test's second line).
    #[test]
    fn access_repeats_an_inherited_sandbox_line() {
        let mut app = App::new("m", "s");
        app.set_sandbox_note(Some(
            "workspace · sandbox inherited (an outer sandbox confines this process; radian's own \
             rules are not applied)"
                .to_owned(),
        ));
        app.run_slash_command("/access");
        let lines: Vec<&str> = app
            .session
            .entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect();
        assert!(
            lines.iter().any(|line| line.contains("access: ")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("sandbox inherited")
                && line.contains("radian's own rules are not applied")),
            "the inherited sandbox line is missing: {lines:?}"
        );

        // A session whose sandbox is not inherited adds nothing.
        let mut app = App::new("m", "s");
        app.run_slash_command("/access");
        assert!(
            !app.session
                .entries
                .iter()
                .any(|entry| entry.text.contains("sandbox inherited")),
            "only an inherited sandbox gets the note"
        );
    }

    /// `!` shows a block, injects the `Ran \`cmd\`` message and audits; `!!` only shows and
    /// audits (ADR 0033).
    #[test]
    fn a_bang_done_injects_and_audits_and_a_bangbang_only_audits() {
        let mut app = App::new("m", "s");
        app.run_slash_command("!echo hi");
        let result = app.update(Input::BashDone {
            command: "echo hi".to_owned(),
            exclude_from_context: false,
            result: Ok(a_run(Some(0), "hi\n")),
        });
        assert!(
            matches!(result.effects[0], Effect::Inject(_)),
            "{:?}",
            result.effects
        );
        assert!(matches!(
            result.effects[1],
            Effect::AuditUserBash {
                exclude_from_context: false,
                exit_code: Some(0),
                ..
            }
        ));
        let entry = app.session.entries.last().unwrap();
        assert!(matches!(
            entry.kind,
            EntryKind::Bash {
                exclude_from_context: false
            }
        ));
        // The marker column carries the `$ `; the text is the command and output.
        assert!(entry.text.starts_with("echo hi"), "{:?}", entry.text);
        assert!(entry.text.contains("hi"), "{:?}", entry.text);

        let mut app = App::new("m", "s");
        app.run_slash_command("!!echo hi");
        let result = app.update(Input::BashDone {
            command: "echo hi".to_owned(),
            exclude_from_context: true,
            result: Ok(a_run(Some(0), "hi\n")),
        });
        assert!(
            !result
                .effects
                .iter()
                .any(|effect| matches!(effect, Effect::Inject(_))),
            "!! must not enter the context: {:?}",
            result.effects
        );
        assert!(matches!(
            app.session.entries.last().unwrap().kind,
            EntryKind::Bash {
                exclude_from_context: true
            }
        ));
    }

    /// The `!` message is shown live as a block; when it comes back through the transcript it
    /// is not drawn a second time (ADR 0033).
    #[test]
    fn the_injected_bang_message_is_not_drawn_twice() {
        let mut app = App::new("m", "s");
        app.run_slash_command("!echo hi");
        app.update(Input::BashDone {
            command: "echo hi".to_owned(),
            exclude_from_context: false,
            result: Ok(a_run(Some(0), "hi\n")),
        });
        let before = app.session.entries.len();
        let text = bash_context_text("echo hi", &a_run(Some(0), "hi\n"));
        app.push_message(&Message::user(text));
        assert_eq!(app.session.entries.len(), before, "no duplicate block");
    }

    /// A resumed `!` context message (no live block) renders as a Bash block, not user prose.
    #[test]
    fn a_resumed_bang_context_message_renders_as_a_block() {
        let mut app = App::new("m", "s");
        let text = bash_context_text("echo hi", &a_run(Some(0), "hi\n"));
        app.push_message(&Message::user(text));
        assert!(matches!(
            app.session.entries.last().unwrap().kind,
            EntryKind::Bash {
                exclude_from_context: false
            }
        ));
    }

    fn filler(count: usize) -> Vec<agent_core::Message> {
        (0..count)
            .map(|i| agent_core::Message::user(format!("message {i}")))
            .collect()
    }

    fn scans() -> u64 {
        LIVE_SCANS.with(std::cell::Cell::get)
    }

    /// resume-perf A: a resume pushes every message with no live entry, so the live-entry scan
    /// must not run. The number of entries walked is the property; no wall clock.
    #[test]
    fn push_message_does_not_walk_the_transcript_without_a_live_entry() {
        let mut app = App::new("m", "s");
        for message in filler(2_000) {
            app.push_message(&message);
        }
        assert_eq!(app.session.entries.len(), 2_000);
        let before = scans();
        for message in filler(1_000) {
            app.push_message(&message);
        }
        assert_eq!(
            scans(),
            before,
            "no live entry exists: push_message must not walk the transcript"
        );
    }

    /// The counter is live: with a live entry the walk does happen, and the entry is dropped.
    #[test]
    fn push_message_walks_the_transcript_to_drop_a_live_entry() {
        let mut app = App::new("m", "s");
        app.set_live(EntryKind::Assistant, "partial");
        let before = scans();
        app.push_message(&agent_core::Message::user("committed"));
        assert!(scans() > before, "a live entry makes the walk run");
        assert!(app.session.entries.iter().all(|entry| !entry.live));
        assert_eq!(app.session.live_count, 0);
    }

    /// A live entry is not always a suffix: a compaction notice can sit after it. The drop
    /// must still find it (a count, not a trailing-only scan).
    #[test]
    fn push_message_drops_a_live_entry_that_is_not_at_the_tail() {
        let mut app = App::new("m", "s");
        app.set_live(EntryKind::Assistant, "partial");
        app.session.entries.push(Entry::new(
            EntryKind::Notice,
            "context over the budget; compacting\u{2026}",
        ));
        app.push_message(&agent_core::Message::user("committed"));
        assert!(
            app.session.entries.iter().all(|entry| !entry.live),
            "the live entry is dropped wherever it sits"
        );
        assert!(
            app.session
                .entries
                .iter()
                .any(|entry| entry.kind == EntryKind::Notice),
            "the non-live entry after it survives"
        );
        assert_eq!(app.session.live_count, 0);
    }

    fn pair_scans() -> u64 {
        TOOL_PAIR_SCANS.with(std::cell::Cell::get)
    }

    /// A resume replays assistant tool calls and their results in order, so every result pairs
    /// with the most recent call. The pairing must not walk the transcript from the front: that
    /// was a second O(n^2) over the whole resume (resume-perf2). The number of entries walked
    /// is the property; no wall clock.
    #[test]
    fn tool_result_pairing_does_not_walk_the_transcript() {
        use agent_core::{ContentPart, ToolCall, ToolResult};

        let mut app = App::new("m", "s");
        let pairs = 400u64;
        let before = pair_scans();
        for i in 0..pairs {
            app.push_message(&agent_core::Message::Assistant {
                content: Vec::new(),
                reasoning: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: format!("call-{i}"),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "file.txt"}),
                }],
                provider_payload: None,
                interrupted: false,
                meta: Default::default(),
            });
            app.push_message(&agent_core::Message::Tool {
                result: ToolResult::success(format!("call-{i}"), vec![ContentPart::text("ok")]),
                meta: Default::default(),
            });
        }
        let scanned = pair_scans() - before;
        assert!(
            scanned <= pairs * 4,
            "pairing {pairs} results walked {scanned} entries; it must not scan the transcript"
        );
        // The results still pair: one Assistant entry, one call and one result per pair.
        assert_eq!(app.session.entries.len() as u64, pairs * 3);
    }

    /// The pairing search is a scan from the back, so it finds the most recent matching call.
    /// A batch of calls (one assistant message, several results) pairs each result with its own
    /// call, not the first one in the batch.
    #[test]
    fn a_batch_of_tool_results_pairs_each_with_its_own_call() {
        use agent_core::{ContentPart, ToolCall, ToolResult};

        let mut app = App::new("m", "s");
        app.push_message(&agent_core::Message::Assistant {
            content: Vec::new(),
            reasoning: Vec::new(),
            tool_calls: vec![
                ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "a"}),
                },
                ToolCall {
                    id: "b".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "b"}),
                },
            ],
            provider_payload: None,
            interrupted: false,
            meta: Default::default(),
        });
        // The results arrive in the reverse order (allowed): each must pair with its own call.
        for id in ["b", "a"] {
            app.push_message(&agent_core::Message::Tool {
                result: ToolResult::success(id.to_owned(), vec![ContentPart::text("ok")]),
                meta: Default::default(),
            });
        }
        let order: Vec<&str> = app
            .session
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::ToolCall)
            .filter_map(|entry| entry.tool.as_ref().map(|card| card.call_id.as_str()))
            .collect();
        assert_eq!(order, vec!["a", "b"], "the calls keep their order");
        // Each result sits right after **its own** call (rule 4): call a, result a, call b,
        // result b - not every call then every result.
        let layout: Vec<(EntryKind, String)> = app
            .session
            .entries
            .iter()
            .map(|entry| {
                let id = entry
                    .tool
                    .as_ref()
                    .map(|card| card.call_id.clone())
                    .unwrap_or_default();
                (entry.kind, id)
            })
            .collect();
        assert_eq!(
            layout,
            vec![
                (EntryKind::Assistant, String::new()),
                (EntryKind::ToolCall, "a".to_owned()),
                (EntryKind::ToolResult, "a".to_owned()),
                (EntryKind::ToolCall, "b".to_owned()),
                (EntryKind::ToolResult, "b".to_owned()),
            ]
        );
        // Both calls paired with their own result.
        let calls: Vec<Option<bool>> = app
            .session
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::ToolCall)
            .map(|entry| entry.result)
            .collect();
        assert_eq!(calls, vec![Some(false), Some(false)]);
    }
}
