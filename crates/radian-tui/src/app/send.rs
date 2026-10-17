//! The send path and the results the loop hands back: [`App::begin_send`] and the `/memory`
//! confirmation, the `@`-mention and `/skill` expansions, the `/…` completion state
//! (`refresh_slash`, `set_slash`, `accept_slash_candidate`, `completions_ready`), the fetched
//! `@` completions, and the `/memory` and `/compact` outcomes.

use super::*;

impl App {
    /// The one notice a submit gets once the session ended (the driver stopped): no turn can
    /// start, so say so and say how to continue the session. Nothing is accepted into an inbox
    /// that no driver will read.
    pub(super) fn session_ended_notice(&self) -> String {
        let reason = self
            .session
            .session_ended
            .as_deref()
            .unwrap_or("the session ended");
        format!(
            "the session has ended ({reason}); the message was not sent — resume the session \
             with `radian --resume {}`",
            self.session.session_id
        )
    }

    /// Refuses a submit after the session ended, with the notice above. `None` means the
    /// session is live and the caller goes on.
    pub(super) fn refuse_if_session_ended(&mut self) -> Option<UpdateResult> {
        self.session.session_ended.as_ref()?;
        self.push_notice(self.session_ended_notice());
        Some(UpdateResult::dirty())
    }

    /// Starts a send. The `@` references are expanded by the loop first (an effect), and the
    /// result becomes the follow-up or steer effect. Whether it steers is decided here, because
    /// the turn state is app state.
    pub(super) fn begin_send(&mut self, text: String) -> UpdateResult {
        if let Some(refused) = self.refuse_if_session_ended() {
            self.restore_input(text);
            return refused;
        }
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
    pub(super) fn confirm_command(&mut self, confirmed: bool) -> UpdateResult {
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

    /// A `/skill:<name>` expansion finished (ADR 0025 S3). On success the message is sent like
    /// a typed one (steer when a turn was running); on failure the notice is shown and nothing
    /// is sent — an unknown skill is not a message.
    pub(super) fn skill_expanded(
        &mut self,
        result: Result<String, String>,
        steer: bool,
    ) -> UpdateResult {
        match result {
            Ok(text) => {
                if let Some(refused) = self.refuse_if_session_ended() {
                    return refused;
                }
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
    pub(super) fn mentions_expanded(
        &mut self,
        result: Result<Expansion, ExpandError>,
    ) -> UpdateResult {
        let Some(pending) = self.pending_expand.take() else {
            return UpdateResult::none();
        };
        match result {
            Ok(expansion) => {
                if let Some(refused) = self.refuse_if_session_ended() {
                    self.restore_input(pending.text);
                    return refused;
                }
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
    pub(super) fn completion_ready(&mut self, completion: Option<Completion>) -> UpdateResult {
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
    pub(super) fn refresh_slash(&mut self) -> Vec<Effect> {
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
    pub(super) fn accept_slash_candidate(&mut self) -> UpdateResult {
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
    pub(super) fn completions_ready(
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
    pub(super) fn memory_command_done(&mut self, result: MemoryResult) -> UpdateResult {
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
    pub(super) fn compact_done(
        &mut self,
        result: Result<CompactionOutcome, String>,
    ) -> UpdateResult {
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
}
