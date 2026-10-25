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

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use radian_coding::PromptError;
use radian_core::{
    AgentEvent, BudgetReport, CompactionOutcome, ContentPart, Message, MessageId, UsageSource,
};
use radian_host::memory::commands::MemoryCommand;
use radian_host::{PendingRequest, PendingResponse, PendingView};
use radian_permissions::{ApprovalOutcome, ApprovalRequest};
use radian_session::RuntimeEvent;
use radian_session::TurnOutcome;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::bell::BellMode;
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
mod bash;
mod btw;
mod commands;
mod composer;
pub(crate) mod config;
mod find;
mod footer;
pub(crate) mod goal_mode;
mod history;
mod input;
mod inspector;
mod layers;
pub(crate) mod loop_mode;
pub(crate) use goal_mode::GoalMode;
mod mouse;
mod overlay;
mod send;
mod state;
mod transcript;
mod update;

pub use input::{Input, LogoutPick, MemoryResult, PickerData, SlashCompletion};
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

// The `/loop` message tags (the `origin` the journal audits, ADR 0005): the integration
// tests build the expected followups with them, the unit tests cover the round trip.
pub use loop_mode::{loop_iteration, loop_message};

/// How long the first idle Ctrl-C arms "press again to quit" (ADR 0023 U4). A second Ctrl-C inside the window quits; the window closing, or any other
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
    /// Cancel the in-flight `/btw` side question (Esc on its overlay).
    CancelSideQuestion,
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
    /// Read a dropped absolute path at the cursor (Q7). The host reads the bytes itself; the
    /// answer is [`Input::DroppedLoaded`]. `text` is the composer text the range belongs to,
    /// so a result for a line that has since changed is ignored.
    LoadDropped {
        text: String,
        path: PathBuf,
        range: std::ops::Range<usize>,
    },
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
    /// Run the `/login` verification request (ADR 0026 D3): one `GET` of the descriptor's
    /// verification URL (`<base>/models`, or the descriptor's own when it names one). The
    /// answer is [`Input::LoginVerified`].
    LoginVerify {
        name: String,
        protocol: &'static str,
        base_url: String,
        key: String,
    },
    /// Write the `/login` entry into `credentials/auth.json` (D3; only auth.json is written).
    /// `add` names a key to append to an entry that already has one; `None` replaces the first
    /// key. The answer is [`Input::LoginSaved`].
    LoginSave {
        name: String,
        key: String,
        env: Vec<(String, String)>,
        add: Option<String>,
    },
    /// Remove one stored credential (`/logout`, after the confirmation), or one key from it
    /// when `key` is `Some`. The answer is [`Input::LogoutDone`].
    Logout {
        provider: String,
        key: Option<String>,
    },
    /// Read the entry's keys so `/logout` can list them (`Input::LogoutChoices`). No network,
    /// no write.
    LogoutCheck { provider: String },
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
    /// `/context [all]` (backlog `tui-context-command`, grid spec
    /// `tui-context-grid`): the next request's estimated size by category. The
    /// loop reads it from the facade (journal + stored layers, no inbox, no
    /// driver) and answers with [`Input::ContextReady`], so it works mid-turn.
    Context { all: bool },
    /// Ask a side question (`/btw`, backlog `tui-btw-side-question`): the loop sends it
    /// through the session's current model and streams the answer back as
    /// [`Input::SideDelta`]/[`Input::SideDone`]. Nothing lands in the transcript.
    SideQuestion { question: String },
    /// Cancel the in-flight side question (Esc). The loop drops its cancel handle;
    /// the late [`Input::SideDone`] is ignored.
    /// `/jobs` (ADR 0037 decision 4): list the running background jobs, or stop one/all. The
    /// loop runs it (the registry lives in the host) and answers with [`Input::SessionDone`].
    Jobs(command::JobsRequest),
    /// Open the inspector overlay (`/jobs`, `/tools`, `/mcp`, `/hooks`, `/panel`). The loop
    /// gathers the snapshot (jobs, tools, mcp, hooks) and answers with
    /// [`Input::InspectorOpened`]. `None` opens the last page (`/panel`).
    OpenInspector {
        page: Option<command::InspectorPage>,
    },
    /// Reload the open inspector's snapshot after an event (a job settle, a turn
    /// boundary). The loop answers with [`Input::InspectorRefreshed`]. No polling: the
    /// refresh is requested by `update` when it sees the event, never by a timer.
    RefreshInspector,
    /// Write the transcript selection to the system clipboard (`tui.clipboard` picks the
    /// transport). The loop runs it and answers with [`Input::ClipboardDone`]: OSC 52 has no
    /// receipt, and `pbcopy` can fail — the loop owns both facts, `update` only owns the text.
    Copy { text: String },
    /// Ring the terminal bell: one `\x07` on stdout (`tui.bell`). The loop runs it at most
    /// once per frame — bell-class events that share a frame merge into one ring.
    Bell,
    /// `/theme <name>` (ADR 0008): load the theme and write `tui.theme` back to the
    /// config file. Both are I/O, so the loop runs them and answers with
    /// [`Input::ThemeReady`].
    Theme { name: String },
    /// Apply a startup trust choice (ADR 0031 T3): the loop writes `trust.json` through the
    /// facade and journals one `ProjectTrustChanged` per write. The answer is
    /// [`Input::TrustResolved`].
    SetTrust { choice: radian_host::trust::Choice },
    /// Show the current directory's trust decision and its source (`/trust`, `/trust
    /// status`, ADR 0031 T3): the loop reads it from the facade and answers with
    /// [`Input::SessionDone`]. No disk write.
    TrustStatus,
    /// Apply a `/trust on|off|forget` write (ADR 0031 T3): the loop writes `trust.json`
    /// through the facade and journals one `ProjectTrustChanged` per write, like
    /// [`Effect::SetTrust`]. Disk state only — loaded resources never hot-load or
    /// hot-unload — so the answer is [`Input::SessionDone`] with the takes-effect line.
    WriteTrust {
        write: radian_host::trust::TrustWrite,
    },
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
    /// Journal one `/goal` change first (ADR 0047): the loop appends `event` to the session
    /// journal and answers with [`Input::GoalDone`], which applies `apply`. A failed append
    /// applies nothing. Turn-end stops use the same effect for their audit record, but they
    /// mutate at once (the decision gates the send) and the completion only reports a lost
    /// write.
    GoalAppend {
        event: radian_session::RuntimeEvent,
        apply: goal_mode::GoalApply,
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
            (CancelSideQuestion, CancelSideQuestion) => true,
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
                GoalAppend {
                    event: ae,
                    apply: aa,
                },
                GoalAppend {
                    event: be,
                    apply: bb,
                },
            ) => ae == be && aa == bb,
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
                    add: aa,
                },
                LoginSave {
                    name: bn,
                    key: bk,
                    env: be,
                    add: ba,
                },
            ) => an == bn && ak == bk && ae == be && aa == ba,
            (LoginOAuthBrowser, LoginOAuthBrowser) | (LoginOAuthDevice, LoginOAuthDevice) => true,
            (
                Logout {
                    provider: a,
                    key: ak,
                },
                Logout {
                    provider: b,
                    key: bk,
                },
            ) => a == b && ak == bk,
            (LogoutCheck { provider: a }, LogoutCheck { provider: b }) => a == b,
            (Copy { text: a }, Copy { text: b }) => a == b,
            (Bell, Bell) => true,
            (SetTrust { choice: a }, SetTrust { choice: b }) => a == b,
            (TrustStatus, TrustStatus) => true,
            (WriteTrust { write: a }, WriteTrust { write: b }) => a == b,
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
            (SideQuestion { question: a }, SideQuestion { question: b }) => a == b,
            (OpenInspector { page: a }, OpenInspector { page: b }) => a == b,
            (RefreshInspector, RefreshInspector) => true,
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

/// One running background job the footer's `jobs` item shows (ADR 0027 slice ③
/// item 5): its id and when the TUI saw it start (the loop's clock, so the age ticks with the
/// existing footer refresh).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunningJob {
    pub(crate) id: String,
    pub(crate) started: Instant,
}

/// The **per-session view** (the per-session view): the state that belongs to exactly one
/// session — its transcript and the entries' caches, its projection handle, its scroll and
/// selection, its pending turn state, and its mode/tier markers. [`App`] owns one of these
/// today; a later session-view slice holds several and switches between them, so nothing here is
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
    /// Whether a model is routed (ADR 0008 §2): false when the session started with no model
    /// configured (no `--model`, no journaled route, no `model.model`). Submit is blocked until
    /// `/model` queues one; `/model`'s write-back fires once for such a session.
    pub has_model: bool,
    /// The per-turn model call limit this session runs under (the CLI flag or its default).
    /// Used for the notice a `BudgetExceeded` turn shows. Set once at startup.
    pub max_model_calls: u32,
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
    /// and they settle in **completion order**, which may differ from start order (radian-core's
    /// `ToolFinished` contract), so the list — not a single name — is what the note follows: it
    /// names the last call that is still running.
    running_tools: Vec<(String, String)>,
    /// This session's running background jobs, in start order (ADR 0027 slice ③
    /// item 5): the footer's `jobs` item reads them. A `BackgroundJobStarted` runtime event
    /// appends with the loop's clock, its settle removes; while any runs and the footer shows
    /// the `jobs` item, the idle loop waits the sooner of the hang-up tick and the next
    /// age-text change (ADR 0023 U2), so the age ticks with no other timer.
    pub(crate) running_jobs: Vec<RunningJob>,
    /// The `next_turn` inputs this process sent that the runtime has not admitted yet: one
    /// entry per `InboxEnqueued` without a matching `InputAdmitted`, by message id. The
    /// footer's `queued` item reads its length. A `steer` never touches it — its enqueue is
    /// `NextStep` and its admission carries an untracked id — so the count only moves on a
    /// followup's own enqueue and admission.
    pub(crate) queued: HashSet<MessageId>,
    /// Whether the last turn ended in a model or hook error (`TurnFailed` seen, no new turn
    /// started since): `/retry` resends the last user message only then. A normal end, a
    /// cancel or a fresh session leaves it false.
    pub(crate) last_turn_failed: bool,
    /// The `/loop` continuation (steps 1-2): the per-turn repeat state. Process-local;
    /// quitting the TUI ends it. The turn-end decision lives in `loop_mode` so step 3
    /// (`goal-mode.md`) can add its input in the same place.
    pub(crate) loop_state: loop_mode::LoopState,
    /// The `/goal` continuation (step 3, ADR 0047): the durable goal's live half. The
    /// durable state is shared with the `goal` tool and the prompt hook; the stretches,
    /// deadlines and send guards here are process-local like the loop's.
    pub(crate) goal_mode: GoalMode,
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
    /// A `/btw` side question is in flight (backlog `tui-btw-side-question`): the
    /// overlay streams it. At most one runs at a time; a second `/btw` is refused
    /// with a notice.
    pub(crate) btw_in_flight: bool,
    /// The last finished side answer, for `/btw` alone to reopen. In-memory only;
    /// the process exit drops it.
    pub(crate) btw_last: Option<crate::btw::BtwAnswer>,
    /// Whether this session has a shell (`!`) at all: false when no `shell` is mounted.
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
    /// The wake-budget notice entry of the current degrade burst, if shown. `update`
    /// rewrites it with the latest count instead of pushing a line per degrade; the next
    /// turn start clears it (the line stays as plain history). An entry-list mutation that
    /// shifts entries (the startup-block splice) moves it along.
    pub(crate) wake_notice: Option<usize>,
    /// Degraded wake notices wait while the session is idle: herdr reports `blocked` with
    /// the `wake budget` label until the next turn starts (which clears this).
    pub(crate) wake_waiting: bool,
    /// A lost job notice's stop waits while the session is idle: herdr reports `blocked`
    /// with the `job notice lost` label until the next turn starts (which clears this).
    /// Shares the burst's one notice line with [`wake_waiting`](Self::wake_waiting): the
    /// latest stop owns it.
    pub(crate) notice_lost_waiting: bool,
    /// The intent-stop cap waits while the session is idle: herdr reports `blocked`
    /// with the `intent stop` label until the next turn starts (which clears this).
    /// Shares the burst's one notice line with [`wake_waiting`](Self::wake_waiting): the
    /// latest stop owns it.
    pub(crate) intent_stop_waiting: bool,
}

/// The whole TUI's state: the global chrome every session view shares, plus exactly one
/// [`SessionView`] today (the per-session view). The chrome is what is set once from config and
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
    /// The input composer: text plus a cursor (ADR 0009 Phase 4).
    pub input: Composer,
    /// Images attached in the composer by a dropped absolute path (Q7). They join the next
    /// message after its text; Backspace on an empty input removes the newest.
    pub composer_images: Vec<radian_coding::LoadedImage>,
    /// The transcript's decoded image protocols (ADR 0028 R10), keyed by the image payload.
    /// Each entry hands over its one decode; the first frame that draws it builds the protocol
    /// from that, and a failed decode is remembered, so a broken image costs one attempt and
    /// then draws as its placeholder row only.
    pub images: crate::media::ImageCache,
    pub quit: bool,
    /// `/new` was accepted: the loop leaves, the host ends this session and opens a fresh
    /// one in the same terminal. Set with `quit` (the same exit), read by the loop's caller;
    /// a new session's app starts with it cleared.
    pub new_session: bool,
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
    /// The sends waiting for their `@` expansion, in submit order (see
    /// [`Effect::ExpandMentions`]): one entry per submitted send, so two rapid submits —
    /// `/queue a` then `/queue b`, or two fast Enters — cannot overwrite each other and drop
    /// one. The loop runs the effects in order and their results come back in order, so the
    /// front entry is always the one a result belongs to.
    pending_expand: VecDeque<PendingExpand>,
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
    /// When the terminal bell rings (config `tui.bell`, default off): a newly shown
    /// approval/question and a completed or failed turn end (`attention`), plus a
    /// background job's settle (`all`).
    pub bell: BellMode,
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
    /// The one session view this process holds today (the per-session view). A later slice
    /// (a later session-view slice) keeps several and switches between them.
    pub session: SessionView,
    /// The inspector's last page (`/panel` reopens it). Survives the overlay's close, so
    /// `/panel` after an `Esc` returns where the user was.
    pub inspector_last: Option<command::InspectorPage>,
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
}
