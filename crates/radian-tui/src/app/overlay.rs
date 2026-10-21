//! The layer stack's state: the three [`Tier`]s, one [`Layer`], the [`Overlay`] variants, and
//! the modal payloads - [`PendingApproval`], the [`Modal`] variants, the `/access full` and
//! startup-trust wizards, and the hardcoded [`ApprovalScroll`] keys. [`App`] owns the stack;
//! the code that pushes and pops it lives in `app.rs`.

use super::*;

/// The three tiers of the layer stack (ADR 0023 U4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Long-lived: the transcript and the message box. Always at the bottom of the stack.
    Surface,
    /// A temporary floating layer: the find box, the `@` completion list, the session picker.
    /// Never above a [`Modal`].
    Overlay,
    /// A prompt that needs an explicit answer before it goes away: the approval prompt, the
    /// `/memory` confirmation, and (extension point, not implemented) a structured question.
    /// Always above every [`Overlay`].
    Modal,
}

impl Tier {
    pub fn name(self) -> &'static str {
        match self {
            Self::Surface => "surface",
            Self::Overlay => "overlay",
            Self::Modal => "modal",
        }
    }
}

/// One layer of the stack. The variant *is* the tier, so an [`Overlay`] can never be a
/// [`Modal`]; [`App::push_overlay`] additionally keeps an overlay below every modal.
pub enum Layer {
    Surface,
    Overlay(Overlay),
    Modal(Modal),
}

impl Layer {
    pub fn tier(&self) -> Tier {
        match self {
            Self::Surface => Tier::Surface,
            Self::Overlay(_) => Tier::Overlay,
            Self::Modal(_) => Tier::Modal,
        }
    }
}

/// The state of one [`Tier::Overlay`] layer.
pub enum Overlay {
    /// The in-transcript search box (Phase 5).
    Find(FindState),
    /// The completion candidate list (label + description), drawn by `select_list`.
    Completion(Vec<crate::command::Candidate>),
    /// The session picker (ADR 0023 U4): `--pick` at startup, `/sessions` in a session.
    Picker(Picker),
    /// The inspector (`jobs | tools | mcp | hooks`): one tabbed overlay over the
    /// transcript. Temporary, like the picker; never above a modal.
    Inspector(crate::inspector::Inspector),
    /// The `/btw` side-question overlay (backlog `tui-btw-side-question`): the
    /// streaming side answer. Temporary, like the picker; never above a modal.
    Btw(crate::btw::BtwOverlay),
}

/// One approval as the UI shows it: the queue's id (ADR 0023 U7), the session that asked and
/// the request itself. The queue id is what the answer names, not the permission layer's own
/// `request_id`.
///
/// The scroll fields belong to this modal, not to the app: a new pending request builds a fresh
/// `PendingApproval`, so the next question starts at the top of its body.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub request_id: u64,
    pub origin: String,
    pub request: ApprovalRequest,
    /// The body's first visible row. The renderer clamps it against what it measured, so a key
    /// can never park the window past the end (or before the first frame move it at all).
    pub scroll: usize,
    /// The wrapped body rows and the visible rows, as the last frame measured them. Both zero
    /// before the first frame, which clamps every scroll to zero.
    pub total_rows: usize,
    pub visible_rows: usize,
}

/// The state of one [`Tier::Modal`] layer.
pub enum Modal {
    /// A tool call waiting for `y`/`a`/`n`. The keys are hardcoded (ADR 0023 U4, invariant 2).
    /// Boxed, like the question: the two payloads are much larger than the confirmation's.
    Approval(Box<PendingApproval>),
    /// A `/memory` write waiting for `y`/`n`. Also hardcoded.
    ConfirmCommand(PendingCommand),
    /// A structured question (ADR 0024 Q7). Its keys are the rebindable `form` layer, so it is
    /// the one modal whose input goes through the keymap. Boxed: the form is much larger than
    /// the other variants, and a `Modal` sits in every `Layer`.
    Question(Box<QuestionForm>),
    /// The `/login` wizard (ADR 0026 D3). Its keys are hardcoded (a login prompt is
    /// sensitive, like the approval), and the key field is never echoed. Boxed, like the
    /// question.
    Login(Box<LoginWizard>),
    /// A `/logout` confirmation: the provider whose stored credential would be removed.
    /// `y` removes, `n`/`Esc` keeps — hardcoded, like the `/memory` confirmation.
    ConfirmLogout(String),
    /// The startup project-trust prompt (ADR 0031 T3). A small list whose keys are hardcoded
    /// like the approval's: a startup decision must not depend on the keymap. Boxed, like the
    /// other payload modals.
    Trust(Box<TrustWizard>),
    /// The `/access full` confirmation (ADR 0038 slice 2): switching to `full` means no
    /// sandbox, so a human must confirm. A small list whose keys are hardcoded; the default
    /// focus is `cancel`, so Enter does not open the sandbox. Boxed like the trust prompt.
    AccessFull(Box<AccessWizard>),
}

/// What the `/access full` confirmation offers (ADR 0038 decision 2/3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessChoice {
    /// Switch to `full` for this session only (still journaled).
    Session,
    /// Switch to `full` and remember it for this project (`~/.radian/access/`).
    Remember,
    /// Leave the tier unchanged.
    Cancel,
}

impl AccessChoice {
    /// The three options in display order; `cancel` last so the default focus is fail-safe.
    pub const ALL: [AccessChoice; 3] = [
        AccessChoice::Session,
        AccessChoice::Remember,
        AccessChoice::Cancel,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Session => "this session only",
            Self::Remember => "remember for this project",
            Self::Cancel => "cancel",
        }
    }
}

/// The `/access full` confirmation's state: the highlighted option. The default is `cancel`
/// (owner rule: opening the sandbox's absence must never be the pre-selected answer, so Enter
/// cannot do it).
pub struct AccessWizard {
    pub cursor: usize,
}

impl AccessWizard {
    pub fn new() -> Self {
        let cursor = AccessChoice::ALL
            .iter()
            .position(|choice| *choice == AccessChoice::Cancel)
            .unwrap_or(AccessChoice::ALL.len() - 1);
        Self { cursor }
    }

    pub fn selected(&self) -> AccessChoice {
        AccessChoice::ALL[self.cursor]
    }

    pub fn move_cursor(&mut self, delta: i32) {
        let len = AccessChoice::ALL.len() as i32;
        self.cursor = (self.cursor as i32 + delta).rem_euclid(len) as usize;
    }
}

impl Default for AccessWizard {
    fn default() -> Self {
        Self::new()
    }
}

/// The startup trust modal's state (ADR 0031 T3): the workspace and the highlighted option.
/// The default highlight is "trust this directory, and use the sandbox for this project"
/// (decision 2026-10-20: for a project with no trusted decision the sandbox choice
/// comes first and is pre-selected, so Enter trusts with the sandbox). This keeps the
/// owner rule below the access wizard: opening the sandbox's absence is never the
/// pre-selected answer.
///
/// Slice B2: `mcp` carries the project `.mcp.json` preview the modal lists (one line per
/// server, env key names only, never values), or `None` when there is no project file.
/// Slice B (hooks): `hooks` carries the project `.radian/hooks.json` preview the modal
/// lists (one line per hook: the event, the matcher, the command), or `None` when there is
/// no project file.
pub struct TrustWizard {
    pub workspace: PathBuf,
    pub cursor: usize,
    pub choices: Vec<radian_host::trust::Choice>,
    pub mcp: Option<radian_host::ProjectPreview>,
    pub hooks: Option<radian_host::ProjectHooksPreview>,
}

impl TrustWizard {
    pub fn new(
        workspace: PathBuf,
        mcp: Option<radian_host::ProjectPreview>,
        hooks: Option<radian_host::ProjectHooksPreview>,
    ) -> Self {
        let choices: Vec<radian_host::trust::Choice> = radian_host::trust::Choice::ALL
            .into_iter()
            // "Trust parent" needs a parent to name.
            .filter(|choice| {
                *choice != radian_host::trust::Choice::TrustParent || workspace.parent().is_some()
            })
            .collect();
        let cursor = choices
            .iter()
            .position(|choice| *choice == radian_host::trust::Choice::TrustHereSandbox)
            .unwrap_or(0);
        Self {
            workspace,
            cursor,
            choices,
            mcp,
            hooks,
        }
    }

    pub fn selected(&self) -> radian_host::trust::Choice {
        self.choices[self.cursor]
    }

    pub fn move_cursor(&mut self, delta: i32) {
        let len = self.choices.len() as i32;
        self.cursor = (self.cursor as i32 + delta).rem_euclid(len) as usize;
    }

    pub fn label(&self, choice: radian_host::trust::Choice) -> String {
        choice
            .label(&self.workspace)
            .unwrap_or_else(|| "Trust the parent directory".to_owned())
    }
}

/// How far one approval-scroll key moves the box's body (approval-fit). Hardcoded like the
/// answers (ADR 0023 U4, invariant 2): scrolling must never depend on the keymap and must
/// never collide with an answer key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalScroll {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
}
