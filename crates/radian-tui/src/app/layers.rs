//! The layer stack (ADR 0023 U4) and the state it holds: the stack itself and its
//! `push_*`/removal rules, the read-only accessors the frame and the loop use, the `/…`/`@`
//! completion menu that lives on an overlay, the session picker's accessors, and the
//! pre-session `--pick` phase that pushes the picker overlay.

use super::*;

impl App {
    /// Enters the pre-session `--pick` phase: the picker overlay is pushed **focused** (it is
    /// the whole point of the phase) and [`Startup`] becomes [`Startup::Picking`].
    pub fn start_picking(&mut self, mut picker: Picker) {
        picker.focus();
        self.startup = Startup::Picking;
        self.push_overlay(Overlay::Picker(picker));
    }

    /// Leaves the pre-session phase: the picker overlay is removed and the app is a normal
    /// session app again (the caller then opens the session).
    pub fn finish_picking(&mut self) {
        self.picker_close();
        self.startup = Startup::Session;
    }

    /// Takes the pre-session picker's choice, if it has one.
    pub fn take_picker_outcome(&mut self) -> Option<PickerOutcome> {
        self.picker_outcome.take()
    }

    /// True once the pre-session picker has resolved, so the loop can leave. Always false in
    /// [`Startup::Session`].
    pub fn picking_resolved(&self) -> bool {
        self.picker_outcome.is_some()
    }

    // ---------- the layer stack (ADR 0023 U4) ----------

    /// The layer stack, bottom ([`Layer::Surface`]) first. Read-only; use the `push_*` and
    /// removal methods to change it.
    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// Pushes an overlay. ADR 0023 U4: an overlay is never above **any** modal, so when modals
    /// are up the overlay is inserted just below the lowest one (below the highest one would
    /// put it above every modal under that). With no modal it goes on top.
    pub fn push_overlay(&mut self, overlay: Overlay) {
        match self
            .layers
            .iter()
            .position(|layer| layer.tier() == Tier::Modal)
        {
            Some(index) => self.layers.insert(index, Layer::Overlay(overlay)),
            None => self.layers.push(Layer::Overlay(overlay)),
        }
    }

    /// Pushes a modal on top of the whole stack.
    pub fn push_modal(&mut self, modal: Modal) {
        self.layers.push(Layer::Modal(modal));
    }

    /// Pushes the startup trust modal (ADR 0031 T3). Called once, after the terminal is
    /// entered, when the workspace has project resources and no saved decision.
    /// `mcp` is the project `.mcp.json` preview the modal lists (slice B2); `hooks` is the
    /// project `.radian/hooks.json` preview (slice B).
    pub fn push_trust_modal(
        &mut self,
        workspace: PathBuf,
        mcp: Option<radian_host::ProjectPreview>,
        hooks: Option<radian_host::ProjectHooksPreview>,
    ) {
        self.push_modal(Modal::Trust(Box::new(TrustWizard::new(
            workspace, mcp, hooks,
        ))));
    }

    /// Removes the topmost overlay matching `pick`. Returns it, or `None` when absent.
    pub(super) fn remove_overlay(
        &mut self,
        mut pick: impl FnMut(&Overlay) -> bool,
    ) -> Option<Overlay> {
        let index = self.layers.iter().rposition(|layer| match layer {
            Layer::Overlay(overlay) => pick(overlay),
            _ => false,
        })?;
        match self.layers.remove(index) {
            Layer::Overlay(overlay) => Some(overlay),
            _ => None,
        }
    }

    /// Removes the topmost modal matching `pick`. Returns it, or `None` when absent.
    pub(super) fn remove_modal(&mut self, mut pick: impl FnMut(&Modal) -> bool) -> Option<Modal> {
        let index = self.layers.iter().rposition(|layer| match layer {
            Layer::Modal(modal) => pick(modal),
            _ => false,
        })?;
        match self.layers.remove(index) {
            Layer::Modal(modal) => Some(modal),
            _ => None,
        }
    }

    /// The topmost approval modal, mutably, so the renderer can feed the measured body size
    /// back into it (the way the transcript's viewport is fed back) and the scroll keys can
    /// move its window.
    pub fn approval_modal_mut(&mut self) -> Option<&mut PendingApproval> {
        self.layers.iter_mut().rev().find_map(|layer| match layer {
            Layer::Modal(Modal::Approval(prompt)) => Some(&mut **prompt),
            _ => None,
        })
    }

    /// The topmost `/login` wizard, mutably, so `update` can step it.
    pub fn login_modal_mut(&mut self) -> Option<&mut LoginWizard> {
        self.layers.iter_mut().rev().find_map(|layer| match layer {
            Layer::Modal(Modal::Login(wizard)) => Some(&mut **wizard),
            _ => None,
        })
    }

    /// The `/logout` confirmation on screen, if any.
    pub fn pending_logout(&self) -> Option<&str> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Modal(Modal::ConfirmLogout(provider)) => Some(provider.as_str()),
            _ => None,
        })
    }

    /// The topmost modal, if any. The input box is not editable while one is up.
    pub fn top_modal(&self) -> Option<&Modal> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Modal(modal) => Some(modal),
            _ => None,
        })
    }

    pub fn has_modal(&self) -> bool {
        self.top_modal().is_some()
    }

    /// The approval prompt on screen, if any.
    pub fn pending_approval(&self) -> Option<&ApprovalRequest> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Modal(Modal::Approval(prompt)) => Some(&prompt.request),
            _ => None,
        })
    }

    /// The `/memory` confirmation on screen, if any.
    pub fn pending_command(&self) -> Option<&PendingCommand> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Modal(Modal::ConfirmCommand(command)) => Some(command),
            _ => None,
        })
    }

    /// The `@` completion candidates, if the list is up.
    pub fn completion(&self) -> Option<&[crate::command::Candidate]> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Completion(candidates)) => Some(candidates.as_slice()),
            _ => None,
        })
    }

    pub(super) fn completion_clear(&mut self) {
        self.remove_overlay(|overlay| matches!(overlay, Overlay::Completion(_)));
        self.mention_completion = None;
    }

    pub fn completion_set(&mut self, candidates: Vec<crate::command::Candidate>) {
        self.completion_clear();
        if !candidates.is_empty() {
            // A fresh menu highlights its first (best-ranked) row.
            self.completion_cursor = 0;
            self.push_overlay(Overlay::Completion(candidates));
        }
    }

    /// The word the slash menu is completing: the text from the level's range start to the
    /// cursor. The menu filters on it, so typing a provider name finds that provider's models
    /// (ADR 0035).
    pub fn completion_filter(&self) -> String {
        let Some(slash) = &self.slash_completion else {
            return String::new();
        };
        let chars: Vec<char> = self.input.text().chars().collect();
        let end = self.input.cursor().min(chars.len());
        let start = slash.range.start.min(end);
        chars[start..end].iter().collect()
    }

    /// Whether a completion menu is on screen (the slash list or the `@` list).
    pub fn menu_open(&self) -> bool {
        self.slash_completion.is_some() || self.completion().is_some()
    }

    /// Closes the menu and keeps the input as it is.
    pub(super) fn menu_close(&mut self) {
        self.completion_clear();
        self.slash_completion = None;
    }

    /// The number of rows in the open menu.
    fn menu_len(&self) -> usize {
        if let Some(slash) = &self.slash_completion {
            return slash.candidates.len();
        }
        self.completion()
            .map(<[crate::command::Candidate]>::len)
            .unwrap_or(0)
    }

    /// Moves the menu highlight, wrapping.
    pub(super) fn move_menu(&mut self, delta: i32) {
        let len = self.menu_len();
        if len == 0 {
            return;
        }
        self.completion_cursor =
            (self.completion_cursor as i32 + delta).rem_euclid(len as i32) as usize;
    }

    /// Accepts the highlighted row: Tab and Enter do the same, and Enter never sends.
    pub(super) fn accept_menu_candidate(&mut self) -> UpdateResult {
        if self.slash_completion.is_some() {
            return self.accept_slash_candidate();
        }
        self.accept_mention_candidate()
    }

    /// Accepts the highlighted `@` candidate (a path), completing the reference.
    fn accept_mention_candidate(&mut self) -> UpdateResult {
        let Some((range, candidates)) = self.mention_completion.take() else {
            return UpdateResult::none();
        };
        let Some(candidate) = candidates.get(self.completion_cursor) else {
            return UpdateResult::none();
        };
        // A directory keeps its trailing `/`; a file gets a trailing space (mention.rs's rule).
        let insert = if candidate.ends_with('/') {
            candidate.clone()
        } else {
            format!("{candidate} ")
        };
        self.replace_range(range, &insert);
        self.menu_close();
        UpdateResult::dirty()
    }

    /// The session picker overlay, if it is up.
    pub fn picker(&self) -> Option<&Picker> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Picker(picker)) => Some(picker),
            _ => None,
        })
    }

    pub(super) fn picker_mut(&mut self) -> Option<&mut Picker> {
        self.layers.iter_mut().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Picker(picker)) => Some(picker),
            _ => None,
        })
    }

    pub(super) fn picker_close(&mut self) {
        self.remove_overlay(|overlay| matches!(overlay, Overlay::Picker(_)));
    }

    /// The inspector overlay, if it is up.
    pub fn inspector(&self) -> Option<&crate::inspector::Inspector> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Inspector(inspector)) => Some(inspector),
            _ => None,
        })
    }

    pub(super) fn inspector_mut(&mut self) -> Option<&mut crate::inspector::Inspector> {
        self.layers.iter_mut().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Inspector(inspector)) => Some(inspector),
            _ => None,
        })
    }

    pub(super) fn inspector_close(&mut self) {
        self.remove_overlay(|overlay| matches!(overlay, Overlay::Inspector(_)));
    }

    /// The `/btw` side-question overlay, if it is up.
    pub fn btw(&self) -> Option<&crate::btw::BtwOverlay> {
        self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Btw(overlay)) => Some(overlay),
            _ => None,
        })
    }

    pub(super) fn btw_mut(&mut self) -> Option<&mut crate::btw::BtwOverlay> {
        self.layers.iter_mut().rev().find_map(|layer| match layer {
            Layer::Overlay(Overlay::Btw(overlay)) => Some(overlay),
            _ => None,
        })
    }

    pub(super) fn btw_close(&mut self) {
        self.remove_overlay(|overlay| matches!(overlay, Overlay::Btw(_)));
    }
}
