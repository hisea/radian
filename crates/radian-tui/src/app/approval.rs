//! The approval prompt and the structured question: showing the queue's head (`sync_pending`),
//! the hardcoded approval keys (`y`/`a`/`n`, the `!` auto switch, the body's scroll), the answer
//! that removes the modal and behaves the same in pure tests as in the loop, and the `form`
//! layer's answers for an `ask_user` question.

use super::*;

impl App {
    /// Shows the queue's head (ADR 0023 U7). The head is the only pending request on screen:
    /// a second request waits in the queue instead of superseding the first. A head that is
    /// already shown keeps its state, so a question's drafts survive a redraw.
    pub(super) fn sync_pending(
        &mut self,
        head: Option<Box<PendingView<PendingRequest>>>,
        len: usize,
    ) -> UpdateResult {
        self.pending = len;
        let head_id = head.as_ref().map(|view| view.request_id);
        if head_id != self.last_answered {
            // The head moved on, so a request answered a moment ago is gone for good.
            self.last_answered = None;
        }
        let showing = self.layers.iter().rev().find_map(|layer| match layer {
            Layer::Modal(Modal::Approval(prompt)) => Some(prompt.request_id),
            Layer::Modal(Modal::Question(form)) => Some(form.request_id),
            _ => None,
        });
        if showing == head_id {
            return UpdateResult::dirty();
        }
        // A different request (or none): drop the old modal and show the new head.
        self.remove_modal(|modal| matches!(modal, Modal::Approval(_) | Modal::Question(_)));
        let Some(view) = head else {
            return UpdateResult::dirty();
        };
        if Some(view.request_id) == self.last_answered {
            return UpdateResult::dirty();
        }
        match view.request {
            PendingRequest::Approval(request) => {
                self.push_modal(Modal::Approval(Box::new(PendingApproval {
                    request_id: view.request_id,
                    origin: view.origin,
                    request,
                    // A new question starts at the top of its body.
                    scroll: 0,
                    total_rows: 0,
                    visible_rows: 0,
                })))
            }
            PendingRequest::Question(request) => self.push_modal(Modal::Question(Box::new(
                QuestionForm::new(view.request_id, view.origin, request.questions),
            ))),
        }
        // A newly shown approval or question rings the bell in every mode but `off`. Showing
        // the same head again (the early return above) never re-rings: one prompt, one ring.
        if self.bell != BellMode::Off {
            return UpdateResult::effect(Effect::Bell);
        }
        UpdateResult::dirty()
    }

    /// Scrolls the approval box's body (approval-fit). The window is clamped against what the
    /// last frame measured, so a key before the first draw (or past the end) moves nothing.
    pub(super) fn approval_scroll(&mut self, direction: ApprovalScroll) -> UpdateResult {
        let Some(index) = self
            .layers
            .iter()
            .rposition(|layer| matches!(layer, Layer::Modal(Modal::Approval(_))))
        else {
            return UpdateResult::none();
        };
        let Layer::Modal(Modal::Approval(prompt)) = &mut self.layers[index] else {
            return UpdateResult::none();
        };
        let visible = prompt.visible_rows.max(1);
        let max = prompt.total_rows.saturating_sub(visible);
        prompt.scroll = match direction {
            ApprovalScroll::LineUp => prompt.scroll.saturating_sub(1),
            ApprovalScroll::LineDown => (prompt.scroll + 1).min(max),
            ApprovalScroll::PageUp => prompt.scroll.saturating_sub(visible),
            ApprovalScroll::PageDown => (prompt.scroll + visible).min(max),
        };
        UpdateResult::dirty()
    }

    /// Answers the pending approval and clears it. A no-op when nothing is pending.
    pub(super) fn answer_approval(&mut self, outcome: ApprovalOutcome) -> UpdateResult {
        match self.remove_modal(|modal| matches!(modal, Modal::Approval(_))) {
            Some(Modal::Approval(prompt)) => {
                self.last_answered = Some(prompt.request_id);
                UpdateResult::quiet(Effect::AnswerRequest {
                    request_id: prompt.request_id,
                    response: PendingResponse::Approval(outcome),
                })
            }
            _ => UpdateResult::none(),
        }
    }

    /// The approval box's `!` key (ADR 0026 D9): approve the pending request once **and**
    /// turn auto on for the rest of the session, in one press, no confirmation (owner's
    /// correction 2026-10-13: `!` had no meaning before, so it cannot be mistyped into one).
    /// The effects run in order: the answer first, then the switch. The switch stands even
    /// with nothing pending — it is the same switch `/auto on` makes.
    pub(super) fn approval_auto_key(&mut self) -> UpdateResult {
        let mut effects = Vec::new();
        if let Some(Modal::Approval(prompt)) =
            self.remove_modal(|modal| matches!(modal, Modal::Approval(_)))
        {
            self.last_answered = Some(prompt.request_id);
            effects.push(Effect::AnswerRequest {
                request_id: prompt.request_id,
                response: PendingResponse::Approval(ApprovalOutcome::AllowOnce),
            });
        }
        self.session.auto = true;
        self.push_notice(
            "auto: tool calls are approved without asking for the rest of this session; \
             deny rules and the sandbox still apply",
        );
        effects.push(Effect::SetApprovalPolicy { on: true });
        UpdateResult {
            dirty: true,
            effects,
        }
    }

    /// One key inside the question form (ADR 0024 Q7).
    pub(super) fn form_key(&mut self, key: FormKey) -> UpdateResult {
        let Some(index) = self
            .layers
            .iter()
            .rposition(|layer| matches!(layer, Layer::Modal(Modal::Question(_))))
        else {
            return UpdateResult::none();
        };
        let outcome = match &mut self.layers[index] {
            Layer::Modal(Modal::Question(form)) => form.key(key),
            _ => return UpdateResult::none(),
        };
        match outcome {
            FormOutcome::Stay => UpdateResult::dirty(),
            FormOutcome::Blocked(id) => {
                self.push_notice(format!(
                    "`{id}` is required: answer it before submitting (Tab to it)"
                ));
                UpdateResult::dirty()
            }
            FormOutcome::Answer(answer) => {
                let request_id = self.question_request_id(index);
                self.remove_modal(|modal| matches!(modal, Modal::Question(_)));
                self.last_answered = request_id;
                UpdateResult::quiet(Effect::AnswerRequest {
                    request_id: request_id.unwrap_or_default(),
                    response: PendingResponse::Question(Ok(answer)),
                })
            }
            FormOutcome::Decline => {
                let request_id = self.question_request_id(index);
                self.remove_modal(|modal| matches!(modal, Modal::Question(_)));
                self.last_answered = request_id;
                UpdateResult::quiet(Effect::AnswerRequest {
                    request_id: request_id.unwrap_or_default(),
                    response: PendingResponse::Question(Err(PromptError::Declined)),
                })
            }
        }
    }

    pub(super) fn question_request_id(&self, index: usize) -> Option<u64> {
        match self.layers.get(index) {
            Some(Layer::Modal(Modal::Question(form))) => Some(form.request_id),
            _ => None,
        }
    }
}
