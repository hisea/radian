//! The dispatch: [`App::update_inner`] and the one place a key becomes an [`Input`]
//! (`key_input`, with the hardcoded layers first), the `@` picker's keys, and the semantic
//! actions (`update_action`) with the Ctrl-C quit window (`cancel_or_quit_at`). [`App::update`]
//! itself stays in `app.rs`, where the purity test names it.

use super::*;

impl App {
    pub(super) fn update_inner(&mut self, input: Input) -> UpdateResult {
        // A pending quit is disarmed by user input only: a resize,
        // a stream/runtime fact or an asynchronous result is not the user changing their mind,
        // and letting one disarm the window made the second Ctrl-C look like it did nothing.
        // The confirming Ctrl-C itself and the deadline input are excluded here because this
        // match handles them.
        let disarmed = self.quit_pending.is_some()
            && is_user_input(&input)
            && !matches!(input, Input::Cancel { .. });
        if disarmed {
            self.quit_pending = None;
        }
        let mut result = match input {
            // The two clock-aware inputs the loop (or a test) sends.
            Input::Cancel { now } => self.cancel_or_quit_at(now),
            Input::QuitDeadline => {
                // The armed window closed: clear it here (the disarm rule below excludes this
                // input, because this *is* the disarming event).
                self.quit_pending = None;
                UpdateResult::dirty()
            }
            Input::ForkDeadline => {
                // The wait for the turn boundary ran out: say so and drop the fork, so no
                // empty child session is created.
                if self.session.pending_fork.take().is_some() {
                    self.push_notice("still writing the turn; try again");
                }
                UpdateResult::dirty()
            }
            Input::Action(action) => {
                // A completion menu is a selectable list: while it is up,
                // Up/Down (and Ctrl-N) move the highlight and wrap, Enter and Tab accept the
                // highlighted row (Enter never sends), and Esc closes the menu keeping the
                // input. Every other action dismisses the menu and proceeds as usual.
                if self.menu_open() {
                    match action {
                        Action::HistoryPrev => {
                            self.move_menu(-1);
                            return UpdateResult::dirty();
                        }
                        Action::HistoryNext | Action::MenuNext => {
                            self.move_menu(1);
                            return UpdateResult::dirty();
                        }
                        Action::Escape => {
                            self.menu_close();
                            return UpdateResult::dirty();
                        }
                        Action::Complete | Action::Send => {
                            return self.accept_menu_candidate();
                        }
                        _ => self.menu_close(),
                    }
                }
                self.update_action(action)
            }
            Input::Char(c) => {
                self.completion_clear();
                if self.find_state().is_some() {
                    self.find_push(c);
                } else {
                    self.input.insert_char(c);
                }
                UpdateResult::dirty()
            }
            Input::FindBackspace => {
                self.completion_clear();
                self.find_backspace();
                UpdateResult::dirty()
            }
            // A bracketed paste is text for the message box — unless the `/login` wizard is
            // up: its fields take the paste (masked like typing, D3), and the message box is
            // frozen while a modal is up anyway.
            Input::Paste(text) => {
                if matches!(self.top_modal(), Some(Modal::Login(_))) {
                    let outcome = self
                        .login_modal_mut()
                        .map(|wizard| wizard.on_key(LoginKey::Paste(text)));
                    return self.login_outcome(outcome.unwrap_or(LoginOutcome::Stay));
                }
                self.input.insert_str(&text);
                UpdateResult::dirty()
            }
            Input::Resize { .. } => UpdateResult::dirty(),
            Input::ApprovalAnswer(outcome) => self.answer_approval(outcome),
            Input::ApprovalAuto => self.approval_auto_key(),
            Input::ApprovalScroll(direction) => self.approval_scroll(direction),
            Input::ConfirmCommand(confirmed) => self.confirm_command(confirmed),
            Input::Stream(stream) => {
                // The snapshot is the whole live view of the current stream, so the entries are
                // *set*, not appended: a coalesced notification can never leave a short entry.
                if stream.order.is_empty() {
                    // A cleared stream (a committed message replaced it): nothing to show.
                    return UpdateResult::none();
                }
                for kind in &stream.order {
                    match kind {
                        crate::projection::StreamKind::Assistant => {
                            self.set_live(EntryKind::Assistant, &stream.assistant);
                        }
                        crate::projection::StreamKind::Reasoning => {
                            self.set_live(EntryKind::Reasoning, &stream.reasoning);
                        }
                    }
                }
                UpdateResult::dirty()
            }
            Input::Event(event) => UpdateResult {
                dirty: true,
                effects: self.apply_event(&event),
            },
            Input::Runtime(event) => self.apply_runtime_event(&event),
            Input::Pending(head, len) => self.sync_pending(head, len),
            Input::Form(key) => self.form_key(key),
            Input::MentionsExpanded(result) => self.mentions_expanded(result),
            Input::CompletionReady(completion) => self.completion_ready(completion),
            Input::CompletionsReady { fetch, candidates } => {
                self.completions_ready(fetch, candidates)
            }
            Input::MemoryCommandDone(result) => self.memory_command_done(result),
            Input::CompactDone(result) => self.compact_done(result),
            Input::BashDone {
                command,
                exclude_from_context,
                result,
            } => self.bash_done(command, exclude_from_context, result),
            Input::ThinkingChecked {
                requested,
                applied,
                supported,
            } => self.thinking_checked(requested, applied, supported),
            Input::ModelChecked {
                provider,
                model,
                tokens,
                budget,
            } => self.model_checked(provider, model, tokens, budget),
            Input::SwitchCompacted {
                provider,
                model,
                result,
            } => self.switch_compacted(provider, model, result),
            Input::SessionDone(notices) => {
                for notice in notices {
                    self.push_notice(notice);
                }
                UpdateResult::dirty()
            }
            Input::TrustMove(delta) => {
                if let Some(Modal::Trust(wizard)) =
                    self.layers.iter_mut().rev().find_map(|layer| match layer {
                        Layer::Modal(modal) => Some(modal),
                        _ => None,
                    })
                {
                    wizard.move_cursor(delta);
                }
                UpdateResult::dirty()
            }
            Input::TrustChosen(choice) => {
                self.remove_modal(|modal| matches!(modal, Modal::Trust(_)));
                UpdateResult::effect(Effect::SetTrust { choice })
            }
            Input::TrustResolved(setup) => {
                self.push_notice(radian_host::trust::status_line(&setup));
                UpdateResult::dirty()
            }
            Input::AccessMove(delta) => {
                if let Some(Modal::AccessFull(wizard)) =
                    self.layers.iter_mut().rev().find_map(|layer| match layer {
                        Layer::Modal(modal) => Some(modal),
                        _ => None,
                    })
                {
                    wizard.move_cursor(delta);
                }
                UpdateResult::dirty()
            }
            Input::AccessChosen(choice) => {
                self.remove_modal(|modal| matches!(modal, Modal::AccessFull(_)));
                match choice {
                    AccessChoice::Cancel => {
                        self.push_notice("access unchanged");
                        UpdateResult::dirty()
                    }
                    AccessChoice::Session => UpdateResult::effect(Effect::SetAccess {
                        tier: crate::profile::Tier::FullAccess,
                        remember: false,
                    }),
                    AccessChoice::Remember => UpdateResult::effect(Effect::SetAccess {
                        tier: crate::profile::Tier::FullAccess,
                        remember: true,
                    }),
                }
            }
            Input::AccessApplied {
                tier,
                remember,
                result,
            } => {
                match result {
                    Ok(()) => {
                        // The startup source is spent: this is a run-time switch, remembered or
                        // not. The footer's tier item reads the projection, so re-mark it.
                        self.session.access_source = if remember {
                            crate::profile::AccessSource::Project
                        } else {
                            crate::profile::AccessSource::Session
                        };
                        self.set_tier(format!(
                            "{}{}",
                            tier.as_str(),
                            self.session.access_source.suffix()
                        ));
                        // `!`/`!!` exist whenever the new tier has a sandbox (`full` and
                        // `workspace`; not `safe`).
                        self.set_bash_available(tier != crate::profile::Tier::Safe);
                        if remember {
                            self.push_notice(format!(
                                "access: {} for this session, and remembered for this project",
                                tier.as_str()
                            ));
                        } else {
                            self.push_notice(format!("access: {} for this session", tier.as_str()));
                        }
                        // ADR 0036: the tool list (and the startup line)
                        // is computed once, when the session opens. The switch changes what
                        // runs from the next call, not the advertised text, so say so rather
                        // than leave the description stale.
                        self.push_notice(
                            "the shell tool's description and the startup line are fixed for \
                             this session; a new session shows the new tier",
                        );
                    }
                    Err(reason) => self.push_notice(format!("cannot switch access: {reason}")),
                }
                UpdateResult::dirty()
            }
            Input::SkillExpanded { result, steer } => self.skill_expanded(result, steer),
            Input::CommandRefused(refused) => {
                self.push_notice(format!(
                    "the session refused `{}`: {}",
                    refused.command, refused.reason
                ));
                UpdateResult::dirty()
            }
            Input::PickerKey(key) => self.picker_key(key),
            Input::PickerClose => {
                self.picker_close();
                UpdateResult::dirty()
            }
            Input::PickerOpened(data) => self.picker_opened(data),
            // The `/login` wizard's keys, its verification result, and the save/removal
            // results (ADR 0026 D3).
            Input::LoginKey(key) => {
                let outcome = self
                    .login_modal_mut()
                    .map(|wizard| wizard.on_key(key))
                    .unwrap_or(LoginOutcome::Stay);
                self.login_outcome(outcome)
            }
            Input::LoginVerified { name: _, result } => {
                // Only a wizard still waiting consumes the answer (D3).
                let waiting = matches!(
                    self.top_modal(),
                    Some(Modal::Login(wizard))
                        if matches!(wizard.step, crate::login::LoginStep::Verifying)
                );
                if !waiting {
                    return UpdateResult::none();
                }
                let outcome = self
                    .login_modal_mut()
                    .and_then(|wizard| wizard.verified(&result))
                    .unwrap_or(LoginOutcome::Stay);
                self.login_outcome(outcome)
            }
            Input::LoginSaved { name, result } => {
                self.remove_modal(|modal| matches!(modal, Modal::Login(_)));
                match result {
                    Ok(()) => {
                        // D3's required "what next" line: how to actually use the provider.
                        self.push_notice(format!(
                            "saved the key for `{name}` in credentials/auth.json; switch to it \
                             with /model {name}/<model> (or set model.provider)"
                        ));
                    }
                    Err(error) => self.push_notice(format!("could not save the key: {error}")),
                }
                UpdateResult::dirty()
            }
            Input::LoginOAuthUrlReady { url } => {
                if let Some(wizard) = self.login_modal_mut() {
                    wizard.oauth_browser_url_ready(&url);
                }
                UpdateResult::dirty()
            }
            Input::LoginOAuthDeviceReady {
                user_code,
                verification_url,
            } => {
                if let Some(wizard) = self.login_modal_mut() {
                    wizard.oauth_device_ready(&user_code, &verification_url);
                }
                UpdateResult::dirty()
            }
            Input::ConfirmLogout(confirmed) => {
                let Some(provider) = self.pending_logout().map(str::to_owned) else {
                    return UpdateResult::none();
                };
                self.remove_modal(|modal| matches!(modal, Modal::ConfirmLogout(_)));
                if confirmed {
                    UpdateResult::effect(Effect::Logout { provider })
                } else {
                    self.push_notice(format!("kept `{provider}`"));
                    UpdateResult::dirty()
                }
            }
            Input::LogoutDone { provider, result } => {
                match result {
                    Ok(()) => {
                        self.push_notice(format!("removed the stored credential for `{provider}`"))
                    }
                    Err(error) => {
                        self.push_notice(format!("could not remove `{provider}`: {error}"))
                    }
                }
                UpdateResult::dirty()
            }
            Input::MouseDown { row, col } => self.mouse_down(row, col),
            Input::MouseDrag { row, col } => self.mouse_drag(row, col),
            Input::MouseUp { row, col } => self.mouse_up(row, col),
            // The loop's clipboard write finished; its notice says what Radian did ("copied N
            // chars (via OSC 52)"), never that the clipboard holds it — OSC 52 has no receipt.
            Input::ClipboardDone(notice) => {
                self.session.note = Some(notice);
                UpdateResult::dirty()
            }
            Input::ThemeReady { theme, notices } => {
                if let Some(theme) = theme {
                    self.theme = *theme;
                }
                for notice in notices {
                    self.push_notice(notice);
                }
                UpdateResult::dirty()
            }
        };
        if disarmed {
            result.dirty = true;
        }
        result
    }

    /// Resolves one key event to an [`Input`] through the keymap and the layer stack
    /// (ADR 0023 U4).
    ///
    /// Order: (2) a modal's hardcoded keys (`y`/`a`/`n`/`Esc`) win over the keymap, even over a
    /// `cancel` rebound onto them; then (1) the bound `cancel` action runs before the stack; then
    /// the stack is walked top-down, the first layer that consumes the key winning and an
    /// unconsumed key falling toward the [`Layer::Surface`].
    pub fn key_input(&mut self, key: &KeyEvent) -> Option<Input> {
        // Invariant 2 (ADR 0023 U4): a modal's own keys are hardcoded, not read from the keymap.
        if self.has_modal()
            && let Some(input) = self.modal_hardcoded_key(key)
        {
            return Some(input);
        }
        // Invariant 1 (ADR 0023 U4): the `cancel` action runs before the whole stack. Its
        // default key is Ctrl-C; a rebind moves it, and it still wins over every overlay.
        let action = self.keymap.action_for(key);
        if action == Some(Action::Cancel) {
            return Some(Input::Action(Action::Cancel));
        }
        // The question form is the one modal whose keys are rebindable (ADR 0024 Q7): its own
        // actions come from the `form` layer, and everything else that can edit text goes to
        // the focused field. A key the form does not want is swallowed, never passed down.
        if let Some(Modal::Question(form)) = self.top_modal() {
            // `alt-h` collapses the panel (ADR 0024 R3): it is the form's own key,
            // not a rebindable `form` action.
            if key.code == KeyCode::Char('h') && key.modifiers.contains(KeyModifiers::ALT) {
                return Some(Input::Form(FormKey::Hide));
            }
            // ←/→ switch questions while no free-text field has focus (the hint row says so);
            // with the `Other` row, a `text` question or a note focused they fall through to
            // the composer's cursor actions below. feedback: ←/→ used to do nothing
            // outside a field.
            let field_focused = form.note_focused() || form.text_focused();
            if !field_focused && key.modifiers.is_empty() {
                match key.code {
                    KeyCode::Left => {
                        return Some(Input::Form(FormKey::Action(Action::FormPrev)));
                    }
                    KeyCode::Right => {
                        return Some(Input::Form(FormKey::Action(Action::FormNext)));
                    }
                    _ => {}
                }
            }
            if let Some(action) = self.keymap.form_action_for(key) {
                return Some(Input::Form(FormKey::Action(action)));
            }
            if let KeyCode::Char(c) = key.code
                && !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
            {
                return Some(Input::Form(FormKey::Char(c)));
            }
            if let Some(action) = self.keymap.action_for(key) {
                return Some(Input::Form(FormKey::Action(action)));
            }
            return None;
        }

        /// What a layer did with one key.
        enum Step {
            /// The layer turned it into an input.
            Input(Input),
            /// The layer took the key and did nothing with it; it does not fall through.
            Swallow,
            /// The layer did not want the key; keep walking down.
            Pass,
            /// An unfocused picker: typing dismisses it; keep walking down.
            DropPicker,
        }

        let mut index = self.layers.len();
        while index > 0 {
            index -= 1;
            let step = match &self.layers[index] {
                // Its hardcoded keys and `cancel` were handled above; every other key stops
                // here rather than reaching the transcript.
                Layer::Modal(_) => Step::Swallow,
                Layer::Overlay(Overlay::Picker(picker)) => {
                    if picker.is_focused() {
                        Step::Input(Input::PickerKey(*key))
                    } else {
                        match key.code {
                            // Navigation focuses the picker and moves the selection.
                            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                                Step::Input(Input::PickerKey(*key))
                            }
                            KeyCode::Esc => Step::Input(Input::PickerClose),
                            // Typing dismisses the overlay and goes to the composer.
                            _ => Step::DropPicker,
                        }
                    }
                }
                // The find box is a layer over the transcript: while it is open its keys are
                // looked up first, so `enter` is the earlier match there and the send key in
                // the transcript. `ctrl-f` is both "open the box" and "earlier match"; `esc`
                // closes it.
                Layer::Overlay(Overlay::Find(_)) => {
                    if let Some(found) = self.keymap.find_action_for(key) {
                        Step::Input(Input::Action(found))
                    } else {
                        match key.code {
                            KeyCode::Backspace => Step::Input(Input::FindBackspace),
                            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                                Step::Input(Input::Char(c))
                            }
                            // PageUp/PageDown, Home/End and the arrows keep reaching the
                            // transcript layer, so the box does not trap navigation.
                            _ => Step::Pass,
                        }
                    }
                }
                Layer::Overlay(Overlay::Completion(_)) => Step::Pass,
                Layer::Surface => {
                    if let Some(found) = action {
                        Step::Input(Input::Action(found))
                    } else if let KeyCode::Char(c) = key.code
                        && !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT)
                    {
                        Step::Input(Input::Char(c))
                    } else {
                        Step::Pass
                    }
                }
            };
            match step {
                Step::Input(input) => return Some(input),
                Step::Swallow => return None,
                Step::Pass => continue,
                Step::DropPicker => {
                    self.layers.remove(index);
                    continue;
                }
            }
        }
        None
    }

    /// The hardcoded key of the topmost modal, if `key` is one of them (ADR 0023 U4, invariant
    /// 2). `None` means the modal does not own this key; the caller then checks `cancel` and the
    /// stack.
    fn modal_hardcoded_key(&self, key: &KeyEvent) -> Option<Input> {
        match self.top_modal()? {
            Modal::Approval(_) => match key.code {
                // The auto key `!` (ADR 0026 D9, the constant in `render.rs`): approve this
                // request once and turn auto on for the session. It must not take a scroll
                // key, and it is un-rebindable like every answer key (ADR 0023 U4).
                KeyCode::Char('!') => Some(Input::ApprovalAuto),
                // `Y` is `y` (allow once); it never switches auto on (correction
                // 2026-10-13: a Caps-Lock `y` must not silently enable auto).
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    Some(Input::ApprovalAnswer(ApprovalOutcome::AllowOnce))
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    Some(Input::ApprovalAnswer(ApprovalOutcome::AllowForSession))
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    Some(Input::ApprovalAnswer(ApprovalOutcome::Deny {
                        reason: None,
                    }))
                }
                // The body's scroll keys (approval-fit). They are not answer keys, so they
                // cannot be confused with one, and the answers stay un-rebindable. The auto
                // key `!` (ADR 0026 D9) shares this property: it is no scroll key either.
                KeyCode::Up => Some(Input::ApprovalScroll(ApprovalScroll::LineUp)),
                KeyCode::Down => Some(Input::ApprovalScroll(ApprovalScroll::LineDown)),
                KeyCode::PageUp => Some(Input::ApprovalScroll(ApprovalScroll::PageUp)),
                KeyCode::PageDown => Some(Input::ApprovalScroll(ApprovalScroll::PageDown)),
                _ => None,
            },
            // The question form's keys are the rebindable `form` layer (ADR 0024 Q7), not
            // hardcoded: `key_input` looks them up when this modal is on top.
            Modal::Question(_) => None,
            Modal::ConfirmCommand(_) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(Input::ConfirmCommand(true)),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    Some(Input::ConfirmCommand(false))
                }
                _ => None,
            },
            // The `/logout` confirmation is hardcoded like the `/memory` one (ADR 0023 U4).
            Modal::ConfirmLogout(_) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => Some(Input::ConfirmLogout(true)),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    Some(Input::ConfirmLogout(false))
                }
                _ => None,
            },
            // The startup trust prompt's keys are hardcoded (ADR 0031 T3): a startup decision
            // must not depend on the keymap, and Enter takes the fail-safe default.
            Modal::Trust(wizard) => match key.code {
                KeyCode::Up => Some(Input::TrustMove(-1)),
                KeyCode::Down => Some(Input::TrustMove(1)),
                KeyCode::Enter => Some(Input::TrustChosen(wizard.selected())),
                // Esc is "do not trust, this session only": it must not
                // write `trust.json`, so the next start asks again. Enter takes the
                // highlighted default, which is the persistent "do not trust this directory".
                KeyCode::Esc => Some(Input::TrustChosen(
                    radian_host::trust::Choice::UntrustSession,
                )),
                _ => None,
            },
            // The `/access full` confirmation's keys are hardcoded (ADR 0038 slice 2): a tier
            // switch to "no sandbox" must not depend on the keymap. Enter takes the highlighted
            // option (default `cancel`); Esc is `cancel`.
            Modal::AccessFull(wizard) => match key.code {
                KeyCode::Up => Some(Input::AccessMove(-1)),
                KeyCode::Down => Some(Input::AccessMove(1)),
                KeyCode::Enter => Some(Input::AccessChosen(wizard.selected())),
                KeyCode::Esc => Some(Input::AccessChosen(AccessChoice::Cancel)),
                _ => None,
            },
            // The `/login` wizard's keys are hardcoded too: a login prompt is sensitive, and
            // the key field must behave identically whoever rebinds what (ADR 0026 D3).
            Modal::Login(_) => match key.code {
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    Some(Input::LoginKey(LoginKey::Char(c)))
                }
                KeyCode::Backspace => Some(Input::LoginKey(LoginKey::Backspace)),
                KeyCode::Enter => Some(Input::LoginKey(LoginKey::Enter)),
                KeyCode::Esc => Some(Input::LoginKey(LoginKey::Esc)),
                KeyCode::Up => Some(Input::LoginKey(LoginKey::Up)),
                KeyCode::Down => Some(Input::LoginKey(LoginKey::Down)),
                _ => None,
            },
        }
    }

    /// Handles one key for the picker overlay. An in-session picker opens unfocused (the
    /// composer keeps the keys); the first navigation focuses it. Selecting does not switch
    /// sessions in place (ADR 0022/0023): it shows the `--resume <id>` line to copy.
    pub(super) fn picker_key(&mut self, key: KeyEvent) -> UpdateResult {
        if let Some(picker) = self.picker_mut()
            && !picker.is_focused()
        {
            picker.focus();
        }
        let outcome = self.picker_mut().and_then(|picker| picker.on_key(key));
        let Some(outcome) = outcome else {
            return UpdateResult::dirty();
        };
        self.picker_close();
        if self.startup == Startup::Picking {
            // The pre-session phase: hand the choice to the loop, which leaves and lets the
            // caller open the session in the same terminal. `Quit` also sets `quit` so the
            // loop leaves even if the caller does not read the outcome.
            if outcome == PickerOutcome::Quit {
                self.quit = true;
            }
            self.picker_outcome = Some(outcome);
            return UpdateResult::dirty();
        }
        match outcome {
            // In-session the picker's `Esc` closes the overlay, it does not quit the app.
            PickerOutcome::Quit => UpdateResult::dirty(),
            PickerOutcome::New => {
                self.push_notice(
                    "a new session starts on the next launch (run `radian` with no `--resume`)",
                );
                UpdateResult::dirty()
            }
            PickerOutcome::Resume(id) => {
                self.push_notice(format!(
                    "--resume {id} (this process does not switch sessions in place)"
                ));
                UpdateResult::dirty()
            }
        }
    }

    /// Pushes the picker overlay from the loop's loaded rows. An in-session overlay opens
    /// unfocused, so it informs without stealing the composer (ADR 0023 U4).
    fn picker_opened(&mut self, data: PickerData) -> UpdateResult {
        let mut picker =
            Picker::new(data.rows, data.index, data.workspace).with_secrets(data.secrets);
        if let Some(query) = data.query {
            picker = picker.with_search(query);
        }
        self.push_overlay(Overlay::Picker(picker));
        UpdateResult::dirty()
    }

    /// The terminal's entry point: resolve the key, then [`App::update`]. A thin wrapper —
    /// everything that changes state lives in `update`.
    pub fn on_key(&mut self, key: KeyEvent) -> UpdateResult {
        self.on_key_at(key, Instant::now())
    }

    /// Like [`App::on_key`], with the wall clock supplied: only the Ctrl-C quit window
    /// depends on it, so production passes the real clock and a test injects its own.
    pub fn on_key_at(&mut self, key: KeyEvent, now: Instant) -> UpdateResult {
        match self.key_input(&key) {
            // The cancel action (`Ctrl-C` by default) carries its timestamp: idle with an
            // empty input it arms the quit window instead of quitting at once.
            Some(Input::Action(Action::Cancel)) => self.update(Input::Cancel { now }),
            Some(input) => self.update(input),
            None => UpdateResult::none(),
        }
    }

    /// The current turn state, read from the projection (ADR 0023 U5). `update` reads it to
    /// decide between cancel and quit, and between a steer and a follow-up; it never writes it.
    pub(super) fn turn(&self) -> TurnState {
        self.session.projection.read().turn()
    }

    /// Ctrl-C (the `cancel` action), the one action whose meaning depends on the clock
    ///:
    /// - a turn is running: cancel it (unchanged);
    /// - idle, the input has text: clear it into the history (the same clearing `Esc` does,
    ///   so `Up` finds it);
    /// - idle, the input is empty: the first press arms [`QUIT_WINDOW`]; a second press
    ///   inside the window quits; a press after it re-arms.
    fn cancel_or_quit_at(&mut self, now: Instant) -> UpdateResult {
        if self.turn() == TurnState::Running {
            self.quit_pending = None;
            return UpdateResult::quiet(Effect::Cancel);
        }
        if !self.input.is_empty() {
            let text = std::mem::take(&mut self.input).text();
            self.session.history.push(text);
            self.session.history_cursor = None;
            self.session.history_stash.clear();
            self.quit_pending = None;
            return UpdateResult::dirty();
        }
        match self.quit_pending.take() {
            Some(deadline) if now <= deadline => {
                self.quit = true;
                UpdateResult::quiet(Effect::Quit)
            }
            // The first press, or a second one after the window closed (re-arm, do not
            // double-fire): arm the deadline and redraw the hint.
            _ => {
                self.quit_pending = Some(now + QUIT_WINDOW);
                UpdateResult::dirty()
            }
        }
    }

    /// Runs one keymap action. The default bindings reproduce the pre-keymap key table.
    pub(super) fn update_action(&mut self, action: Action) -> UpdateResult {
        match action {
            Action::Send => {
                // A completion list on screen means Enter picks the candidate instead of
                // sending: it completes the line and never executes.
                if self.slash_completion.is_some() {
                    return self.accept_slash_candidate();
                }
                let text = std::mem::take(&mut self.input).text();
                self.session.history_cursor = None;
                self.session.history_stash.clear();
                if text.trim().is_empty() {
                    return UpdateResult::dirty();
                }
                // Every submitted line (messages and `//` escapes and `/…` commands) joins the
                // session history; browsing edits never change it.
                self.session.history.push(text.clone());
                let trimmed = text.trim();
                // `//x` is the escape: send it as the ordinary message `/x`.
                if let Some(escaped) = trimmed.strip_prefix("//") {
                    return self.begin_send(format!("/{escaped}"));
                }
                // A `/…` line is a command: it never reaches the model or the journal. A `!`
                // line is the user's shell command (ADR 0033), routed before `/` so `!/bin/sh`
                // is a command and not the unknown `/bin/sh`.
                if trimmed.starts_with('/') || trimmed.starts_with('!') {
                    return self.run_slash_command(trimmed);
                }
                self.begin_send(text)
            }
            Action::Newline => {
                self.input.insert_char('\n');
                UpdateResult::dirty()
            }
            Action::CursorLeft => {
                self.input.left();
                UpdateResult::dirty()
            }
            Action::CursorRight => {
                self.input.right();
                UpdateResult::dirty()
            }
            Action::WordLeft => {
                self.input.word_left();
                UpdateResult::dirty()
            }
            Action::WordRight => {
                self.input.word_right();
                UpdateResult::dirty()
            }
            Action::LineStart => {
                self.input.line_start();
                UpdateResult::dirty()
            }
            Action::LineEnd => {
                self.input.line_end();
                UpdateResult::dirty()
            }
            Action::HistoryPrev => {
                if self.input.is_first_line() {
                    self.history_up();
                } else {
                    self.input.up();
                }
                UpdateResult::dirty()
            }
            Action::HistoryNext => {
                if self.input.is_last_line() {
                    self.history_down();
                } else {
                    self.input.down();
                }
                UpdateResult::dirty()
            }
            Action::DeleteBefore => {
                self.input.backspace();
                UpdateResult::dirty()
            }
            Action::DeleteUnder => {
                self.input.delete();
                UpdateResult::dirty()
            }
            // Tab: complete the `@` file reference at the cursor (Phase 4). On any other word
            // it does nothing, so the key stays free for a future binding. Reading the
            // directory is the loop's job: this only asks for it.
            Action::Complete => {
                // A `/…` line completes through the command tree (every level, one fuzzy
                // match); anything else completes `@` file references.
                if self.input.text().starts_with('/') {
                    return self.accept_slash_candidate();
                }
                if self.session.workspace.is_none() {
                    self.completion_clear();
                    return UpdateResult::dirty();
                }
                UpdateResult::quiet(Effect::CompleteMention {
                    text: self.input.text(),
                    cursor: self.input.cursor(),
                })
            }
            // --- in-transcript search (Phase 5) ---
            // `find` opens the box; a second `find` (Ctrl-F by default) is the earlier match,
            // which is exactly the historical behavior.
            Action::Find => {
                if self.find_state().is_some() {
                    self.find_prev();
                } else {
                    self.find_open();
                }
                UpdateResult::dirty()
            }
            Action::FindPrev => {
                self.find_prev();
                UpdateResult::dirty()
            }
            Action::FindNext => {
                self.find_next();
                UpdateResult::dirty()
            }
            Action::FindClose => {
                self.find_close();
                UpdateResult::dirty()
            }
            // ADR 0028 R3: Ctrl-T folds or unfolds the thinking blocks. The mode is part of
            // the block/layout cache keys, so the next frame re-lays them out.
            Action::ToggleReasoning => {
                self.reasoning_mode = self.reasoning_mode.toggled();
                // A global fold toggle drops the per-entry search overrides (ADR 0028 R6).
                self.clear_fold_overrides();
                UpdateResult::dirty()
            }
            // ADR 0028 R4: Ctrl-O steps the tool bodies through errors -> expanded ->
            // collapsed -> errors, pi's three modes in one key.
            Action::ToggleToolOutput => {
                self.tool_display.fold = match self.tool_display.fold {
                    crate::tools::ToolFold::Errors => crate::tools::ToolFold::Expanded,
                    crate::tools::ToolFold::Expanded => crate::tools::ToolFold::Collapsed,
                    crate::tools::ToolFold::Collapsed => crate::tools::ToolFold::Errors,
                };
                self.clear_fold_overrides();
                UpdateResult::dirty()
            }
            Action::DeleteWord => {
                self.input.delete_word();
                UpdateResult::dirty()
            }
            Action::DeleteToLineStart => {
                self.input.kill_to_line_start();
                UpdateResult::dirty()
            }
            Action::DeleteToLineEnd => {
                self.input.kill_to_line_end();
                UpdateResult::dirty()
            }
            Action::ScrollPageUp => {
                self.scroll_by(-(self.session.view_height.max(1) as i32));
                UpdateResult::dirty()
            }
            Action::ScrollPageDown => {
                self.scroll_by(self.session.view_height.max(1) as i32);
                UpdateResult::dirty()
            }
            Action::ScrollLineUp => {
                self.scroll_by(-1);
                UpdateResult::dirty()
            }
            Action::ScrollLineDown => {
                self.scroll_by(1);
                UpdateResult::dirty()
            }
            Action::ScrollTop => {
                self.set_scroll(0);
                UpdateResult::dirty()
            }
            Action::ScrollBottom => {
                self.scroll_to_bottom();
                UpdateResult::dirty()
            }
            Action::Escape => {
                // Esc never quits an idle session. In order: the menu and
                // the find box are closed in `key_input`/`update` above; a non-empty input is
                // cleared (kept in the history, so `Up` finds it); an empty one cancels the
                // running `!` command or turn; otherwise it does nothing at all. Quitting is the
                // two-press Ctrl-C (`cancel`) or `/quit`.
                if !self.input.is_empty() {
                    let text = std::mem::take(&mut self.input).text();
                    self.session.history.push(text);
                    self.session.history_cursor = None;
                    self.session.history_stash.clear();
                    UpdateResult::dirty()
                } else if self.session.bash_running.is_some() {
                    // Esc cancels the running `!` command (ADR 0033); the task reports back
                    // as a cancelled `BashDone`.
                    self.push_notice("cancelling the command…");
                    UpdateResult::effect(Effect::CancelBash)
                } else if self.turn() == TurnState::Running {
                    // Esc cancels the running turn (unchanged). A pending Ctrl-C quit window
                    // is disarmed, as the old escalation did.
                    self.quit_pending = None;
                    UpdateResult::quiet(Effect::Cancel)
                } else {
                    UpdateResult::none()
                }
            }
            // The form actions normally arrive as `Input::Form`; a main-layer binding to one
            // would still land here, so route it to the form rather than dropping it.
            Action::FormNext
            | Action::FormPrev
            | Action::FormUp
            | Action::FormDown
            | Action::FormSelect
            | Action::FormSubmit
            | Action::FormDecline => self.form_key(FormKey::Action(action)),
            Action::Cancel => self.cancel_or_quit_at(Instant::now()),
            // `Ctrl-P` folds the startup information.
            Action::ToggleStartup => self.toggle_startup(),
            // Inert without a menu; `update` intercepts it while one is open.
            Action::MenuNext => UpdateResult::none(),
        }
    }
}
