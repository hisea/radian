//! The structured-question form (ADR 0024 Q7): the state of one `ask_user` request, as a
//! [`Modal`](crate::app::Modal) over the transcript.
//!
//! Pure, like [`App`](crate::app::App): it turns keys into state changes and, at the end, one
//! [`QuestionAnswer`]. It never answers the queue itself — the loop does, through the facade.
//!
//! The form's own keys come from the keymap's `form` layer (rebindable, ADR 0024 Q7); the
//! approval and confirmation keys stay hardcoded. A `text` question and the `Other` row reuse
//! [`Composer`], so editing behaves exactly like the message box.

use radian_coding::{Question, QuestionAnswer, QuestionKind, QuestionReply};

use crate::composer::Composer;
use crate::keys::Action;

/// One question's in-progress answer.
#[derive(Debug, Default)]
pub struct FormAnswer {
    /// The highlighted row: an option index, or `options.len()` for the `Other` row.
    pub cursor: usize,
    /// The chosen option ids (`single`/`multi`).
    pub selected: Vec<String>,
    /// Whether the `Other` row is chosen (`single`/`multi` with `allow_other`).
    pub other: bool,
    /// The free text: a `text` answer, or the `Other` text.
    pub text: Composer,
}

/// The form for one request.
#[derive(Debug)]
pub struct QuestionForm {
    /// The queue's request id, which the answer names.
    pub request_id: u64,
    /// The session that asked (shown in the title, ADR 0024 Q2).
    pub origin: String,
    pub questions: Vec<Question>,
    pub answers: Vec<FormAnswer>,
    /// The question shown, or `None` for the overview.
    pub current: Option<usize>,
    /// The first `Esc` set this; the second declines (ADR 0024 Q7).
    pub confirm_decline: bool,
}

/// What one key produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormOutcome {
    /// The form handled the key and stays up.
    Stay,
    /// The overview's submit could not proceed: this required question has no answer.
    Blocked(String),
    /// Submit: the answers.
    Answer(QuestionAnswer),
    /// Decline (the second `Esc`).
    Decline,
}

/// One key inside the form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormKey {
    /// A `form`-layer action (rebindable), or a composer-editing action for the focused field.
    Action(Action),
    /// A printable character for the focused field.
    Char(char),
}

impl QuestionForm {
    pub fn new(request_id: u64, origin: String, questions: Vec<Question>) -> Self {
        let answers = questions.iter().map(|_| FormAnswer::default()).collect();
        Self {
            request_id,
            origin,
            questions,
            answers,
            current: Some(0),
            confirm_decline: false,
        }
    }

    /// How many questions the request has.
    pub fn len(&self) -> usize {
        self.questions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.questions.is_empty()
    }

    /// The question shown, or `None` for the overview.
    pub fn current_question(&self) -> Option<&Question> {
        self.current.and_then(|index| self.questions.get(index))
    }

    pub fn current_answer(&self) -> Option<&FormAnswer> {
        self.current.and_then(|index| self.answers.get(index))
    }

    /// The number of selectable rows of `question`: its options, plus the `Other` row when it
    /// allows one. A `text` question has none.
    pub fn row_count(question: &Question) -> usize {
        if question.kind == QuestionKind::Text {
            0
        } else {
            question.options.len() + usize::from(question.allow_other)
        }
    }

    /// Whether the current question's free-text field is focused: a `text` question, or the
    /// `Other` row of a `single`/`multi` question.
    pub fn text_focused(&self) -> bool {
        let Some(question) = self.current_question() else {
            return false;
        };
        let Some(answer) = self.current_answer() else {
            return false;
        };
        match question.kind {
            QuestionKind::Text => true,
            _ => question.allow_other && answer.cursor == question.options.len(),
        }
    }

    fn text_mut(&mut self) -> Option<&mut Composer> {
        if !self.text_focused() {
            return None;
        }
        let index = self.current?;
        self.answers.get_mut(index).map(|answer| &mut answer.text)
    }

    /// Whether `question` is answered by `answer`. A `single`/`multi` question counts an
    /// `Other` choice only once something was typed; a `text` question needs non-empty text.
    pub fn is_answered(question: &Question, answer: &FormAnswer) -> bool {
        match question.kind {
            QuestionKind::Text => !answer.text.is_empty(),
            _ => {
                !answer.selected.is_empty()
                    || (question.allow_other && answer.other && !answer.text.is_empty())
            }
        }
    }

    /// The first required question with no answer, if any (ADR 0024 Q7: it blocks submit).
    pub fn first_unanswered_required(&self) -> Option<&Question> {
        self.questions
            .iter()
            .zip(&self.answers)
            .find(|(question, answer)| question.required && !Self::is_answered(question, answer))
            .map(|(question, _)| question)
    }

    /// The answers, in the request's order (ADR 0024 Q3: one reply per question id).
    pub fn to_answer(&self) -> QuestionAnswer {
        let replies = self
            .questions
            .iter()
            .zip(&self.answers)
            .map(|(question, answer)| match question.kind {
                QuestionKind::Text => QuestionReply {
                    id: question.id.clone(),
                    text: Some(answer.text.text()).filter(|text| !text.is_empty()),
                    ..QuestionReply::default()
                },
                _ => QuestionReply {
                    id: question.id.clone(),
                    selected: answer.selected.clone(),
                    text: None,
                    other: (question.allow_other && answer.other)
                        .then(|| answer.text.text())
                        .filter(|text| !text.is_empty()),
                },
            })
            .collect();
        QuestionAnswer::new(format!("q-{}", self.request_id), replies)
    }

    /// Handles one key. The form stays up unless it submitted or declined.
    pub fn key(&mut self, key: FormKey) -> FormOutcome {
        match key {
            FormKey::Char(c) => {
                self.confirm_decline = false;
                if let Some(text) = self.text_mut() {
                    text.insert_char(c);
                }
            }
            FormKey::Action(action) => match action {
                Action::FormDecline => {
                    if self.confirm_decline {
                        return FormOutcome::Decline;
                    }
                    self.confirm_decline = true;
                }
                Action::FormNext => {
                    self.confirm_decline = false;
                    self.move_question(1);
                }
                Action::FormPrev => {
                    self.confirm_decline = false;
                    self.move_question(-1);
                }
                Action::FormUp => {
                    self.confirm_decline = false;
                    self.move_cursor(-1);
                }
                Action::FormDown => {
                    self.confirm_decline = false;
                    self.move_cursor(1);
                }
                Action::FormSelect => {
                    self.confirm_decline = false;
                    self.select();
                }
                Action::FormSubmit => {
                    self.confirm_decline = false;
                    return self.submit();
                }
                // The composer's editing actions, for the focused free-text field.
                other => {
                    self.confirm_decline = false;
                    if let Some(text) = self.text_mut() {
                        match other {
                            Action::CursorLeft => {
                                text.left();
                            }
                            Action::CursorRight => {
                                text.right();
                            }
                            Action::WordLeft => {
                                text.word_left();
                            }
                            Action::WordRight => {
                                text.word_right();
                            }
                            Action::LineStart => {
                                text.line_start();
                            }
                            Action::LineEnd => {
                                text.line_end();
                            }
                            Action::DeleteBefore => {
                                text.backspace();
                            }
                            Action::DeleteUnder => {
                                text.delete();
                            }
                            Action::DeleteWord => {
                                text.delete_word();
                            }
                            Action::DeleteToLineStart => {
                                text.kill_to_line_start();
                            }
                            Action::DeleteToLineEnd => {
                                text.kill_to_line_end();
                            }
                            Action::Newline => {
                                text.insert_char('\n');
                            }
                            _ => {}
                        }
                    }
                }
            },
        }
        FormOutcome::Stay
    }

    /// `Tab`/`Shift-Tab`: the next/previous question, wrapping.
    fn move_question(&mut self, delta: isize) {
        let count = self.questions.len();
        if count == 0 {
            return;
        }
        let current = self.current.map_or(0, |index| index as isize);
        let next = (current + delta).rem_euclid(count as isize) as usize;
        self.current = Some(next);
    }

    /// Up/Down: the option cursor of the current question (or the composer's own cursor in a
    /// free-text field).
    fn move_cursor(&mut self, delta: isize) {
        if self.text_focused() {
            if let Some(text) = self.text_mut() {
                if delta < 0 {
                    text.up();
                } else {
                    text.down();
                }
            }
            return;
        }
        let Some(index) = self.current else { return };
        let rows = Self::row_count(&self.questions[index]);
        if rows == 0 {
            return;
        }
        let answer = &mut self.answers[index];
        let next = (answer.cursor as isize + delta).clamp(0, rows as isize - 1) as usize;
        answer.cursor = next;
    }

    /// `Space`: choose the highlighted row.
    fn select(&mut self) {
        let Some(index) = self.current else { return };
        let question = &self.questions[index];
        if question.kind == QuestionKind::Text {
            return;
        }
        let option = question
            .options
            .get(self.answers[index].cursor)
            .map(|o| o.id.clone());
        let answer = &mut self.answers[index];
        match option {
            Some(id) => {
                if question.kind == QuestionKind::Multi {
                    if let Some(at) = answer.selected.iter().position(|chosen| *chosen == id) {
                        answer.selected.remove(at);
                    } else {
                        answer.selected.push(id);
                    }
                } else {
                    answer.selected = vec![id];
                    answer.other = false;
                }
            }
            // The `Other` row.
            None => {
                if question.kind == QuestionKind::Multi {
                    answer.other = !answer.other;
                } else {
                    answer.other = true;
                    answer.selected.clear();
                }
            }
        }
    }

    /// `Enter`: the next question, the overview from the last one, or the submit in the
    /// overview.
    fn submit(&mut self) -> FormOutcome {
        match self.current {
            Some(index) if index + 1 < self.questions.len() => {
                self.current = Some(index + 1);
                FormOutcome::Stay
            }
            Some(_) => {
                // The last question: go to the overview.
                self.current = None;
                FormOutcome::Stay
            }
            None => match self.first_unanswered_required() {
                Some(question) => FormOutcome::Blocked(question.id.clone()),
                None => FormOutcome::Answer(self.to_answer()),
            },
        }
    }
}
