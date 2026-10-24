//! The modal panels the frame draws in place of the input box: the `/login` wizard, the
//! project-trust prompt, the `/access full` and `/logout` confirmations, and the `ask_user`
//! question form.

use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::app::{AccessChoice, AccessWizard, TrustWizard};
use crate::blocks;
use crate::login::LoginWizard;
use crate::question::QuestionForm;
use crate::theme::Theme;

use super::wrap::{clip_columns, wrap_char_offsets};

/// The `/login` wizard's panel (ADR 0035): the shared [`crate::panel::Panel`] with
/// the step's rows, the provider/protocol choices through [`crate::select_list`], and the
/// step's key hints in the bottom border. Pure, so the layout is testable without a terminal.
/// **The key is never among the rows** (D3): a filled field renders one `*` per character.
pub fn login_panel(
    wizard: &LoginWizard,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Vec<Line<'static>> {
    use crate::login::LoginStep;
    let ascii = glyphs == crate::footer::Glyphs::Ascii;
    let inner = width.saturating_sub(4).max(1);
    let label = |text: &str| Line::from(Span::styled(text.to_owned(), theme.text));
    let value = |text: String| Line::from(Span::styled(text, theme.text));
    let mut panel = crate::panel::Panel::new("/login");
    match &wizard.step {
        LoginStep::ChooseProvider { cursor } => {
            panel = panel.row(label("choose a provider"));
            let items: Vec<crate::select_list::Item> = LoginWizard::choices()
                .into_iter()
                .map(|(id, description)| crate::select_list::Item::new(id).description(description))
                .collect();
            let list =
                crate::select_list::view(theme, &items, "", *cursor, items.len(), inner, ascii);
            for line in list.lines {
                panel = panel.row(line);
            }
            panel = panel
                .hint("↑/↓", "choose")
                .hint("⏎", "next")
                .hint("esc", "cancel");
        }
        LoginStep::CustomName => {
            panel = panel
                .row(label("provider name (also its auth.json entry):"))
                .row(value(wizard.name.clone()));
            panel = panel.hint("⏎", "next").hint("esc", "cancel");
        }
        LoginStep::CustomProtocol { cursor } => {
            panel = panel.row(label("wire protocol"));
            let items: Vec<crate::select_list::Item> = crate::login::PROTOCOLS
                .iter()
                .map(|kind| crate::select_list::Item::new(*kind))
                .collect();
            let list =
                crate::select_list::view(theme, &items, "", *cursor, items.len(), inner, ascii);
            for line in list.lines {
                panel = panel.row(line);
            }
            panel = panel
                .hint("↑/↓", "choose")
                .hint("⏎", "next")
                .hint("esc", "cancel");
        }
        LoginStep::CustomBaseUrl => {
            panel = panel
                .row(label("base URL:"))
                .row(value(wizard.base_url.clone()));
            panel = panel.hint("⏎", "next").hint("esc", "cancel");
        }
        LoginStep::KeyInput => {
            panel = panel
                .row(label("API key (hidden; paste works too):"))
                .row(value(LoginWizard::masked(&wizard.key)));
            panel = panel.hint("⏎", "next").hint("esc", "cancel");
        }
        LoginStep::ReplaceOrAdd { cursor } => {
            panel = panel.row(label("this provider already has a key"));
            let items: Vec<crate::select_list::Item> = ["replace the current key", "add another"]
                .into_iter()
                .map(crate::select_list::Item::new)
                .collect();
            let list =
                crate::select_list::view(theme, &items, "", *cursor, items.len(), inner, ascii);
            for line in list.lines {
                panel = panel.row(line);
            }
            panel = panel
                .hint("↑/↓", "choose")
                .hint("⏎", "next")
                .hint("r/a", "replace/add")
                .hint("esc", "cancel");
        }
        LoginStep::AddName => {
            panel = panel
                .row(label("name for the new key (empty allowed):"))
                .row(value(wizard.add_name.clone()));
            panel = panel.hint("⏎", "save").hint("esc", "cancel");
        }
        LoginStep::Verifying => {
            panel = panel.row(label(&format!("verifying {} …", wizard.provider_name())));
            panel = panel.hint("esc", "cancel");
        }
        LoginStep::VerifyFailed { reason } => {
            panel = panel
                .row(Line::from(Span::styled(
                    format!("verification failed: {reason}"),
                    theme.turn_cancelled,
                )))
                .row(label("save anyway?"));
            panel = panel.hint("y", "save").hint("n/esc", "cancel");
        }
        LoginStep::OAuthBrowserWaiting { url } => {
            panel = panel.row(label("signing in with ChatGPT…"));
            match url {
                Some(url) => {
                    panel = panel
                        .row(label("opening your browser; if it did not open, visit:"))
                        .row(value(url.clone()));
                }
                None => panel = panel.row(label("starting the local sign-in listener…")),
            }
            panel = panel.hint("esc", "cancel");
        }
        LoginStep::OAuthDeviceWaiting {
            user_code,
            verification_url,
        } => {
            panel = panel.row(label("signing in with ChatGPT (device code)…"));
            if user_code.is_empty() {
                panel = panel.row(label("requesting a code…"));
            } else {
                panel = panel
                    .row(label(&format!("go to {verification_url} and enter:")))
                    .row(Line::from(Span::styled(user_code.clone(), theme.text)));
            }
            panel = panel.hint("esc", "cancel");
        }
    }
    if let Some(error) = &wizard.error {
        panel = panel.row(Line::from(Span::styled(
            error.clone(),
            theme.turn_cancelled,
        )));
    }
    panel.lines(width, theme, glyphs)
}

/// The `/login` panel's height, measured from the panel itself so the frame and the layout
/// agree.
pub(super) fn login_height(
    wizard: &LoginWizard,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> u16 {
    login_panel(wizard, width, theme, glyphs).len() as u16
}

/// The startup project-trust panel (ADR 0031 T3, ADR 0035): the shared panel with
/// the workspace line, the project `.mcp.json` preview (slice B2: one line per server, env
/// key names only, never values), the choices through [`crate::select_list`], and the
/// hardcoded key hints. The cursor's default is `TrustHereSandbox` (decision
/// 2026-10-20: for a project with no trusted decision the sandbox choice comes first and
/// is pre-selected).
pub fn trust_panel(
    wizard: &TrustWizard,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Vec<Line<'static>> {
    let ascii = glyphs == crate::footer::Glyphs::Ascii;
    let inner = width.saturating_sub(4).max(1);
    let items: Vec<crate::select_list::Item> = wizard
        .choices
        .iter()
        .map(|choice| crate::select_list::Item::new(wizard.label(*choice)))
        .collect();
    let list =
        crate::select_list::view(theme, &items, "", wizard.cursor, items.len(), inner, ascii);
    let mut panel = crate::panel::Panel::new("project trust").row(Line::from(Span::styled(
        format!("project resources found in {}", wizard.workspace.display()),
        theme.text,
    )));
    if let Some(preview) = &wizard.mcp {
        for line in radian_host::preview_lines(preview) {
            panel = panel.row(Line::from(Span::styled(line, theme.hint)));
        }
    }
    if let Some(preview) = &wizard.hooks {
        for line in radian_host::preview_project_hooks_lines(preview) {
            panel = panel.row(Line::from(Span::styled(line, theme.hint)));
        }
    }
    for line in list.lines {
        panel = panel.row(line);
    }
    panel = panel
        .hint("↑/↓", "choose")
        .hint("⏎", "select")
        .hint("esc", "do not trust");
    panel.lines(width, theme, glyphs)
}

/// The trust panel's height, measured from the panel itself.
pub(super) fn trust_height(
    wizard: &TrustWizard,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> u16 {
    trust_panel(wizard, width, theme, glyphs).len() as u16
}

/// The `/access full` confirmation (ADR 0038 slice 2, ADR 0035): the shared panel
/// with two warning lines — no sandbox, and no prompts — the three choices through
/// [`crate::select_list`], and the hardcoded key hints. The default cursor is `cancel`, so
/// Enter does not open the sandbox.
pub fn access_panel(
    wizard: &AccessWizard,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Vec<Line<'static>> {
    let ascii = glyphs == crate::footer::Glyphs::Ascii;
    let inner = width.saturating_sub(4).max(1);
    let items: Vec<crate::select_list::Item> = AccessChoice::ALL
        .iter()
        .map(|choice| crate::select_list::Item::new(choice.label()))
        .collect();
    let list =
        crate::select_list::view(theme, &items, "", wizard.cursor, items.len(), inner, ascii);
    let mut panel = crate::panel::Panel::new("/access full")
        .row(Line::from(Span::styled(
            "full means no sandbox: commands run with your full access",
            theme.text,
        )))
        .row(Line::from(Span::styled(
            "full shows no prompts: allowed calls run without asking",
            theme.text,
        )));
    for line in list.lines {
        panel = panel.row(line);
    }
    panel = panel
        .hint("↑/↓", "choose")
        .hint("⏎", "select")
        .hint("esc", "cancel");
    panel.lines(width, theme, glyphs)
}

/// The `/access full` panel's height, measured from the panel itself.
pub(super) fn access_height(
    wizard: &AccessWizard,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> u16 {
    access_panel(wizard, width, theme, glyphs).len() as u16
}

/// The `/logout` confirmation (ADR 0035): the shared panel with the provider and
/// the hardcoded `y`/`n` hints in the bottom border.
pub fn logout_panel(
    provider: &str,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Vec<Line<'static>> {
    crate::panel::Panel::new("/logout")
        .row(Line::from(Span::styled(
            format!("remove the stored credential for `{provider}`?"),
            theme.text,
        )))
        .hint("[y]", "remove")
        .hint("[n/esc]", "keep")
        .lines(width, theme, glyphs)
}

/// The `/logout` key picker (ADR 0026 D3): one row per key (`1 first (sk-…irst)`), an
/// `all keys` row, and the hardcoded hints.
pub fn logout_keys_panel(
    wizard: &crate::logout::LogoutKeysWizard,
    width: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Vec<Line<'static>> {
    let ascii = glyphs == crate::footer::Glyphs::Ascii;
    let inner = width.saturating_sub(4).max(1);
    let rows = wizard.rows();
    let items: Vec<crate::select_list::Item> = rows
        .into_iter()
        .map(crate::select_list::Item::new)
        .collect();
    let list =
        crate::select_list::view(theme, &items, "", wizard.cursor, items.len(), inner, ascii);
    let mut panel = crate::panel::Panel::new("/logout").row(Line::from(Span::styled(
        format!("`{}` has several keys; remove which one?", wizard.provider),
        theme.text,
    )));
    for line in list.lines {
        panel = panel.row(line);
    }
    panel
        .hint("↑/↓", "choose")
        .hint("⏎", "remove")
        .hint("a", "all")
        .hint("esc", "keep")
        .lines(width, theme, glyphs)
}

/// The most rows a question form's box may take, border included (ADR 0024 Q7: a long option
/// list scrolls instead of growing the box without bound).
const MAX_QUESTION_ROWS: u16 = 18;

/// The question form's key line, drawn on its own row under the content so a long option list
/// can never push it out of the box (same fix as the approval box's `APPROVAL_KEYS`).
pub const QUESTION_KEYS: &str = "Tab next · Space select · Enter next/submit · Esc decline";

/// The height the question form's box needs: its content plus the key line, bounded.
pub(super) fn question_height(
    form: &QuestionForm,
    own_session: &str,
    width: usize,
    max_rows: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> u16 {
    let rows = question_panel(form, own_session, width, max_rows, theme, glyphs).len();
    (rows as u16).clamp(5, MAX_QUESTION_ROWS)
}

/// The `ask_user` panel (ADR 0024 Q7): the shared [`crate::panel::Panel`] with the
/// tab row, the current question's options (or its text field), the `Submit` list, and the
/// key hints in the bottom border. Pure, so the layout is testable without a terminal.
pub fn question_panel(
    form: &QuestionForm,
    own_session: &str,
    width: usize,
    max_rows: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Vec<Line<'static>> {
    // `alt-h`: one line, so the transcript is readable.
    if form.hidden {
        let answered = form
            .questions
            .iter()
            .zip(&form.answers)
            .filter(|(question, answer)| QuestionForm::is_answered(question, answer))
            .count();
        return vec![Line::from(vec![
            Span::styled("Ask", theme.notice_label),
            Span::styled(
                format!(" · {answered}/{} answered · ", form.len()),
                theme.notice_body,
            ),
            Span::styled("alt-h", theme.hint),
            Span::styled(" to show", theme.notice_body),
        ])];
    }
    let inner = width.saturating_sub(4).max(1);
    // ADR 0024 rule 4: `Ask` alone when the asker is this process's own session
    // (the ordinary single-session TUI); the origin only distinguishes several sessions. A
    // TUI process has one session today (the per-session view), so origin == own session is the
    // clean test.
    let title = if form.origin.is_empty() || form.origin == own_session {
        "Ask".to_string()
    } else {
        format!("Ask · {}", form.origin)
    };
    let mut panel = crate::panel::Panel::new(title)
        .row(tab_row(form, inner, theme))
        .divider();
    match form.current_question() {
        Some(question) => {
            let answer = form
                .current_answer()
                .expect("a current question has an answer");
            for line in wrap_char_offsets(&question.prompt, inner) {
                let text: String = line
                    .chars
                    .into_iter()
                    .filter_map(|i| {
                        question
                            .prompt
                            .get(i..)
                            .and_then(|rest| rest.chars().next())
                    })
                    .collect();
                panel = panel.row(Line::from(Span::styled(text, theme.notice_label)));
            }
            panel = panel.divider();
            if question.kind == radian_coding::QuestionKind::Text {
                panel = panel.row(composer_line(&answer.text, theme));
            } else {
                // The option rows (a description is its own row), then a window that keeps the
                // cursor visible and the bottom border (the hints) on screen.
                let mut rows: Vec<(Line<'static>, bool)> = Vec::new();
                for (row, option) in question.options.iter().enumerate() {
                    rows.push((
                        option_line(question, answer, row, theme, glyphs),
                        row == answer.cursor,
                    ));
                    if let Some(description) = &option.description {
                        let prefix = match glyphs {
                            crate::footer::Glyphs::Ascii => "->",
                            _ => "↳",
                        };
                        rows.push((
                            Line::from(Span::styled(
                                clip_columns(&format!("   {prefix} {description}"), inner),
                                theme.notice_body,
                            )),
                            false,
                        ));
                    }
                }
                if question.allow_other {
                    rows.push((
                        other_line(question, answer, theme, glyphs),
                        answer.cursor == question.options.len(),
                    ));
                }
                // The fixed rows: the top border, the tab row, two dividers, the prompt, the
                // note (when open), the decline row, and the bottom border.
                let note_rows = if form.note_focused() { 3 } else { 0 };
                let fixed = 1
                    + 1
                    + 1
                    + question.prompt.lines().count().max(1)
                    + 1
                    + note_rows
                    + usize::from(form.confirm_decline)
                    + 1
                    + 1;
                let budget = max_rows.saturating_sub(fixed).max(1);
                let cursor = rows.iter().position(|(_, cursor)| *cursor).unwrap_or(0);
                let first = cursor
                    .saturating_sub(budget.saturating_sub(1))
                    .min(rows.len().saturating_sub(budget));
                if first > 0 {
                    panel = panel.row(Line::from(Span::styled(
                        format!("  … {first} above"),
                        theme.hint,
                    )));
                }
                let last = (first + budget).min(rows.len());
                for (line, _) in rows.drain(first..last) {
                    panel = panel.row(line);
                }
                if rows.len() > last {
                    panel = panel.row(Line::from(Span::styled(
                        format!("  … {} more", rows.len() - last),
                        theme.hint,
                    )));
                }
            }
            if form.note_focused() {
                panel = panel.divider();
                panel = panel.row(Line::from(Span::styled("note", theme.hint)));
                panel = panel.row(composer_line(&answer.note, theme));
            }
        }
        // The `Submit` tab: one line per question, the header and the answer summary.
        None => {
            for (index, question) in form.questions.iter().enumerate() {
                let answer = &form.answers[index];
                let answered = QuestionForm::is_answered(question, answer);
                let summary = if answered {
                    answer_text(question, answer)
                } else {
                    "(not answered)".to_string()
                };
                let style = if !answered && question.required {
                    theme.tool_result_error
                } else {
                    theme.notice_body
                };
                let mark = if answered { "✓" } else { "·" };
                panel = panel.row(Line::from(Span::styled(
                    clip_columns(&format!("{mark} {} — {summary}", question.header()), inner),
                    style,
                )));
            }
        }
    }
    // The first Esc asks again, so it never looks like it did nothing (ADR 0024 Q7).
    if form.confirm_decline {
        panel = panel.row(Line::from(Span::styled(
            clip_columns(
                "press Esc again to decline this question (Enter keeps it)",
                inner,
            ),
            theme.note,
        )));
    }
    for hint in form.hints() {
        panel = panel.hint(hint.keys, hint.description);
    }
    panel.lines(width, theme, glyphs)
}

/// The tab row (ADR 0024 Q7): one tab per question plus `Submit`, the current one
/// reversed. Each label is capped at [`crate::question::MAX_TAB_WIDTH`] and the row is clipped
/// (revision 2: no horizontal scrolling).
fn tab_row(form: &QuestionForm, width: usize, theme: &Theme) -> Line<'static> {
    let current = form.current.unwrap_or(form.questions.len());
    let mut spans = Vec::new();
    let mut used = 0;
    for (index, (label, _)) in form.tab_labels().iter().enumerate() {
        let text = format!(" {label} ");
        let text_width = UnicodeWidthStr::width(text.as_str());
        if used + text_width > width {
            break;
        }
        used += text_width;
        let style = if index == current {
            theme.notice_label.add_modifier(Modifier::REVERSED)
        } else {
            theme.notice_body
        };
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// One option row: the cursor, the mark and the label.
fn option_line(
    question: &radian_coding::Question,
    answer: &crate::question::FormAnswer,
    row: usize,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Line<'static> {
    let option = &question.options[row];
    let cursor = if row == answer.cursor { "❯ " } else { "  " };
    let chosen = answer.selected.contains(&option.id);
    // ADR 0024 implementation note: one marker source (`AskGlyphs`), the same
    // checkbox pair for single and multi.
    let g = blocks::AskGlyphs::for_glyphs(glyphs);
    let mark = match (question.kind, chosen) {
        (radian_coding::QuestionKind::Multi, true) => g.multi_on,
        (radian_coding::QuestionKind::Multi, false) => g.multi_off,
        (_, true) => g.single_on,
        (_, false) => g.single_off,
    };
    let recommended = if option.recommended {
        " (Recommended)"
    } else {
        ""
    };
    // The cursor and the marker are drawn in their own spans and are never bold: the font's
    // bold face can lack the glyph and fall back to a different size (the report). The
    // marker keeps its label's role (the chosen accent / the weak body), just not the weight.
    let label_style = if chosen {
        theme.notice_label
    } else {
        theme.notice_body
    };
    let marker_style = label_style.remove_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled(cursor, theme.notice_label.remove_modifier(Modifier::BOLD)),
        Span::styled(mark, marker_style),
        Span::styled(format!(" {}{recommended}", option.label), label_style),
    ])
}

/// The `Other` row: `Other (type your own)` before it is chosen, `Other: <text>` once it is. A
/// focused, chosen row draws a visible inline cursor (`Space` on `Other`
/// starts typing at once), at the composer's cursor so `←`/`→` move it.
fn other_line(
    question: &radian_coding::Question,
    answer: &crate::question::FormAnswer,
    theme: &Theme,
    glyphs: crate::footer::Glyphs,
) -> Line<'static> {
    let focused = answer.cursor == question.options.len();
    let cursor = if focused { "❯ " } else { "  " };
    // ADR 0024 implementation note: `Other` uses the **same** checkbox pair as the
    // options (it used to always draw a circle, even in a multi question), from `AskGlyphs`.
    let g = blocks::AskGlyphs::for_glyphs(glyphs);
    let mark = if answer.other {
        g.single_on
    } else {
        g.single_off
    };
    // The marker and the cursor are never bold, like `option_line`.
    let mut spans = vec![
        Span::styled(cursor, theme.notice_label.remove_modifier(Modifier::BOLD)),
        Span::styled(mark, theme.notice_body),
    ];
    if answer.other && focused {
        let text = answer.text.text();
        let at = answer.text.cursor();
        let before: String = text.chars().take(at).collect();
        let after: String = text.chars().skip(at).collect();
        spans.push(Span::styled(" Other: ".to_string(), theme.notice_body));
        spans.push(Span::styled(before, theme.notice_body));
        spans.push(Span::styled("▏", theme.hint));
        spans.push(Span::styled(after, theme.notice_body));
    } else {
        let text = if answer.other && !answer.text.is_empty() {
            format!("Other: {}", answer.text.text())
        } else {
            "Other (type your own)".to_string()
        };
        spans.push(Span::styled(format!(" {text}"), theme.notice_body));
    }
    Line::from(spans)
}

/// A free-text field (`text`, `Other`, a note): `> ` and the text.
fn composer_line(composer: &crate::composer::Composer, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled("> ", theme.notice_label),
        Span::styled(composer.text(), theme.text),
    ])
}

/// One answer as readable text, using option labels.
fn answer_text(question: &radian_coding::Question, answer: &crate::question::FormAnswer) -> String {
    let mut parts: Vec<String> = answer
        .selected
        .iter()
        .map(|id| {
            question
                .options
                .iter()
                .find(|option| &option.id == id)
                .map(|option| option.label.clone())
                .unwrap_or_else(|| id.clone())
        })
        .collect();
    if !answer.text.is_empty() {
        parts.push(answer.text.text());
    }
    if parts.is_empty() {
        "(not answered)".to_string()
    } else {
        parts.join(", ")
    }
}
