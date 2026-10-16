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
        recommended: false,
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
        header: None,
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
        app.session
            .entries
            .iter()
            .any(|entry| entry.text.contains("`two` is required")),
        "the block is explained: {:?}",
        app.session.entries.last().map(|entry| &entry.text)
    );
    // Answer it, and the submit works: `Tab` back to it, type, then `Enter` through the tabs
    // to `Submit` (revision 1: each tab is a question, the last is `Submit`).
    app.on_key(tab()); // Submit -> question 0
    app.on_key(tab()); // question 0 -> question 1 (the required text)
    app.on_key(key(KeyCode::Char('x')));
    app.on_key(key(KeyCode::Enter)); // -> Submit
    assert_eq!(form(&app).current, None, "the Submit tab");
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
    // A re-sync of the **same** head keeps the form, so a redraw never loses a draft
    // (ADR 0023 U7: the modal is driven by the head, not by every notification).
    let head = PendingView {
        request_id: 7,
        origin: "session".into(),
        request: PendingRequest::Question(radian_coding::QuestionRequest::new(Vec::new())),
    };
    app.update(Input::Pending(Some(Box::new(head)), 1));
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
            scroll: 0,
            total_rows: 0,
            visible_rows: 0,
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
    // `Tab` to the `Submit` tab, then `Enter` (revision 1: `Enter` on a `multi` toggles).
    app.on_key(tab());
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

// ---- ADR 0024 R2/R3: the panel, the tabs, the hints and the new keys ----

use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// The whole screen as text.
fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| radian_tui::render::draw(frame, app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    let mut out = String::new();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            out.push_str(buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        out.push('\n');
    }
    out
}

/// The panel: a titled box, a tab row (`header` + `Submit`), the options with `❯`/`○`/`◉`
/// (ADR 0028), a `↳` description, and the context-dependent key hints in
/// the bottom border.
#[test]
fn the_panel_shows_the_tabs_the_options_and_the_hints() {
    let mut app = App::new("m", "s");
    let mut q = question(
        "pick",
        QuestionKind::Single,
        vec![option("a", "A"), option("b", "B")],
        false,
    );
    q.header = Some("Pick".to_string());
    q.options[0].recommended = true;
    q.options[1].description = Some("the second".to_string());
    q.allow_other = true;
    show(&mut app, vec![q]);
    let text = render(&mut app, 80, 24);
    assert!(text.contains("Ask"), "{text}");
    assert!(text.contains("Pick"), "{text}");
    assert!(text.contains("Submit"), "{text}");
    assert!(text.contains("❯ ○ A (Recommended)"), "{text}");
    assert!(text.contains("↳ the second"), "{text}");
    assert!(text.contains("Other (type your own)"), "{text}");
    assert!(
        text.contains("space select"),
        "the only select key: {text}"
    );
    assert!(text.contains("⏎ next"), "the confirm key: {text}");
    assert!(text.contains("alt-h"), "{text}");

    // A multi question changes the hints.
    let mut multi = App::new("m", "s");
    let mut q = question(
        "tiers",
        QuestionKind::Multi,
        vec![option("safe", "Safe")],
        false,
    );
    q.header = Some("Tiers".to_string());
    show(&mut multi, vec![q]);
    let text = render(&mut multi, 80, 16);
    assert!(text.contains("space select"), "{text}");
    assert!(text.contains("○ Safe"), "{text}");
}

/// `n` opens a note for the current question; the note reaches the answer (R3/R4).
#[test]
fn n_opens_a_note_and_the_note_reaches_the_answer() {
    let mut app = App::new("m", "s");
    show(
        &mut app,
        vec![question(
            "pick",
            QuestionKind::Single,
            vec![option("a", "A")],
            false,
        )],
    );
    app.on_key(key(KeyCode::Char('n')));
    assert!(form(&app).note_focused(), "`n` opens the note");
    for c in "because".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.on_key(key(KeyCode::Enter)); // save the note
    app.on_key(key(KeyCode::Char(' '))); // select A
    app.on_key(tab()); // -> Submit
    let answer = answer_of(effect(&mut app, key(KeyCode::Enter))).expect("a submit");
    assert_eq!(answer.answers[0].note.as_deref(), Some("because"));
}

/// `alt-h` collapses the panel to one line and shows it again (R3).
#[test]
fn alt_h_hides_and_shows_the_panel() {
    let mut app = App::new("m", "s");
    show(
        &mut app,
        vec![question(
            "pick",
            QuestionKind::Single,
            vec![option("a", "A")],
            false,
        )],
    );
    let alt_h = KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT);
    app.on_key(alt_h);
    assert!(form(&app).hidden, "`alt-h` hides");
    let text = render(&mut app, 80, 24);
    assert!(text.contains("0/1 answered"), "{text}");
    assert!(text.contains("alt-h to show"), "{text}");
    app.on_key(alt_h);
    assert!(!form(&app).hidden, "`alt-h` shows again");
}

/// A long tab label is truncated to 16 display cells (revision 2: no scrolling).
#[test]
fn a_long_tab_label_is_truncated() {
    let mut app = App::new("m", "s");
    let mut q = question("pick", QuestionKind::Single, vec![option("a", "A")], false);
    q.header = Some("一二三四五六七八九十".to_string());
    show(&mut app, vec![q]);
    let labels = form(&app).tab_labels();
    assert_eq!(labels[0].0, "一二三四五六七…", "{labels:?}");
    assert!(labels[1].0 == "Submit", "{labels:?}");
}

/// feedback 2026-10-16: with the cursor on `Other`, Up/Down still move between the
/// options and the typed text is kept; typing goes into `Other`, and `Enter` confirms.
#[test]
fn other_keeps_its_text_while_moving_with_arrows() {
    let mut app = App::new("m", "s");
    let mut q = question(
        "pick",
        QuestionKind::Single,
        vec![option("a", "A"), option("b", "B")],
        false,
    );
    q.allow_other = true;
    show(&mut app, vec![q]);
    // Move to the `Other` row (A, B, Other).
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Down));
    assert_eq!(form(&app).answers[0].cursor, 2, "the Other row");
    for c in "custom".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert_eq!(form(&app).answers[0].text.text(), "custom");

    // Up moves to the previous option; Down comes back to `Other`; the text survives.
    app.on_key(key(KeyCode::Up));
    assert_eq!(form(&app).answers[0].cursor, 1, "the previous option");
    app.on_key(key(KeyCode::Down));
    assert_eq!(form(&app).answers[0].cursor, 2, "back on Other");
    assert_eq!(
        form(&app).answers[0].text.text(),
        "custom",
        "the typed text is kept"
    );

    // `Enter` confirms the Other choice and advances; the answer carries the text.
    app.on_key(key(KeyCode::Enter));
    let answer = answer_of(effect(&mut app, key(KeyCode::Enter))).expect("a submit");
    assert_eq!(answer.answers[0].other.as_deref(), Some("custom"));
}

/// `Enter` only confirms and advances, it never selects; `Space` selects
/// without advancing. A question with nothing chosen still moves on.
#[test]
fn enter_advances_without_selecting_and_space_selects_without_advancing() {
    let questions = || {
        vec![
            question(
                "one",
                QuestionKind::Single,
                vec![option("a", "A"), option("b", "B")],
                true,
            ),
            question("two", QuestionKind::Text, Vec::new(), true),
        ]
    };
    // `Enter` on the single question advances to the next question and changes nothing.
    let mut app = App::new("m", "s");
    show(&mut app, questions());
    app.on_key(key(KeyCode::Down)); // B
    assert_eq!(effect(&mut app, key(KeyCode::Enter)), None);
    assert_eq!(
        form(&app).answers[0].selected,
        Vec::<String>::new(),
        "Enter must not select"
    );
    assert_eq!(form(&app).current, Some(1), "Enter advances");

    // `Space` selects the highlighted option and stays on the question.
    let mut spaced = App::new("m", "s");
    show(&mut spaced, questions());
    spaced.on_key(key(KeyCode::Down)); // B
    spaced.on_key(key(KeyCode::Char(' '))); // select B, stay
    assert_eq!(form(&spaced).answers[0].selected, vec!["b"]);
    assert_eq!(form(&spaced).current, Some(0), "Space does not advance");
}

/// on a `multi` question `Space` toggles and `Enter` only advances (it does
/// **not** toggle).
#[test]
fn a_multi_question_toggles_with_space_and_enter_only_advances() {
    let questions = || {
        vec![
            question(
                "tiers",
                QuestionKind::Multi,
                vec![option("safe", "Safe"), option("workspace", "Workspace")],
                true,
            ),
            question("two", QuestionKind::Text, Vec::new(), true),
        ]
    };
    let mut app = App::new("m", "s");
    show(&mut app, questions());
    app.on_key(key(KeyCode::Char(' '))); // toggle `safe` on
    assert_eq!(form(&app).answers[0].selected, vec!["safe"]);
    assert_eq!(form(&app).current, Some(0), "Space does not advance");
    assert_eq!(effect(&mut app, key(KeyCode::Enter)), None);
    assert_eq!(
        form(&app).answers[0].selected,
        vec!["safe"],
        "Enter must not toggle"
    );
    assert_eq!(form(&app).current, Some(1), "Enter advances");
}

/// `Space` on the `Other` row selects it and starts typing at once — the
/// cursor is visible on that row, the next keys land in its text, a following `Space` is a
/// space (the field takes the space), and switching to another option and back keeps the text.
#[test]
fn space_on_other_starts_typing_and_the_cursor_is_visible() {
    let mut q = question(
        "pick",
        QuestionKind::Single,
        vec![option("a", "A"), option("b", "B")],
        true,
    );
    q.allow_other = true;
    let mut app = App::new("m", "s");
    show(&mut app, vec![q]);
    // Move onto the `Other` row (A, B, Other); before `Space` it is not chosen.
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Down));
    assert_eq!(form(&app).answers[0].cursor, 2, "the Other row");
    assert!(!form(&app).answers[0].other, "not chosen yet");
    // `Space` chooses it and shows the inline cursor on that row.
    app.on_key(key(KeyCode::Char(' ')));
    assert!(form(&app).answers[0].other, "Space chooses Other");
    let text = render(&mut app, 80, 24);
    assert!(
        text.contains("Other: ▏"),
        "the text cursor is visible on the Other row: {text}"
    );
    // Typing lands in the Other text, and a `Space` is a space now (the field takes it).
    for c in "hi".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.on_key(key(KeyCode::Char(' ')));
    assert_eq!(form(&app).answers[0].text.text(), "hi ");

    // `Space` on another option switches the single answer and keeps the typed text; `Space`
    // back on `Other` chooses it again with the text intact and the cursor at its end.
    app.on_key(key(KeyCode::Up)); // to option B
    app.on_key(key(KeyCode::Char(' '))); // choose option B
    assert_eq!(form(&app).answers[0].selected, vec!["b"]);
    assert!(!form(&app).answers[0].other, "Other is un-chosen");
    assert_eq!(form(&app).answers[0].text.text(), "hi ", "the text is kept");
    app.on_key(key(KeyCode::Down)); // back to Other
    app.on_key(key(KeyCode::Char(' '))); // choose Other again
    assert!(form(&app).answers[0].other);
    assert_eq!(form(&app).answers[0].text.text(), "hi ");
    assert_eq!(
        form(&app).answers[0].text.cursor(),
        form(&app).answers[0].text.len(),
        "the cursor is at the end of the text"
    );
}

/// the inline `Other` cursor sits at the composer's cursor, so `←`/`→` move
/// it within the typed text.
#[test]
fn the_other_cursor_follows_the_text_cursor() {
    let mut q = question("pick", QuestionKind::Single, vec![option("a", "A")], true);
    q.allow_other = true;
    let mut app = App::new("m", "s");
    show(&mut app, vec![q]);
    app.on_key(key(KeyCode::Down)); // the Other row
    app.on_key(key(KeyCode::Char(' '))); // choose Other
    for c in "abc".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.on_key(key(KeyCode::Left)); // move the composer cursor before `c`
    let text = render(&mut app, 80, 24);
    assert!(
        text.contains("Other: ab▏c"),
        "the cursor follows ←/→: {text}"
    );
}

/// `Enter` with nothing chosen still advances; the question counts as
/// unanswered and the overview marks it.
#[test]
fn an_unanswered_question_is_marked_in_the_overview() {
    let mut app = App::new("m", "s");
    show(
        &mut app,
        vec![
            question("one", QuestionKind::Single, vec![option("a", "A")], false),
            question("two", QuestionKind::Single, vec![option("b", "B")], false),
        ],
    );
    // `Enter` on the first question with nothing chosen advances to the second.
    assert_eq!(effect(&mut app, key(KeyCode::Enter)), None);
    assert_eq!(form(&app).current, Some(1));
    assert!(form(&app).answers[0].selected.is_empty());
    // `Enter` again reaches the overview; both are unanswered and marked `· (not answered)`.
    assert_eq!(effect(&mut app, key(KeyCode::Enter)), None);
    assert_eq!(form(&app).current, None);
    let text = render(&mut app, 80, 24);
    assert!(
        text.contains("· one — (not answered)"),
        "the unanswered question is marked: {text}"
    );
    assert!(text.contains("· two — (not answered)"), "{text}");
}

/// A tall question panel shrinks the transcript; when it closes the transcript must be back at
/// the bottom (a live reply is visible), not stuck where the small viewport left it.
#[test]
fn the_transcript_follows_after_the_panel_closes() {
    use agent_core::Message;
    let mut app = App::new("m", "s");
    for i in 0..40 {
        app.push_message(&Message::user(format!("line {i}")));
    }
    show(
        &mut app,
        vec![question("q", QuestionKind::Text, Vec::new(), false)],
    );
    // The panel is up: the transcript viewport is small.
    let _ = render(&mut app, 80, 24);
    app.on_key(key(KeyCode::Char('x')));
    app.on_key(key(KeyCode::Enter)); // -> Submit
    let _ = effect(&mut app, key(KeyCode::Enter)); // submit, the panel closes
    let _ = render(&mut app, 80, 40);
    assert!(
        app.is_following(),
        "the transcript must follow after the panel closes (scroll={} max={})",
        app.session.scroll,
        app.session.max_scroll
    );
}

// ---------- feedback: Other typing and question switching (ADR 0024) ----------

/// The `Other` row is a free-text field: moving to it with Down and typing keeps the text and
/// chooses `Other`. Also checked: a `text` question keeps its typed text.
#[test]
fn typing_in_the_other_row_and_a_text_question_keeps_the_text() {
    let mut app = App::new("m", "s");
    let mut with_other = question(
        "one",
        QuestionKind::Single,
        vec![option("a", "A"), option("b", "B")],
        true,
    );
    with_other.allow_other = true;
    show(
        &mut app,
        vec![
            with_other,
            question("two", QuestionKind::Text, Vec::new(), true),
        ],
    );
    // Move past the two options onto the `Other` row.
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Down));
    assert_eq!(form(&app).answers[0].cursor, 2, "the Other row");
    for c in "a b c".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert!(
        form(&app).answers[0].other,
        "typing in the Other row chooses it"
    );
    assert_eq!(form(&app).answers[0].text.text(), "a b c");

    // The text question keeps its text, spaces included.
    app.on_key(tab());
    assert_eq!(form(&app).current, Some(1));
    for c in "two words".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert_eq!(form(&app).answers[1].text.text(), "two words");
}

/// The hint row says `⇥/←/→ question`: Left/Right switch questions while no text field has
/// focus, and Shift-Tab still walks back.
#[test]
fn left_and_right_switch_questions_without_a_focused_field() {
    let mut app = App::new("m", "s");
    show(
        &mut app,
        vec![
            question("one", QuestionKind::Single, vec![option("a", "A")], true),
            question("two", QuestionKind::Single, vec![option("b", "B")], true),
        ],
    );
    assert_eq!(form(&app).current, Some(0));
    app.on_key(key(KeyCode::Right));
    assert_eq!(form(&app).current, Some(1), "Right moves forward");
    app.on_key(key(KeyCode::Left));
    assert_eq!(form(&app).current, Some(0), "Left moves back");
    app.on_key(key(KeyCode::Right));
    assert_eq!(form(&app).current, Some(1));
    app.on_key(key(KeyCode::BackTab));
    assert_eq!(form(&app).current, Some(0), "Shift-Tab still walks back");
}

/// While the `Other` row or a `text` question has focus, Left/Right move the text cursor and do
/// **not** change the question.
#[test]
fn left_and_right_move_the_text_cursor_while_a_field_has_focus() {
    let mut text = question("one", QuestionKind::Text, Vec::new(), true);
    text.prompt = "Type".into();
    let mut app = App::new("m", "s");
    let mut with_other = question("one", QuestionKind::Single, vec![option("a", "A")], true);
    with_other.allow_other = true;
    show(&mut app, vec![with_other, text]);
    // The Other row (index 1, past one option): type, then Left moves the cursor back.
    app.on_key(key(KeyCode::Down));
    assert_eq!(form(&app).answers[0].cursor, 1, "the Other row");
    for c in "abc".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.on_key(key(KeyCode::Left));
    assert_eq!(
        form(&app).current,
        Some(0),
        "Left on the Other row keeps the question"
    );
    app.on_key(key(KeyCode::Char('Z')));
    assert_eq!(
        form(&app).answers[0].text.text(),
        "abZc",
        "Left moved the Other cursor, so the insert lands before c"
    );

    // The text question: Left/Right edit the text, never the tab.
    app.on_key(tab());
    assert_eq!(form(&app).current, Some(1));
    for c in "xyz".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.on_key(key(KeyCode::Left));
    app.on_key(key(KeyCode::Char('Q')));
    assert_eq!(form(&app).answers[1].text.text(), "xyQz");
    assert_eq!(form(&app).current, Some(1), "still the text question");
}

/// While a text field has focus the hint row shows the cursor keys, not the question arrows.
#[test]
fn the_hint_row_shows_the_cursor_keys_while_a_field_has_focus() {
    let mut app = App::new("m", "s");
    let mut with_other = question("one", QuestionKind::Single, vec![option("a", "A")], true);
    with_other.allow_other = true;
    show(&mut app, vec![with_other]);
    // On an option: the question arrows.
    assert!(
        form(&app)
            .hints()
            .iter()
            .any(|hint| hint.keys == "⇥/←/→" && hint.description == "question"),
        "an option row advertises the question arrows"
    );
    app.on_key(key(KeyCode::Down)); // the Other row
    assert!(
        form(&app).hints().iter().any(|hint| hint.keys == "⇥/⇤"),
        "the Other row advertises the cursor keys and Shift-Tab, not ←/→"
    );
}
