//! The structured-question form (ADR 0024 Q7 and revision 1): the state of one `ask_user`
//! request, as a [`Modal`](crate::app::Modal) over the transcript.
//!
//! Pure, like [`App`](crate::app::App): it turns keys into state changes and, at the end, one
//! [`QuestionAnswer`]. It never answers the queue itself — the loop does, through the facade.
//!
//! Revision 1's shape: the questions are **tabs** (the last tab is `Submit`), the current
//! question shows its options (or a text field), and the panel's bottom border carries the key
//! hints (R2). The form's own keys come from the keymap's `form` layer (rebindable, ADR 0024
//! Q7); `n` (a note) and `alt-h` (hide) are the form's own additions. A `text` question, the
//! `Other` row and a note reuse [`Composer`], so editing behaves exactly like the message box.

use radian_coding::{Question, QuestionAnswer, QuestionKind, QuestionReply};

use crate::composer::Composer;
use crate::keys::Action;
use crate::panel::Hint;

/// The tab label's display width cap (ADR 0024 R4, revision 2: no scrolling).
pub const MAX_TAB_WIDTH: usize = 16;

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
    /// The optional note (ADR 0024 R4).
    pub note: Composer,
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
    /// The shown tab: a question index, or `None` for the `Submit` tab.
    pub current: Option<usize>,
    /// The first `Esc` set this; the second declines (ADR 0024 Q7).
    pub confirm_decline: bool,
    /// `alt-h`: the panel is collapsed to one line so the transcript is readable (R3).
    pub hidden: bool,
    /// The note being edited, when `n` opened one (R3).
    pub editing_note: Option<usize>,
}

/// What one key produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormOutcome {
    /// The form handled the key and stays up.
    Stay,
    /// The submit could not proceed: this required question has no answer.
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
    /// A printable character for the focused field (or the form's own `n`).
    Char(char),
    /// `alt-h`: collapse or show the panel (R3).
    Hide,
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
            hidden: false,
            editing_note: None,
        }
    }

    /// How many questions the request has.
    pub fn len(&self) -> usize {
        self.questions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.questions.is_empty()
    }

    /// The question shown, or `None` for the `Submit` tab.
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

    /// Whether the current question's note field is being edited (`n`).
    pub fn note_focused(&self) -> bool {
        self.editing_note.is_some() && self.editing_note == self.current
    }

    /// Whether the focused field takes a literal space. A `text` question, a note, and an
    /// `Other` row that is **already** chosen do; an `Other` row that is not chosen yet keeps
    /// `space` as the select key (R3), so the first press still chooses it.
    fn field_takes_space(&self) -> bool {
        if self.note_focused() {
            return true;
        }
        let Some(question) = self.current_question() else {
            return false;
        };
        if question.kind == QuestionKind::Text {
            return true;
        }
        self.text_focused() && self.current_answer().is_some_and(|answer| answer.other)
    }

    fn note_mut(&mut self) -> Option<&mut Composer> {
        if !self.note_focused() {
            return None;
        }
        let index = self.editing_note?;
        self.answers.get_mut(index).map(|answer| &mut answer.note)
    }

    /// The focused composer: the note while editing one, else the `Other`/`text` field.
    fn composer_mut(&mut self) -> Option<&mut Composer> {
        if self.note_focused() {
            self.note_mut()
        } else {
            self.text_mut()
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
                    note: note_of(answer),
                    ..QuestionReply::default()
                },
                _ => QuestionReply {
                    id: question.id.clone(),
                    selected: answer.selected.clone(),
                    text: None,
                    other: (question.allow_other && answer.other)
                        .then(|| answer.text.text())
                        .filter(|text| !text.is_empty()),
                    note: note_of(answer),
                },
            })
            .collect();
        QuestionAnswer::new(format!("q-{}", self.request_id), replies)
    }

    /// The tab labels: each question's `header` truncated to [`MAX_TAB_WIDTH`], a completion
    /// mark on an answered one, then `Submit` (ADR 0024 R2, revision 2: no
    /// scrolling — each label is capped and the row is clipped).
    pub fn tab_labels(&self) -> Vec<(String, bool)> {
        let mut tabs: Vec<(String, bool)> = self
            .questions
            .iter()
            .zip(&self.answers)
            .map(|(question, answer)| {
                let answered = Self::is_answered(question, answer);
                let mark = if answered { "✓ " } else { "" };
                (
                    crate::render::clip_display(
                        &format!("{mark}{}", question.header()),
                        MAX_TAB_WIDTH,
                    ),
                    answered,
                )
            })
            .collect();
        tabs.push(("Submit".to_string(), false));
        tabs
    }

    /// The bottom-border key hints, which change with the current tab (R2/R3:
    /// `space select · ⏎ next`). `Space` is the only select key, so it is advertised on a row it
    /// actually selects; a free-text field (`text`, a chosen `Other`, a note) takes the space as
    /// text and only advertises `⏎ next` (the note's `⏎` saves it).
    pub fn hints(&self) -> Vec<Hint> {
        let mut hints = Vec::new();
        if self.note_focused() {
            hints.push(Hint::new("⏎", "save"));
        } else if self.current_question().is_some() {
            if self.field_takes_space() {
                // A `text` question or a chosen `Other` row: the space is text, `⏎` advances.
                hints.push(Hint::new("⏎", "next"));
            } else {
                hints.push(Hint::new("space", "select"));
                hints.push(Hint::new("⏎", "next"));
            }
        } else {
            hints.push(Hint::new("⏎", "submit"));
        }
        hints.push(Hint::new("↑/↓", "move"));
        // With a free-text field focused, ←/→ move the text cursor, so the question switch is
        // `⇥`/`⇤` alone (the hint advertised ←/→ which did not switch).
        if self.note_focused() || self.text_focused() {
            hints.push(Hint::new("⇥/⇤", "question"));
            hints.push(Hint::new("←/→", "cursor"));
        } else {
            hints.push(Hint::new("⇥/←/→", "question"));
        }
        hints.push(Hint::new("esc", "cancel"));
        hints.push(Hint::new("alt-h", "hide"));
        hints
    }

    /// Handles one key. The form stays up unless it submitted or declined.
    pub fn key(&mut self, key: FormKey) -> FormOutcome {
        match key {
            FormKey::Hide => {
                self.hidden = !self.hidden;
            }
            FormKey::Char(c) => {
                self.confirm_decline = false;
                if let Some(note) = self.note_mut() {
                    note.insert_char(c);
                } else if self.text_focused() {
                    // Typing into the `Other` row chooses it, so the typed text survives a move
                    // away and back and counts as the answer.
                    if let Some(index) = self.current
                        && self.questions[index].kind != QuestionKind::Text
                    {
                        self.answers[index].other = true;
                    }
                    if let Some(text) = self.text_mut() {
                        text.insert_char(c);
                    }
                } else if c == 'n' {
                    // `n` opens a note for the current question (R3).
                    self.editing_note = self.current;
                }
            }
            FormKey::Action(action) => return self.action(action),
        }
        FormOutcome::Stay
    }

    fn action(&mut self, action: Action) -> FormOutcome {
        match action {
            Action::FormDecline => {
                // Esc cancels a note edit first; otherwise it confirms a decline.
                if self.editing_note.is_some() {
                    self.editing_note = None;
                    self.confirm_decline = false;
                } else if self.confirm_decline {
                    return FormOutcome::Decline;
                } else {
                    self.confirm_decline = true;
                }
            }
            Action::FormNext => {
                self.confirm_decline = false;
                self.move_tab(1);
            }
            Action::FormPrev => {
                self.confirm_decline = false;
                self.move_tab(-1);
            }
            Action::FormUp => {
                self.confirm_decline = false;
                self.move_cursor(-1);
            }
            Action::FormDown => {
                self.confirm_decline = false;
                self.move_cursor(1);
            }
            // `Space`: a `single` question selects without advancing; a `multi` toggles (R3).
            // In a free-text field that already holds the answer (a `text` question anywhere,
            // an `Other` row that is chosen, a note) the space is **text**, not the select key:
            // the form layer's `space -> form_select` binding otherwise swallowed it, so a
            // multi-word `Other`/text answer could not be typed.
            Action::FormSelect => {
                self.confirm_decline = false;
                if self.field_takes_space() {
                    if let Some(text) = self.composer_mut() {
                        text.insert_char(' ');
                    }
                } else {
                    self.select();
                }
            }
            // `Enter`: save a note, otherwise **only** confirm and advance to the next question
            // (the last question advances to `Submit`; on `Submit` it submits). It never selects
            // a question with nothing chosen still moves on and counts as
            // unanswered, marked in the overview. Reaching `Submit` still blocks a missing
            // required answer.
            Action::FormSubmit => {
                self.confirm_decline = false;
                if self.editing_note.is_some() {
                    self.editing_note = None;
                    return FormOutcome::Stay;
                }
                match self.current_question() {
                    Some(_) => self.move_tab(1),
                    None => return self.submit(),
                }
            }
            // The composer's editing actions, for the focused free-text field.
            other => {
                self.confirm_decline = false;
                let composer = self.composer_mut();
                if let Some(text) = composer {
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
        }
        FormOutcome::Stay
    }

    /// `Tab`/`Shift-Tab`: the next/previous tab, wrapping through `Submit` (R3).
    fn move_tab(&mut self, delta: isize) {
        // The tabs are `0..len` then `Submit` (`None`).
        let count = self.questions.len() + 1;
        let current = self.current.unwrap_or(self.questions.len());
        let next = (current as isize + delta).rem_euclid(count as isize) as usize;
        self.editing_note = None;
        self.current = (next < self.questions.len()).then_some(next);
    }

    /// Up/Down: the option cursor of the current question, or the composer's own cursor in a
    /// **multi-line** field.
    ///
    /// The `Other` row is single-line: with the cursor on it (even while typing), Up/Down move
    /// between the options and the typed text is kept. A `text`
    /// question and a note are multi-line, so there Up/Down move the composer's cursor.
    fn move_cursor(&mut self, delta: isize) {
        let multiline_field = self.note_focused()
            || self
                .current_question()
                .is_some_and(|question| question.kind == QuestionKind::Text);
        if multiline_field {
            if let Some(text) = self.composer_mut() {
                if delta < 0 {
                    text.up();
                } else {
                    text.down();
                }
            }
            return;
        }
        let Some(index) = self.current else { return };
        let question = &self.questions[index];
        let rows = Self::row_count(question);
        if rows == 0 {
            return;
        }
        let answer = &mut self.answers[index];
        let next = (answer.cursor as isize + delta).clamp(0, rows as isize - 1) as usize;
        answer.cursor = next;
    }

    /// `Space`: choose the highlighted row (a `multi` toggles, a `single` selects). It never
    /// advances. Choosing `Other` moves the text cursor to the end so the
    /// next keys type into it at once.
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
            // The `Other` row: choose it and put the text cursor at the end of its text.
            None => {
                if question.kind == QuestionKind::Multi {
                    answer.other = !answer.other;
                } else {
                    answer.other = true;
                    answer.selected.clear();
                }
                if answer.other {
                    answer.text.to_end();
                }
            }
        }
    }

    /// The `Submit` tab: answer, or the first unanswered required question.
    fn submit(&mut self) -> FormOutcome {
        match self.first_unanswered_required() {
            Some(question) => FormOutcome::Blocked(question.id.clone()),
            None => FormOutcome::Answer(self.to_answer()),
        }
    }
}

/// A non-empty note, or `None`.
fn note_of(answer: &FormAnswer) -> Option<String> {
    let text = answer.note.text();
    (!text.is_empty()).then_some(text)
}
