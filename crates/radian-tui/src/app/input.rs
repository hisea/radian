//! What comes into the app: [`Input`], one interaction intent or host fact, plus the types its
//! results carry - the picker's [`PickerData`], the `/memory` executor's [`MemoryResult`], and
//! the `/…`/`@` completion state ([`SlashCompletion`], [`PendingSend`], `PendingExpand`).
//! [`Effect`] stays in `app.rs` with the entry point that returns it.

use super::*;

/// One input to [`App::update`]: an interaction intent, or a fact from the host.
///
/// [`Input::Action`] is the keymap's semantic action. [`Input::Char`] and
/// [`Input::FindBackspace`] are the two editing intents that are deliberately **not** bindings
/// (typing, and the find box's own backspace, which belongs to the box like the approval keys
/// belong to the prompt). Everything else is a host fact or the result of an [`Effect`].
pub enum Input {
    /// A semantic action, from the keymap or synthesized by the host.
    Action(Action),
    /// The `cancel` action (Ctrl-C by default) with the wall clock it was read at. The only
    /// action whose meaning depends on time: idle with an empty input, the first press arms
    /// "press again to quit" and a second inside [`QUIT_WINDOW`] quits. `update` stays pure;
    /// the loop (or a test) stamps the time.
    Cancel { now: Instant },
    /// The armed quit window closed: disarm and redraw, so the hint goes away. Sent by the
    /// loop on its `recv_timeout` (ADR 0023 U2: a time-driven display takes the timeout).
    QuitDeadline,
    /// The deadline of a `/fork` waiting for the turn boundary to reach the journal.
    ForkDeadline,
    /// A printable character typed into the focused text field.
    Char(char),
    /// The find box's backspace (not keymap-driven).
    FindBackspace,
    /// A bracketed paste: inserted at the cursor.
    Paste(String),
    /// The terminal was resized. The renderer re-measures on the next frame.
    Resize { rows: u16, cols: u16 },
    /// The hardcoded answer to the approval prompt on screen (`y`/`a`/`n`/`Esc`).
    ApprovalAnswer(ApprovalOutcome),
    /// The approval box's auto key `!` (ADR 0026 D9, a constant in `render.rs`): approve the
    /// pending request once **and** turn auto on for the rest of the session. Hardcoded like
    /// the answers (ADR 0023 U4, invariant 2).
    ApprovalAuto,
    /// A scroll of the approval box's body (`↑`/`↓`/PgUp/PgDn). Hardcoded like the answers:
    /// scrolling must never depend on the keymap, and it must never collide with an answer key.
    ApprovalScroll(ApprovalScroll),
    /// The hardcoded answer to a `/memory` confirmation (`true` = confirmed).
    ConfirmCommand(bool),
    /// The streaming text the projection has accumulated (ADR 0023 U2). Deltas never come
    /// through the channel: the loop reads the projection when a `Dirty` notification says so.
    Stream(StreamSnapshot),
    /// A live agent event.
    Event(AgentEvent),
    /// A runtime event (ADR 0017's turn failure).
    Runtime(RuntimeEvent),
    /// The pending queue's head changed (ADR 0023 U7): the request to show, and how many are
    /// queued. `None` means nothing is pending.
    Pending(Option<Box<PendingView<PendingRequest>>>, usize),
    /// One key inside the question form (ADR 0024 Q7).
    Form(FormKey),
    /// An `@` expansion finished (the result of [`Effect::ExpandMentions`]).
    MentionsExpanded(Result<Expansion, ExpandError>),
    /// An `@` completion finished (`None` = nothing to complete).
    CompletionReady(Option<Completion>),
    /// The host answered [`Effect::FetchCompletions`] with the candidates for one level of the
    /// command tree.
    CompletionsReady {
        fetch: command::Fetch,
        candidates: Vec<command::Candidate>,
    },
    /// A `/memory` command finished (the result of [`Effect::RunMemoryCommand`]).
    MemoryCommandDone(MemoryResult),
    /// A manual `/compact` finished (the result of [`Effect::Compact`]).
    CompactDone(Result<CompactionOutcome, String>),
    /// A `!`/`!!` command finished (the result of [`Effect::RunBash`]).
    BashDone {
        command: String,
        exclude_from_context: bool,
        result: Result<radian_coding::UserBash, String>,
    },
    /// A `/thinking` level was clamped against the current model's map (D11): `applied` is the
    /// level that will be used (`None` when the model supports no level at all) and
    /// `supported` the levels the completion offered.
    ThinkingChecked {
        requested: String,
        applied: Option<String>,
        supported: Vec<String>,
    },
    /// A `/model` target was vetted by the loop (ADR 0026 D5/D7). `tokens` is the current
    /// context estimate `T` (`None` = unknown, so it cannot exceed anything), `budget` the
    /// new model's `B'` (`None` = unknown, so no comparison is possible).
    ModelChecked {
        provider: String,
        model: String,
        tokens: Option<u64>,
        budget: Option<u64>,
    },
    /// The compaction before a switch finished (D7). Success queues the switch.
    SwitchCompacted {
        provider: String,
        model: String,
        result: Result<CompactionOutcome, String>,
    },
    /// A `/sessions`, `/export` or `/fork` finished: the notices to show.
    SessionDone(Vec<String>),
    /// A wake-gate degrade fired (one per degrade): the wake-budget notice line. `update`
    /// shows a single line per burst (rewriting it with the latest count) and marks the
    /// session as wake-waiting for herdr until the next turn starts.
    WakeBudgetNotice(String),
    /// A `/skill:<name> [args]` expansion finished (ADR 0025 S3): the message text to send, or
    /// a notice (an unknown skill or a file that can no longer be read). `steer` mirrors the
    /// turn state when the command was submitted.
    SkillExpanded {
        result: Result<String, String>,
        steer: bool,
    },
    /// The session's facade refused a command (ADR 0023 U5): an undeclared capability. The UI
    /// shows a notice instead of failing the loop.
    CommandRefused(radian_host::CommandRefused),
    /// One key for the session picker overlay (ADR 0023 U4). The picker owns its own parser,
    /// so the key is forwarded rather than resolved here.
    PickerKey(KeyEvent),
    /// Close the session picker without choosing (`Esc` on an unfocused overlay).
    PickerClose,
    /// The session picker's data arrived (the result of [`Effect::OpenPicker`]).
    PickerOpened(PickerData),
    /// One key for the inspector overlay. The inspector owns its own parser, so the key
    /// is forwarded rather than resolved here.
    InspectorKey(KeyEvent),
    /// The inspector's snapshot arrived (the result of [`Effect::OpenInspector`]): the
    /// requested page (`None` = the last page, `/panel`) and the data to show.
    InspectorOpened {
        requested: Option<command::InspectorPage>,
        snapshot: crate::inspector::InspectorSnapshot,
    },
    /// The open inspector's snapshot was reloaded (the result of
    /// [`Effect::RefreshInspector`]).
    InspectorRefreshed(crate::inspector::InspectorSnapshot),
    /// One key inside the `/login` wizard (ADR 0026 D3). Hardcoded keys, like the approval.
    LoginKey(LoginKey),
    /// The `/login` verification request finished (D3). `name` is the provider the wizard is
    /// logging in (the answer is ignored unless the wizard is still waiting on it).
    LoginVerified {
        name: String,
        result: Result<(), String>,
    },
    /// The `auth.json` write of a `/login` finished: the wizard closes either way. The
    /// success notice names the provider and the way to switch to it (D3). Shared by the
    /// API-key `Save` path and both ChatGPT OAuth flows.
    LoginSaved {
        name: String,
        result: Result<(), String>,
    },
    /// [`Effect::LoginOAuthBrowser`] bound its listener and has the authorize URL. Applied
    /// only while the wizard is still in `OAuthBrowserWaiting` (the same "late answer" rule
    /// as `LoginVerified`).
    LoginOAuthUrlReady { url: String },
    /// [`Effect::LoginOAuthDevice`] has the user code to show. Applied only while the wizard
    /// is still in `OAuthDeviceWaiting`.
    LoginOAuthDeviceReady {
        user_code: String,
        verification_url: String,
    },
    /// The hardcoded answer to a `/logout` confirmation (`true` = remove).
    ConfirmLogout(bool),
    /// A `/logout` removal finished: the provider and the outcome. The notice names the
    /// provider (the removal is otherwise silent, D3's confirmation being the record).
    LogoutDone {
        provider: String,
        result: Result<(), String>,
    },
    /// The startup trust modal's cursor moved (ADR 0031 T3).
    TrustMove(i32),
    /// The startup trust modal chose an option; the loop writes `trust.json` and journals.
    TrustChosen(radian_host::trust::Choice),
    /// The trust choice was applied: the session's new state, for the startup notice.
    TrustResolved(radian_host::trust::Setup),
    /// The `/access full` confirmation's cursor moved (ADR 0038 slice 2).
    AccessMove(i32),
    /// The `/access full` confirmation chose an option.
    AccessChosen(AccessChoice),
    /// A tier switch finished (the result of [`Effect::SetAccess`]): the tier, whether it was
    /// remembered, and the outcome (a failed sandbox probe is a refusal that changes nothing).
    AccessApplied {
        tier: crate::profile::Tier,
        remember: bool,
        result: Result<(), String>,
    },
    /// The left button went down at a screen cell: the press that may start a transcript
    /// selection (and that clears the previous one). Only arrives when mouse reporting is on.
    MouseDown { row: u16, col: u16 },
    /// The left button moved while held: extends the selection.
    MouseDrag { row: u16, col: u16 },
    /// The left button came up: ends the selection and copies it ([`Effect::Copy`]).
    MouseUp { row: u16, col: u16 },
    /// The clipboard write finished (the result of [`Effect::Copy`]): the honest status-bar
    /// notice — what Radian did, never a claim about the clipboard it cannot verify.
    ClipboardDone(String),
    /// `/theme` finished (the result of [`Effect::Theme`]): the loaded theme (`None` when the
    /// switch did not happen, e.g. an unknown name or `NO_COLOR`) and the notices the loop
    /// produced (warnings, and whether the write-back happened).
    ThemeReady {
        theme: Option<Box<crate::theme::Theme>>,
        notices: Vec<String>,
    },
}

/// What the loop loaded for a picker overlay: the rows to show, the index for full-text
/// search, and the workspace they belong to.
///
/// It holds a `radian_session::Index`, whose `Connection` is not `Debug`, so this type does
/// not derive `Debug`.
pub struct PickerData {
    pub rows: Vec<radian_host::sessions::SessionRow>,
    pub index: Option<radian_session::Index>,
    pub workspace: String,
    /// `/sessions <query>`: open in full-text search mode with this query.
    pub query: Option<String>,
    /// Values to redact from a search snippet (the host's secret list).
    pub secrets: Vec<String>,
}

/// The result of [`Effect::RunMemoryCommand`]. The executor decides nothing: a command that
/// writes comes back as [`MemoryResult::NeedsConfirmation`] and `update` arms the prompt.
#[derive(Debug)]
pub enum MemoryResult {
    /// The command ran; these lines are the notices to show.
    Output(Vec<String>),
    /// The command writes and needs the user's `y` first.
    NeedsConfirmation {
        command: MemoryCommand,
        prompt: String,
    },
    /// The command failed; this is the message to show.
    Failed(String),
}

/// The `/…` completion on screen: the char range a chosen candidate replaces, and the
/// candidates (best first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCompletion {
    pub range: std::ops::Range<usize>,
    pub candidates: Vec<command::Candidate>,
}

/// Which inbox call an in-flight `@` expansion is for, so its result can become the right
/// effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingSend {
    Followup,
    Steer,
}

/// The send that is waiting for its `@` expansion: `update` produced
/// [`Effect::ExpandMentions`] and remembers what to do when [`Input::MentionsExpanded`]
/// arrives.
#[derive(Debug)]
pub(super) struct PendingExpand {
    pub(super) kind: PendingSend,
    pub(super) text: String,
}
