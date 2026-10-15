//! In-transcript search (Phase 5): the app-level key flow and the viewport jump.

use radian_permissions::ApprovalRequest;
use radian_tui::app::{App, Entry, EntryKind, Input};
use radian_tui::blocks::ReasoningMode;
use radian_tui::render;
use radian_tui::tools::DisplayModes;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn render(app: &mut App) {
    let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
}

/// The width `render` used, so the test can compute the same wrapped lines the finder does.
const WIDTH: usize = 40;

fn fill(app: &mut App, needle_entries: &[usize]) {
    for i in 0..40 {
        let text = if needle_entries.contains(&i) {
            format!("entry {i} has NEEDLE here")
        } else {
            format!("entry {i} is filler")
        };
        app.entries.push(Entry::new(EntryKind::Notice, text));
    }
}

/// The absolute transcript line `entry` starts at, computed independently of the finder.
fn entry_line(app: &App, entry: usize) -> usize {
    // The container's gap lines are part of the transcript (ADR 0028 rule 1), so
    // the geometry the finder uses includes them.
    let mut line = 0usize;
    for index in 0..=entry {
        let Some(item) = app.entries.get(index) else {
            break;
        };
        line += radian_tui::blocks::block_gap(app.entries.get(index.wrapping_sub(1)), item);
        if index < entry {
            line += render::display_lines(item, EXPANDED, WIDTH).len();
        }
    }
    line
}

#[test]
fn ctrl_f_opens_and_typing_jumps_to_the_nearest_match_above() {
    let mut app = App::new("m", "s");
    fill(&mut app, &[5, 25]);
    render(&mut app);
    app.input = "draft".into();

    app.on_key(ctrl('f'));
    let find = app.find_state().expect("the find box is open");
    assert_eq!(find.query, "");
    assert_eq!(find.status(), None, "an empty query has no status");

    for c in "needle".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    let find = app.find_state().unwrap();
    assert_eq!(find.query, "needle");
    assert_eq!(find.matches.len(), 2, "two matching entries");
    // The anchor was the bottom, so the nearest match above is the last one.
    assert_eq!(find.status().as_deref(), Some("match 2/2"));
    assert_eq!(find.current_line(), Some(entry_line(&app, 25)));
    assert!(!app.is_following(), "a jump closes follow");
    assert_eq!(app.input.text(), "draft", "the message draft is untouched");
    // The match line is centered: it is inside the viewport.
    assert!(
        app.scroll <= entry_line(&app, 25),
        "scroll {} is past the match",
        app.scroll
    );
}

#[test]
fn prev_walks_earlier_and_next_walks_later_without_wrapping() {
    let mut app = App::new("m", "s");
    fill(&mut app, &[5, 25]);
    render(&mut app);
    app.on_key(ctrl('f'));
    for c in "needle".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        app.find_state().unwrap().status().as_deref(),
        Some("match 2/2")
    );

    // Enter (and Ctrl-F again) is the earlier match.
    app.on_key(key(KeyCode::Enter));
    assert_eq!(
        app.find_state().unwrap().status().as_deref(),
        Some("match 1/2")
    );
    assert_eq!(
        app.find_state().unwrap().current_line(),
        Some(entry_line(&app, 5))
    );
    // At the first match, Enter does not wrap.
    app.on_key(key(KeyCode::Enter));
    assert_eq!(
        app.find_state().unwrap().status().as_deref(),
        Some("match 1/2")
    );

    // Ctrl-G (later) returns to the second; again does not wrap.
    app.on_key(ctrl('g'));
    assert_eq!(
        app.find_state().unwrap().status().as_deref(),
        Some("match 2/2")
    );
    app.on_key(ctrl('g'));
    assert_eq!(
        app.find_state().unwrap().status().as_deref(),
        Some("match 2/2")
    );
    // Shift-Enter is also "later".
    app.on_key(key(KeyCode::Enter));
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    assert_eq!(
        app.find_state().unwrap().status().as_deref(),
        Some("match 2/2")
    );
}

#[test]
fn escape_closes_and_leaves_the_viewport_where_it_is() {
    let mut app = App::new("m", "s");
    fill(&mut app, &[5, 25]);
    render(&mut app);
    app.input = "unchanged".into();
    app.on_key(ctrl('f'));
    for c in "needle".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    app.on_key(key(KeyCode::Enter)); // jump to the earlier match
    let scroll = app.scroll;
    app.on_key(key(KeyCode::Esc));
    assert!(app.find_state().is_none(), "Esc closes the box");
    assert_eq!(app.scroll, scroll, "the viewport stays where it was");
    assert_eq!(app.input.text(), "unchanged");
    // The next message key types into the input again.
    app.on_key(key(KeyCode::Char('x')));
    assert_eq!(app.input.text(), "unchangedx");
}

#[test]
fn a_query_with_no_match_says_no_match_and_does_not_move() {
    let mut app = App::new("m", "s");
    fill(&mut app, &[5, 25]);
    render(&mut app);
    let scroll = app.scroll;
    app.on_key(ctrl('f'));
    // No prefix of "zzz" matches anything, so no intermediate query moved the viewport.
    for c in "zzz".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        app.find_state().unwrap().status().as_deref(),
        Some("no match")
    );
    assert_eq!(app.scroll, scroll, "no match must not move the viewport");
    assert!(app.is_following(), "and follow is untouched");
}

#[test]
fn ctrl_f_is_refused_while_an_approval_is_pending() {
    let mut app = App::new("m", "s");
    fill(&mut app, &[5]);
    render(&mut app);
    app.update(Input::Pending(
        Some(Box::new(radian_host::PendingView {
            request_id: 1,
            origin: "session".into(),
            request: radian_host::PendingRequest::Approval(ApprovalRequest::new(
                "perm-1",
                "call-1",
                "write",
                "/ws/out.txt",
                "/ws/out.txt",
                json!({"path": "out.txt"}),
                None,
            )),
        })),
        1,
    ));
    app.on_key(ctrl('f'));
    assert!(app.find_state().is_none(), "no find box over an approval");
}

#[test]
fn backspace_edits_the_query_and_recomputes() {
    let mut app = App::new("m", "s");
    fill(&mut app, &[5, 25]);
    render(&mut app);
    app.on_key(ctrl('f'));
    for c in "needlex".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert_eq!(
        app.find_state().unwrap().status().as_deref(),
        Some("no match")
    );
    app.on_key(key(KeyCode::Backspace));
    assert_eq!(
        app.find_state().unwrap().status().as_deref(),
        Some("match 2/2")
    );
}

/// Correctness on a 50,000-entry transcript: the search finds the one match and reports it.
/// **No wall clock here** — timing is the ignored benchmark below (the repo's rule: every
/// timing assertion lives in an `#[ignore]`d benchmark, `perf_render.rs` and
/// `docs/perf-baseline.md` §7 explain why an absolute bound is load-sensitive).
#[test]
fn a_search_over_fifty_thousand_entries_finds_the_needle() {
    let mut app = fifty_thousand_entries();
    app.on_key(ctrl('f'));
    for c in "needle".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    let find = app.find_state().unwrap();
    assert_eq!(find.matches.len(), 1);
    assert_eq!(find.status().as_deref(), Some("match 1/1"));
}

/// A 50k transcript with the needle at entry 40,000.
fn fifty_thousand_entries() -> App {
    let mut app = App::new("m", "s");
    for i in 0..50_000 {
        let text = if i == 40_000 {
            "the one needle".to_string()
        } else {
            format!("filler line {i}")
        };
        app.entries.push(Entry::new(EntryKind::Notice, text));
    }
    render(&mut app);
    app
}

/// The performance benchmark (decision 6): one search over 50,000 entries is well under the
/// debug bound. **`#[ignore]`d**, like every timing assertion in this repo: the debug numbers
/// are load-sensitive (observed 2.59 s under load average 200+), so it is run deliberately
/// with `--ignored` (`docs/perf-baseline.md` §7). It also prints the 5k calibration, so a
/// pathological regression is visible in the ratio as well as the absolute bound.
#[test]
#[ignore = "wall-clock benchmark, not an assertion in the default run; --ignored --nocapture"]
fn a_search_over_fifty_thousand_entries_is_fast() {
    fn app_with(entries: usize, needle_at: usize) -> App {
        let mut app = App::new("m", "s");
        for i in 0..entries {
            let text = if i == needle_at {
                "the one needle".to_string()
            } else {
                format!("filler line {i}")
            };
            app.entries.push(Entry::new(EntryKind::Notice, text));
        }
        render(&mut app);
        app
    }
    fn timed_search(app: &mut App) -> std::time::Duration {
        let started = std::time::Instant::now();
        app.on_key(ctrl('f'));
        for c in "needle".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        started.elapsed()
    }

    let mut calibration = app_with(5_000, 4_000);
    let baseline = timed_search(&mut calibration);
    assert_eq!(calibration.find_state().unwrap().matches.len(), 1);

    let mut app = fifty_thousand_entries();
    let elapsed = timed_search(&mut app);
    assert_eq!(app.find_state().unwrap().matches.len(), 1);
    eprintln!(
        "find: 5k {baseline:?}, 50k {elapsed:?} (ratio {:.1}x)",
        elapsed.as_secs_f64() / baseline.as_secs_f64()
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "50k entries took {elapsed:?}, over the 2 s debug bound"
    );
}

/// The visible match is marked with `Modifier::REVERSED` at the right span (acceptance 3).
#[test]
fn the_matching_substring_is_reversed_in_the_rendered_line() {
    use ratatui::style::Modifier;
    use ratatui::text::{Line, Span};
    let line = Line::from(vec![Span::raw("> "), Span::raw("a NEEDLE b")]);
    let highlighted = render::highlight(line, "needle");
    let reversed: Vec<&str> = highlighted
        .spans
        .iter()
        .filter(|span| span.style.add_modifier.contains(Modifier::REVERSED))
        .map(|span| span.content.as_ref())
        .collect();
    assert_eq!(reversed, vec!["NEEDLE"], "{highlighted:?}");
    // A case-sensitive query with no match leaves the line as one span.
    let line = Line::from(vec![Span::raw("> "), Span::raw("a NEEDLE b")]);
    let unchanged = render::highlight(line, "absent");
    assert!(
        unchanged
            .spans
            .iter()
            .all(|span| !span.style.add_modifier.contains(Modifier::REVERSED))
    );
}

/// The display modes most tests lay out with: both folds expanded, so a test's geometry does
/// not depend on the fold defaults.
#[allow(dead_code)]
const EXPANDED: DisplayModes = DisplayModes {
    glyphs: radian_tui::footer::Glyphs::Unicode,
    reasoning: ReasoningMode::Expanded,
    tools: radian_tui::tools::ToolDisplay {
        fold: radian_tui::tools::ToolFold::Expanded,
        preview_lines: 5,
    },
    mermaid: true,
    highlight: true,
};

/// A folded tool body must not vanish from the search (ADR 0028 R4 / this slice): the match
/// geometry is the expanded form, whatever `tui.display.tool_output` says. (Slice 4 will
/// auto-expand on a hit; this slice only keeps the content findable.)
#[test]
fn a_folded_tool_body_is_still_found() {
    use agent_core::{ContentPart, Message, ToolCall, ToolResult};
    use radian_tui::tools::ToolFold;
    let mut app = App::new("m", "s");
    app.tool_display.fold = ToolFold::Collapsed;
    let body: String = (1..=20)
        .map(|i| format!("line {i}\n"))
        .chain(std::iter::once("the need1e marker\n".to_string()))
        .collect();
    app.push_message(&Message::Assistant {
        content: vec![],
        reasoning: vec![],
        tool_calls: vec![ToolCall {
            id: "c1".into(),
            name: "generic_tool".into(),
            arguments: json!({}),
        }],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    });
    app.push_message(&Message::Tool {
        result: ToolResult {
            call_id: "c1".into(),
            content: vec![ContentPart::text(body)],
            structured: None,
            is_error: false,
            error_code: None,
        },
        meta: Default::default(),
    });
    // The displayed body is folded to the preview: the marker is not among the laid-out
    // lines (`display_lines` is the display geometry the renderer uses).
    let shown = radian_tui::render::display_lines(&app.entries[1], app.display_modes(), 80);
    assert!(
        !shown.iter().any(|line| line.contains("need1e")),
        "folded out of the display: {shown:?}"
    );
    // ...and the search still finds it.
    app.on_key(ctrl('f'));
    for c in "need1e".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert_eq!(app.find_state().unwrap().matches.len(), 1);
}

/// ADR 0028 R6: the search matches the **full** content, and a hit in a folded entry opens
/// exactly that entry - the per-entry override - with a status-bar note. The viewport lands on
/// the match's display line (the folded geometry, not the search's expanded one).
#[test]
fn a_hit_in_a_folded_tool_body_expands_only_that_entry() {
    use agent_core::{ContentPart, Message, ToolCall, ToolResult};
    use radian_tui::tools::ToolFold;
    let mut app = App::new("m", "s");
    app.tool_display.fold = ToolFold::Collapsed;
    // Two tool cards, each with a body longer than the preview: only one hides the needle.
    for (call, body) in [
        ("c1", "alpha\nbeta\ngamma\nNEEDLE-HERE"),
        ("c2", "one\ntwo\nthree\nfour"),
    ] {
        app.push_message(&Message::Assistant {
            content: vec![],
            reasoning: vec![],
            tool_calls: vec![ToolCall {
                id: call.into(),
                name: "generic_tool".into(),
                arguments: json!({}),
            }],
            provider_payload: None,
            interrupted: false,
            meta: Default::default(),
        });
        app.push_message(&Message::Tool {
            result: ToolResult {
                call_id: call.into(),
                content: vec![ContentPart::text(body)],
                structured: None,
                is_error: false,
                error_code: None,
            },
            meta: Default::default(),
        });
    }
    render(&mut app);
    // Folded: neither body shows anything but its one-line hint.
    let folded_total = app.total_lines;
    app.on_key(ctrl('f'));
    for c in "needle".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    let match_line = app.find_state().unwrap().matches[0];
    assert_eq!(
        app.find_state().unwrap().matches.len(),
        1,
        "the folded body is searched"
    );
    // Exactly one entry opened, and the note says so.
    let overridden: Vec<usize> = app
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.fold_override == Some(true))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(overridden.len(), 1, "one entry at a time");
    assert_eq!(
        app.note.as_deref(),
        Some("expanded 1 folded entry for the match")
    );
    render(&mut app);
    assert_eq!(
        app.total_lines,
        folded_total + 3,
        "the hit entry's four body lines replace its one-line hint"
    );
    // The match line is on screen, centered: the jump mapped it through the display geometry.
    let display_line: usize = app.entries[..match_line.entry]
        .iter()
        .map(|entry| radian_tui::render::display_lines(entry, app.display_modes(), 40).len())
        .sum::<usize>()
        + match_line.line;
    assert!(
        display_line >= app.scroll && display_line < app.scroll + usize::from(app.view_height),
        "display line {display_line} is outside the viewport (scroll {})",
        app.scroll
    );
    // A global toggle drops the override.
    app.on_key(ctrl('o'));
    assert!(
        app.entries
            .iter()
            .all(|entry| entry.fold_override.is_none())
    );
}

/// A hit in a folded **thinking** block expands it too, and a hit in a folded compaction
/// summary does the same.
#[test]
fn a_hit_in_folded_thinking_and_a_summary_expands_them() {
    use agent_core::compaction::compacted_history;
    use agent_core::{ContentPart, Message};
    let mut app = App::new("m", "s");
    app.reasoning_mode = radian_tui::blocks::ReasoningMode::Collapsed;
    app.push_message(&Message::Assistant {
        content: vec![ContentPart::text("the answer")],
        // Three lines, so expanding it changes the transcript's line count.
        reasoning: vec![ContentPart::text(
            "a deep THOUGHT about it\nsecond line\nthird line",
        )],
        tool_calls: vec![],
        provider_payload: None,
        interrupted: false,
        meta: Default::default(),
    });
    let history = compacted_history("the compacted SUMMARY body", &[], &[]).expect("valid");
    app.push_message(&history[0]);
    render(&mut app);
    let folded = app.total_lines;
    // Search for the thinking text.
    app.on_key(ctrl('f'));
    for c in "thought".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert_eq!(app.find_state().unwrap().matches.len(), 1);
    let thinking = app
        .entries
        .iter()
        .position(|entry| entry.fold_override == Some(true))
        .expect("the thinking entry opened");
    render(&mut app);
    assert!(
        app.total_lines > folded,
        "the thinking text is laid out now"
    );
    // Now search for the summary's text: the override moves to that entry.
    for _ in 0.."thought".len() {
        app.on_key(key(KeyCode::Backspace));
    }
    for c in "summary".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    assert_eq!(app.find_state().unwrap().matches.len(), 1);
    let now: Vec<usize> = app
        .entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.fold_override == Some(true))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(now.len(), 1, "still exactly one");
    assert_ne!(now[0], thinking, "the override moved to the summary");
}
