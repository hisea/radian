//! ADR 0024 Q7: the question form. Pure `App` state, so these run without a terminal.
//!
//! Every test drives the form through `App::on_key` (the real key path: the `form` layer of
//! the keymap, then the composer), and checks the [`Effect`] the loop would run.

use radian_coding::{PromptError, Question, QuestionKind, QuestionOption};
use radian_host::{PendingRequest, PendingResponse, PendingView};
use radian_tui::app::{App, Effect, Input, Modal};
use radian_tui::keys::Keymap;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn option(id: &str, label: &str) -> QuestionOption {
    QuestionOption {
        id: id.into(),
        label: label.into(),
        description: None,
    }
}

fn question(
    id: &str,
    kind: QuestionKind,
    options: Vec<QuestionOption>,
    required: bool,
) -> Question {
    Question {
        id: id.into(),
        prompt: format!("question {id}?"),
        kind,
        options,
        allow_other: false,
        required,
    }
}

/// The queue's view of the request, and the app showing it (ADR 0023 U7).
fn show(app: &mut App, questions: Vec<Question>) {
    app.update(Input::Pending(
        Some(Box::new(PendingView {
            request_id: 7,
            origin: "session".into(),
            request: PendingRequest::Question(radian_coding::QuestionRequest::new(questions)),
        })),
        1,
    ));
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn tab() -> KeyEvent {
    KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)
}

fn form(app: &App) -> &radian_tui::question::QuestionForm {
    match app.layers().iter().rev().find_map(|layer| match layer {
        radian_tui::app::Layer::Modal(Modal::Question(form)) => Some(form.as_ref()),
        _ => None,
    }) {
        Some(form) => form,
        None => panic!("no question form is up"),
    }
}

fn effect(app: &mut App, event: KeyEvent) -> Option<Effect> {
    let result = app.on_key(event);
    assert!(
        result.effects.len() <= 1,
        "one key produces at most one effect: {:?}",
        result.effects
    );
    result.effects.into_iter().next()
}

/// The answer the effect carries, if it is a question answer.
fn answer_of(effect: Option<Effect>) -> Option<radian_coding::QuestionAnswer> {
    match effect {
        Some(Effect::AnswerRequest {
            request_id,
            response,
        }) => {
            assert_eq!(request_id, 7);
            match response {
                PendingResponse::Question(Ok(answer)) => Some(answer),
                other => panic!("expected an answer, got {other:?}"),
            }
        }
        _ => None,
    }
}

fn declined(effect: Option<Effect>) -> bool {
    matches!(
        effect,
        Some(Effect::AnswerRequest {
            response: PendingResponse::Question(Err(PromptError::Declined)),
            ..
        })
    )
}

/// `Enter` walks the questions and reaches the overview; the overview submits.
#[test]
fn enter_walks_to_the_overview_and_submits() {
    let mut app = App::new("m", "s");
    show(
        &mut app,
        vec![
            question(
                "one",
                QuestionKind::Single,
                vec![option("a", "A"), option("b", "B")],
                true,
            ),
            question("two", QuestionKind::Text, Vec::new(), true),
        ],
    );
    assert_eq!(form(&app).current, Some(0));
    // Answer the first: `Space` chooses the highlighted option.
    app.on_key(key(KeyCode::Char(' ')));
    // `Enter` moves to the second question (not the overview yet).
    assert_eq!(effect(&mut app, key(KeyCode::Enter)), None);
    assert_eq!(form(&app).current, Some(1));
    // Type the text answer.
    for c in "radian".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    // `Enter` on the last question goes to the overview, not submit.
    assert_eq!(effect(&mut app, key(KeyCode::Enter)), None);
    assert_eq!(form(&app).current, None, "the overview");
    // `Enter` in the overview submits.
    let answer = answer_of(effect(&mut app, key(KeyCode::Enter))).expect("a submit");
    assert_eq!(answer.request_id, "q-7");
    assert_eq!(answer.answers[0].id, "one");
    assert_eq!(answer.answers[0].selected, vec!["a"]);
    assert_eq!(answer.answers[1].text.as_deref(), Some("radian"));
}

/// ADR 0024 Q7: a required question with no answer cannot be submitted.
#[test]
fn a_required_question_blocks_the_submit() {
    let mut app = App::new("m", "s");
    show(
        &mut app,
        vec![
            question("one", QuestionKind::Single, vec![option("a", "A")], false),
            question("two", QuestionKind::Text, Vec::new(), true),
        ],
    );
    app.on_key(tab());
    // Go to the overview without answering the required text question.
    app.on_key(key(KeyCode::Enter));
    assert_eq!(form(&app).current, None);
    assert_eq!(
        effect(&mut app, key(KeyCode::Enter)),
        None,
        "a missing required answer must not submit"
    );
    assert!(
        app.entries
            .iter()
            .any(|entry| entry.text.contains("`two` is required")),
        "the block is explained: {:?}",
        app.entries.last().map(|entry| &entry.text)
    );
    // Answer it, and the submit works: `Tab` back to it, type, `Enter` to the overview,
    // `Enter` to submit.
    app.on_key(tab());
    app.on_key(key(KeyCode::Char('x')));
    app.on_key(key(KeyCode::Enter));
    assert_eq!(form(&app).current, None, "the overview again");
    assert!(answer_of(effect(&mut app, key(KeyCode::Enter))).is_some());
}

/// ADR 0024 Q7: the drafts are kept while moving between questions.
#[test]
fn drafts_are_kept_between_questions() {
    let mut app = App::new("m", "s");
    show(
        &mut app,
        vec![
            question("one", QuestionKind::Text, Vec::new(), false),
            question("two", QuestionKind::Text, Vec::new(), false),
        ],
    );
    for c in "first".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.on_key(tab());
    for c in "second".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    // Back to the first: its draft is still there.
    app.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
    assert_eq!(form(&app).answers[0].text.text(), "first");
    assert_eq!(form(&app).answers[1].text.text(), "second");
    // And the submit carries both.
    app.on_key(tab());
    app.on_key(key(KeyCode::Enter));
    let answer = answer_of(effect(&mut app, key(KeyCode::Enter))).expect("a submit");
    assert_eq!(answer.answers[0].text.as_deref(), Some("first"));
    assert_eq!(answer.answers[1].text.as_deref(), Some("second"));
}

/// ADR 0024 Q7: `Esc` asks once, then declines.
#[test]
fn esc_asks_once_before_declining() {
    let mut app = App::new("m", "s");
    show(
        &mut app,
        vec![question("one", QuestionKind::Text, Vec::new(), false)],
    );
    // The first Esc only asks.
    assert_eq!(effect(&mut app, key(KeyCode::Esc)), None);
    assert!(form(&app).confirm_decline);
    assert!(form(&app).current.is_some(), "still up");
    // Any other key clears the question.
    app.on_key(key(KeyCode::Char('x')));
    assert!(!form(&app).confirm_decline);
    // Esc twice declines, and the answer is a decline, not a fake empty answer.
    assert_eq!(effect(&mut app, key(KeyCode::Esc)), None);
    let decline = effect(&mut app, key(KeyCode::Esc));
    assert!(declined(decline), "the second Esc declines");
    assert!(
        app.layers()
            .iter()
            .all(|layer| !matches!(layer, radian_tui::app::Layer::Modal(Modal::Question(_)))),
        "the form goes with the decline"
    );
}

/// ADR 0024 Q7: the form keys come from the keymap's `form` layer and can be rebound; the
/// approval keys stay hardcoded.
#[test]
fn the_form_keys_are_rebindable() {
    let mut app = App::new("m", "s");
    let config = radian_host::config::parse(
        std::path::Path::new("/tmp/question.toml"),
        "schema_version = 1\n[tui.keybindings]\nform_next = [\"ctrl-n\"]\n",
    )
    .expect("a valid config");
    app.apply_config(&config, false).expect("a valid keymap");
    show(
        &mut app,
        vec![
            question("one", QuestionKind::Text, Vec::new(), false),
            question("two", QuestionKind::Text, Vec::new(), false),
        ],
    );
    // The default `Tab` no longer moves; the new key does.
    app.on_key(tab());
    assert_eq!(form(&app).current, Some(0), "Tab was rebound away");
    app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert_eq!(form(&app).current, Some(1));
}

/// U4 invariant 2: the approval modal's keys are hardcoded, so they still work when it sits
/// above a question form.
#[test]
fn the_approval_modal_above_a_question_keeps_its_keys() {
    let mut app = App::new("m", "s");
    show(
        &mut app,
        vec![question("one", QuestionKind::Text, Vec::new(), false)],
    );
    app.push_modal(Modal::Approval(Box::new(
        radian_tui::app::PendingApproval {
            request_id: 2,
            origin: "session".into(),
            request: radian_permissions::ApprovalRequest::new(
                "perm-2",
                "call-2",
                "write",
                "/ws/out.txt",
                "/ws/out.txt",
                serde_json::json!({}),
                None,
            ),
        },
    )));
    // `y` answers the approval, it does not type into the question's text field.
    let effect = effect(&mut app, key(KeyCode::Char('y')));
    assert!(
        matches!(
            effect,
            Some(Effect::AnswerRequest {
                request_id: 2,
                response: PendingResponse::Approval(_)
            })
        ),
        "{effect:?}"
    );
    assert!(form(&app).answers[0].text.is_empty(), "`y` did not type");
}

/// A `multi` question toggles its options, and `Other` is a separate choice with its own text.
#[test]
fn a_multi_question_toggles_and_other_is_its_own_answer() {
    let mut app = App::new("m", "s");
    let mut q = question(
        "tiers",
        QuestionKind::Multi,
        vec![option("safe", "Safe"), option("workspace", "Workspace")],
        true,
    );
    q.allow_other = true;
    show(&mut app, vec![q]);
    app.on_key(key(KeyCode::Char(' '))); // select `safe`
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Char(' '))); // select `workspace`
    app.on_key(key(KeyCode::Char(' '))); // toggle `workspace` off again
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Char(' '))); // choose `Other`
    for c in "custom".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.on_key(key(KeyCode::Enter));
    let answer = answer_of(effect(&mut app, key(KeyCode::Enter))).expect("a submit");
    assert_eq!(answer.answers[0].selected, vec!["safe"]);
    assert_eq!(answer.answers[0].other.as_deref(), Some("custom"));
}

/// The `text` question uses the composer: editing keys behave like the message box.
#[test]
fn a_text_question_uses_the_composer() {
    let mut app = App::new("m", "s");
    show(
        &mut app,
        vec![question("name", QuestionKind::Text, Vec::new(), true)],
    );
    for c in "radian".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert_eq!(form(&app).answers[0].text.text(), "radian");
    // The composer's own editing: left, delete, type.
    app.on_key(key(KeyCode::Left));
    app.on_key(key(KeyCode::Backspace));
    assert_eq!(form(&app).answers[0].text.text(), "radin");
    app.on_key(key(KeyCode::Char('a')));
    assert_eq!(form(&app).answers[0].text.text(), "radian");
}

/// `App::new` starts with no pending count, and the queue's length is what the status bar
/// shows (ADR 0023 U7).
#[test]
fn the_pending_count_follows_the_queue() {
    let mut app = App::new("m", "s");
    assert_eq!(app.pending, 0);
    show(
        &mut app,
        vec![question("one", QuestionKind::Text, Vec::new(), false)],
    );
    assert_eq!(app.pending, 1);
    app.update(Input::Pending(None, 0));
    assert_eq!(app.pending, 0);
    assert!(
        app.layers()
            .iter()
            .all(|layer| !matches!(layer, radian_tui::app::Layer::Modal(Modal::Question(_)))),
        "the form is gone with the head"
    );
}

/// The keymap's built-in form layer is the ADR's key table.
#[test]
fn the_builtin_form_keys_are_the_ads() {
    let map = Keymap::builtin();
    assert_eq!(
        map.form_action_for(&tab()),
        Some(radian_tui::keys::Action::FormNext)
    );
    assert_eq!(
        map.form_action_for(&key(KeyCode::Char(' '))),
        Some(radian_tui::keys::Action::FormSelect)
    );
    assert_eq!(
        map.form_action_for(&key(KeyCode::Esc)),
        Some(radian_tui::keys::Action::FormDecline)
    );
    assert_eq!(
        map.form_action_for(&key(KeyCode::Enter)),
        Some(radian_tui::keys::Action::FormSubmit)
    );
    // The layers are independent: the main layer's `Tab` is still `complete`, and its `Enter`
    // is still `send`.
    assert_eq!(
        map.action_for(&tab()),
        Some(radian_tui::keys::Action::Complete)
    );
    assert_eq!(
        map.action_for(&key(KeyCode::Enter)),
        Some(radian_tui::keys::Action::Send)
    );
}
