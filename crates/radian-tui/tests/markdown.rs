//! Assistant-text Markdown (ADR 0028 R2): blocks, inline roles, tables, the streaming case,
//! selection copy and search. `TestBackend` frames, so colors and modifiers are asserted on
//! real cells.

use radian_tui::app::{App, EntryKind};
use radian_tui::blocks::{self, ReasoningMode};
use radian_tui::render;
use radian_tui::select;
use radian_tui::tools::{DisplayModes, ToolDisplay, ToolFold};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn modes(reasoning: ReasoningMode) -> DisplayModes {
    DisplayModes {
        reasoning,
        tools: ToolDisplay {
            fold: ToolFold::Expanded,
            preview_lines: 5,
        },
        mermaid: true,
        highlight: true,
        glyphs: radian_tui::footer::Glyphs::Unicode,
    }
}

fn assistant(app: &mut App, text: &str) {
    app.entries
        .push(radian_tui::app::Entry::new(EntryKind::Assistant, text));
}

fn render(app: &mut App, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
}

fn screen(terminal: &Terminal<TestBackend>) -> String {
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

fn style_of(terminal: &Terminal<TestBackend>, needle: &str) -> Option<ratatui::style::Style> {
    let buffer = terminal.backend().buffer();
    let area = *buffer.area();
    let mut line = String::new();
    for y in area.top()..area.bottom() {
        line.clear();
        for x in area.left()..area.right() {
            line.push_str(buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        if let Some(col) = line.find(needle) {
            let x = area.left() + col as u16;
            return buffer.cell((x, y)).map(|c| c.style());
        }
    }
    None
}

/// Every block kind and inline role reaches the screen with its own styling.
#[test]
fn the_block_kinds_and_inline_roles_render() {
    let mut app = App::new("m", "s");
    assistant(
        &mut app,
        "# Title\n\nA **bold** *italic* `code` [link](https://x.example) word.\n\n- one\n  - nested\n- two\n\n> quoted\n\n---\n",
    );
    let terminal = render(&mut app, 60, 24);
    let text = screen(&terminal);
    assert!(text.contains("Title"), "{text}");
    assert!(
        text.contains("bold") && !text.contains("**bold**"),
        "{text}"
    );
    assert!(text.contains("italic"), "{text}");
    assert!(text.contains("code"), "{text}");
    assert!(text.contains("link (https://x.example)"), "{text}");
    assert!(text.contains("• one"), "{text}");
    assert!(text.contains("  • nested"), "nested indent: {text}");
    assert!(text.contains("│ quoted"), "{text}");
    assert!(text.contains('─'), "the rule: {text}");
    // The roles' colors come from the theme.
    let heading = style_of(&terminal, "Title").expect("heading");
    let link = style_of(&terminal, "link").expect("link");
    let code = style_of(&terminal, "code").expect("code");
    let bullet = style_of(&terminal, "•").expect("bullet");
    assert_eq!(heading.fg, app.theme.md_heading.fg);
    assert_eq!(link.fg, app.theme.md_link.fg);
    assert_eq!(code.fg, app.theme.md_code.fg);
    assert_eq!(bullet.fg, app.theme.md_list_bullet.fg);
    assert_ne!(heading.fg, link.fg, "roles differ");
}

/// A table draws borders when it fits, wraps a cell that does not, and falls back to the raw
/// `|` text when there are too many columns.
#[test]
fn tables_wrap_and_fall_back() {
    let mut app = App::new("m", "s");
    assistant(
        &mut app,
        "| one | two |\n| --- | --- |\n| a very long cell that must wrap | b |\n",
    );
    let text = screen(&render(&mut app, 30, 16));
    assert!(
        text.contains('┌') && text.contains('│'),
        "bordered table: {text}"
    );
    assert!(text.contains("one"), "{text}");
    // Too many columns for the width: the raw `|` source comes back.
    let mut app = App::new("m", "s");
    assistant(
        &mut app,
        "| a | b | c | d | e | f | g | h |\n| --- | --- | --- | --- | --- | --- | --- | --- |\n| 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 |\n",
    );
    let text = screen(&render(&mut app, 20, 16));
    assert!(
        text.contains("| a | b |"),
        "the fallback keeps the raw source: {text}"
    );
    assert!(!text.contains('┌'), "no partial table: {text}");
}

/// A half-typed list and a half table still show something sane; they fix up when closed.
#[test]
fn a_streaming_half_parses() {
    let mut app = App::new("m", "s");
    assistant(&mut app, "- one\n- tw");
    let text = screen(&render(&mut app, 40, 12));
    assert!(text.contains("• one") && text.contains("• tw"), "{text}");
    let mut app = App::new("m", "s");
    assistant(&mut app, "| a | b |\n| --- | --- |\n| 1 |");
    let text = screen(&render(&mut app, 40, 12));
    assert!(text.contains('┌') || text.contains("| a |"), "{text}");
}

/// A selection across bold and a link copies the **source** it covers (ADR 0028 R2).
#[test]
fn a_selection_copies_the_source_it_covers() {
    let mut app = App::new("m", "s");
    assistant(&mut app, "A **bold** [link](https://x) tail\n");
    app.set_transcript_width(60);
    // Row 0 is the paragraph: cells 2..=9 are `bold** [l` (the display text is
    // `A bold link (https://x) tail`).
    let anchor =
        select::locate(&app.entries, modes(ReasoningMode::Expanded), 60, 0, 2).expect("anchor");
    let head =
        select::locate(&app.entries, modes(ReasoningMode::Expanded), 60, 0, 9).expect("head");
    app.selection = Some(select::Selection { anchor, head });
    let text = radian_tui::select::logical_text(&app.entries, &app.selection.expect("a selection"));
    assert!(
        text.contains("**bold**"),
        "the copy includes the markup: {text:?}"
    );
}

/// Ctrl-F matches the displayed text (not the markup).
#[test]
fn find_matches_the_displayed_text() {
    let mut app = App::new("m", "s");
    assistant(&mut app, "A **bold** and a [link](https://x).\n");
    let (matches, _) =
        radian_tui::find::compute(&app.entries, modes(ReasoningMode::Expanded), 60, "bold");
    assert_eq!(matches.len(), 1, "the displayed word matches");
    let (matches, _) =
        radian_tui::find::compute(&app.entries, modes(ReasoningMode::Expanded), 60, "**bold**");
    assert!(matches.is_empty(), "the markup is not displayed");
}

/// `line_count` equals the drawn body lines at two widths (virtualization).
#[test]
fn line_count_matches_the_layout() {
    let mut app = App::new("m", "s");
    assistant(
        &mut app,
        "# Title\n\nA paragraph that is long enough to wrap at a narrow width for sure.\n\n- one\n- two\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n",
    );
    let entry = &app.entries[0];
    let blocks = blocks::parse_blocks(entry.text.as_str());
    for width in [60usize, 20] {
        let count = blocks::line_count(entry, modes(ReasoningMode::Expanded), width);
        let laid_out = blocks::layout(entry, &blocks, modes(ReasoningMode::Expanded), width).len();
        assert_eq!(count, laid_out, "width {width}");
    }
}

/// Locate the cell of the `n`-th character of the **displayed** body (across the wrapped
/// lines), as `(line, column)`.
fn cell_of(lines: &[radian_tui::blocks::BodyLine], n: usize) -> Option<(usize, usize)> {
    let mut seen = 0usize;
    for (line_index, line) in lines.iter().enumerate() {
        let cells = line.chars.len();
        if n < seen + cells {
            return Some((line_index, n - seen));
        }
        seen += cells;
    }
    None
}

/// The property (review R2): the selection mapping points at the **source** characters. Every
/// display cell that came from the source (`md` piece with an offset) locates to exactly that
/// source offset, at every block kind and width; a synthesized cell (a bullet, a link's
/// ` (url)`) locates to its block's start and is skipped here.
#[test]
fn every_cell_maps_to_its_source_offset() {
    let bodies = [
        "alpha beta gamma delta epsilon\n",
        "A **bold** *italic* `code` [link](https://x) tail\n",
        "first paragraph\n\nsecond paragraph\n",
        "- item one\n- item two\n\n> quoted\n",
        "| a | b |\n| --- | --- |\n| 1 | 2 |\n",
        "# Heading\n\nbody text\n",
    ];
    for body in bodies {
        for width in [80usize, 30, 16] {
            let mut app = App::new("m", "s");
            assistant(&mut app, body);
            let modes = modes(ReasoningMode::Expanded);
            let blocks = blocks::parse_blocks(body);
            let lines = blocks::layout(&app.entries[0], &blocks, modes, width);
            let cells: usize = lines.iter().map(|line| line.chars.len()).sum();
            for cell in 0..cells {
                let (line_index, column) = cell_of(&lines, cell).expect("a cell");
                // `layout` is body-relative; `locate` takes screen columns (the assistant
                // prefix is two cells).
                let hit = select::locate_hit(&app.entries, modes, width, line_index, column + 2)
                    .expect("an anchor");
                match lines[line_index].chars[column] {
                    Some(offset) => {
                        assert_eq!(
                            hit.anchor.offset, offset,
                            "body {body:?} width {width} cell {cell}: a source cell anchors at \
                             its source offset"
                        );
                        assert!(
                            hit.on_char,
                            "body {body:?} cell {cell}: a source cell is a char"
                        );
                    }
                    None => assert!(
                        !hit.on_char,
                        "body {body:?} width {width} cell {cell}: a synthesized cell is not a \
                         source char"
                    ),
                }
            }
        }
    }
}

/// The soft-wrap case, spelled out: selecting across the break copies the source's space.
#[test]
fn a_selection_across_a_soft_wrap_keeps_the_source_space() {
    let body = "alpha beta gamma delta epsilon\n";
    let mut app = App::new("m", "s");
    assistant(&mut app, body);
    app.set_transcript_width(20);
    let modes = modes(ReasoningMode::Expanded);
    let blocks = blocks::parse_blocks(body);
    let lines = blocks::layout(&app.entries[0], &blocks, modes, 20);
    assert!(lines.len() > 1, "the paragraph wraps: {lines:?}");
    // From 'b' in "beta" to 'g' in "gamma" (display positions 6..11).
    let (sl, sc) = cell_of(&lines, 6).expect("start cell");
    let (el, ec) = cell_of(&lines, 11).expect("end cell");
    let anchor = select::locate(&app.entries, modes, 20, sl, sc + 2).expect("anchor");
    // The head is the cell after 'g': the copy then covers `beta gamma`.
    let head = select::locate(&app.entries, modes, 20, el, ec + 1 + 2).expect("head");
    assert_eq!(anchor.offset, 6, "'b' is byte 6 in the source");
    assert_eq!(head.offset, 12, "the 'a' after 'g' is byte 12");
    let copied = select::logical_text(&app.entries, &select::Selection { anchor, head });
    assert_eq!(
        copied, "beta g",
        "the source's space survives the soft wrap"
    );
}

/// The merge's integration point (s4's per-entry expansion × R2's Markdown): a `Ctrl-F` hit
/// inside a Markdown paragraph still lands on that paragraph when a folded reasoning channel
/// sits above it. The search measures the channel **fully expanded** (ADR 0028 R6), the jump
/// measures it folded (the display) - the two geometries differ, so the hit's absolute line has
/// to be recomputed against the display. This drives `App::find_jump` itself, so a jump that
/// measured the entries above expanded would land two lines off. The selection then copies the
/// source the paragraph covers.
#[test]
fn a_find_jump_lands_on_the_markdown_paragraph_under_a_folded_entry() {
    let mut app = App::new("m", "s");
    // A reasoning channel above: twenty lines expanded, one line folded.
    let thinking: String = (1..=20).map(|i| format!("thinking {i}\n")).collect();
    app.entries
        .push(radian_tui::app::Entry::new(EntryKind::Reasoning, &thinking));
    assistant(&mut app, "A **bold** paragraph here\n");
    // Filler below, so the transcript is longer than the viewport and the jump's scroll is
    // not clamped to zero: an expanded jump would scroll the paragraph off the top.
    for i in 0..30 {
        app.entries.push(radian_tui::app::Entry::new(
            EntryKind::Notice,
            format!("filler {i}"),
        ));
    }
    let width = 40;
    let _ = render(&mut app, width as u16, 12);
    let modes = app.display_modes();
    let search = blocks::search_modes(modes);
    let folded = render::display_lines(&app.entries[0], modes, width).len();
    let expanded = render::display_lines(&app.entries[0], search, width).len();
    assert!(folded < expanded, "folded {folded} vs expanded {expanded}");
    app.on_key(ctrl('f'));
    for c in "bold".chars() {
        app.on_key(key(KeyCode::Char(c)));
    }
    let find = app.find_state().expect("the find box is open");
    assert_eq!(find.matches.len(), 1, "the rendered paragraph is searched");
    let m = find.matches[0];
    assert_eq!(m.entry, 1);
    // `current_line` is the **search** geometry (the channel expanded); the jump recomputes
    // the display line with the channel folded.
    // The container's gap line before the assistant entry is part of both geometries
    // (ADR 0028 rule 1).
    let gap = blocks::block_gap(Some(&app.entries[0]), &app.entries[1]);
    assert_eq!(find.current_line(), Some(expanded + gap + m.line));
    assert!(!app.is_following(), "a jump closes follow");
    let hit_line = folded + gap + m.line;
    // The jump scrolled the paragraph into view: a jump that kept the expanded line would put
    // it twenty lines down, off a twelve-row screen.
    let terminal = render(&mut app, width as u16, 12);
    // The whole paragraph, not just "bold": the find box renders the query "bold" too.
    assert!(
        screen(&terminal).contains("A bold paragraph here"),
        "the paragraph is on screen after the jump:\n{}",
        screen(&terminal)
    );
    let lines = render::display_lines(&app.entries[1], modes, width);
    assert!(
        lines[m.line].contains("bold"),
        "the match is the paragraph line: {:?}",
        lines[m.line]
    );
    // The selection at that display line copies the paragraph's source (styles included).
    let anchor = select::locate(&app.entries, modes, width, hit_line, 0).expect("anchor");
    let head = select::locate(
        &app.entries,
        modes,
        width,
        hit_line,
        lines[m.line].chars().count() + 2,
    )
    .expect("head");
    let copied = select::logical_text(&app.entries, &select::Selection { anchor, head });
    assert!(
        copied.contains("**bold**"),
        "the copy is the paragraph's source: {copied:?}"
    );
}

/// The stronger form of the property, independent of `chars`: at a width where the block is one
/// display line, selecting the whole line copies the block's **source** exactly (inline markup
/// included). A `chars` mapping that collapsed onto a piece's start would truncate the copy.
#[test]
fn a_whole_line_selection_copies_the_source_line() {
    for body in [
        "alpha beta gamma delta epsilon\n",
        "A **bold** *italic* `code` tail\n",
        "a plain sentence with a long tail that still fits\n",
    ] {
        let mut app = App::new("m", "s");
        assistant(&mut app, body);
        let modes = modes(ReasoningMode::Expanded);
        let blocks = blocks::parse_blocks(body);
        let lines = blocks::layout(&app.entries[0], &blocks, modes, 80);
        // Padding rows (if any) come along; the copy still has to be the source.
        let last = lines.len() - 1;
        let cells = lines[last].chars.len();
        let anchor = select::locate(&app.entries, modes, 80, 0, 0).expect("anchor");
        let head = select::locate(&app.entries, modes, 80, last, cells + 2).expect("head");
        let copied = select::logical_text(&app.entries, &select::Selection { anchor, head });
        // A trailing newline in the source is a blank **edge** line, which the layout strips
        // (ADR 0028 rule 1): the display's last line is the line itself, and the
        // copy is what is displayed.
        assert_eq!(
            copied,
            body.trim_end_matches('\n'),
            "the whole line copies the displayed source line for {body:?}"
        );
    }
}

/// The source-independent half of the property: for every cell whose **displayed** character is
/// alphanumeric (no synthesized cell - a bullet, a table border, a link's ` (url)` - is
/// alphanumeric), the source at the anchor's offset starts with that same character. This does
/// not read `chars` to decide what is expected, so a mapping that collapsed onto a piece's start
/// fails here.
#[test]
fn every_displayed_source_char_is_at_its_offset() {
    for body in [
        "alpha beta gamma delta epsilon\n",
        "A **bold** *italic* `code` tail\n",
        "first paragraph\n\nsecond paragraph\n",
        "# Heading\n\nbody text\n",
    ] {
        for width in [80usize, 30, 16] {
            let mut app = App::new("m", "s");
            assistant(&mut app, body);
            let modes = modes(ReasoningMode::Expanded);
            let blocks = blocks::parse_blocks(body);
            let lines = blocks::layout(&app.entries[0], &blocks, modes, width);
            let cells: usize = lines.iter().map(|line| line.chars.len()).sum();
            for cell in 0..cells {
                let (line, column) = cell_of(&lines, cell).expect("a cell");
                let display = lines[line]
                    .text
                    .chars()
                    .nth(column)
                    .expect("the display char");
                if !display.is_alphanumeric() {
                    continue;
                }
                let anchor = select::locate(&app.entries, modes, width, line, column + 2)
                    .expect("an anchor");
                assert!(
                    body[anchor.offset..].starts_with(display),
                    "body {body:?} width {width} cell {cell}: offset {} does not start with \
                     {display:?}",
                    anchor.offset
                );
            }
        }
    }
}

/// ADR 0028 R5: only Assistant text goes through Markdown. A user message is plain text, so
/// `**bold**`, a `# heading` line and an inline `` `code` `` span show **literally** - the
/// asterisks, the hash and the backticks all stay - and the text keeps the user role's color,
/// not a `md*` role's.
#[test]
fn a_user_message_shows_markup_literally() {
    let mut app = App::new("m", "s");
    app.entries.push(radian_tui::app::Entry::new(
        EntryKind::User,
        "**bold**\n# heading\n`code`\n",
    ));
    let terminal = render(&mut app, 60, 12);
    let text = screen(&terminal);
    assert!(text.contains("**bold**"), "the asterisks stay: {text}");
    assert!(text.contains("# heading"), "the hash stays: {text}");
    assert!(text.contains("`code`"), "the backticks stay: {text}");
    assert!(!text.contains("•"), "no list bullet: {text}");
    // The user role: the message carries the user background band (the marker carries the
    // user foreground), not a `md*` role.
    let bold = style_of(&terminal, "**bold**").expect("the bold line");
    assert_eq!(bold.bg, app.theme.user.bg, "the user band: {bold:?}");
    let marker = style_of(&terminal, "> ").expect("the user marker");
    assert_eq!(marker.fg, app.theme.user.fg, "the user role: {marker:?}");
    assert_ne!(
        bold.bg, app.theme.md_code.bg,
        "not the Markdown code role: {bold:?}"
    );
}

/// The crash: a selection over Chinese Markdown panicked at
/// `select.rs`'s `text[offset..]` because a synthesized cell (a heading's `##`, a bullet, an
/// indent, a link's `(url)`) carried a fake source offset that landed inside a multi-byte
/// char. Every cell, at every width, now maps safely as either end of a selection.
///
/// The failure belongs to **one endpoint's** offset, so every cell is used as the anchor and as
/// the head against the first cell, the last cell and itself: 4 selections per cell instead of
/// every ordered pair. The all-pairs form was N² selections, each laying the entry out again,
/// and took about 700 s of the workspace test run (2026-10-15) for the same coverage.
#[test]
fn a_chinese_markdown_selection_does_not_panic() {
    let body = "能。这就是上面那份文档里 7 张图……\n\n## 一个很长的中文标题\n\n- 所以我逐张在这个库上检查了每一张图的来源，其中 **7 张图** 和 `code`\n\n> 这是一段中文引用\n\n```mermaid\ngraph TD; A-->B;\n```\n";
    for width in [80usize, 40, 20] {
        let mut app = App::new("m", "s");
        assistant(&mut app, body);
        let modes = modes(ReasoningMode::Expanded);
        // Every display cell, synthesized cells and each line's end gap included, located once.
        let points: Vec<_> = display_cells(&app, modes, width)
            .into_iter()
            .filter_map(|(line, col)| select::locate(&app.entries, modes, width, line, col + 2))
            .collect();
        assert!(!points.is_empty(), "the body lays out at width {width}");
        let (first, last) = (points[0], points[points.len() - 1]);
        for &point in &points {
            for (anchor, head) in [
                (point, first),
                (first, point),
                (point, last),
                (point, point),
            ] {
                let selection = select::Selection { anchor, head };
                let _ = select::logical_text(&app.entries, &selection);
                let _ = select::highlight_rows(&app.entries, modes, width, 0, 60, &selection);
            }
        }
    }
}

/// Every `(line, column)` of the entry's laid-out body, including one column past each line's
/// last cell (the end gap) and the synthesized cells.
fn display_cells(app: &App, modes: DisplayModes, width: usize) -> Vec<(usize, usize)> {
    let blocks = blocks::parse_blocks(&app.entries[0].text);
    let lines = blocks::layout(&app.entries[0], &blocks, modes, width);
    let mut out = Vec::new();
    for (line, body) in lines.iter().enumerate() {
        for column in 0..=body.chars.len() {
            out.push((line, column));
        }
    }
    out
}

/// The copy rule over the crash's body: selecting a Chinese run copies the source text. The
/// item's bullet is a synthesized cell, so the copy starts at the item's own text.
#[test]
fn a_chinese_selection_copies_the_source() {
    let mut app = App::new("m", "s");
    assistant(&mut app, "## 标题\n\n- 所以我逐张核对\n");
    let modes = modes(ReasoningMode::Expanded);
    let width = 40;
    let blocks = blocks::parse_blocks(&app.entries[0].text);
    let lines = blocks::layout(&app.entries[0], &blocks, modes, width);
    let line = lines
        .iter()
        .position(|l| l.text.contains("所以我逐张核对"))
        .expect("the item line");
    let sources: Vec<usize> = lines[line]
        .chars
        .iter()
        .enumerate()
        .filter_map(|(col, source)| source.map(|_| col))
        .collect();
    let first = sources[0];
    let line_width = unicode_width::UnicodeWidthStr::width(lines[line].text.as_str());
    let anchor = select::locate(&app.entries, modes, width, line, first + 2).expect("anchor");
    // One screen column past the line's last cell: the line's end gap.
    let head = select::locate(&app.entries, modes, width, line, line_width + 2).expect("head");
    let copied = select::logical_text(&app.entries, &select::Selection { anchor, head });
    assert_eq!(copied, "所以我逐张核对", "the item's source text");
}

/// Every laid-out line fits its width in **display columns**, not chars: a Chinese list item,
/// heading, quote or table at 80/40/20 columns wraps instead of overflowing the right edge
/// (the truncation the owner saw; the layout already measures display columns).
#[test]
fn every_chinese_line_fits_the_width() {
    let bodies = [
        "- 所以我逐张在这个库上检查了每一张图的来源和它的授权情况，然后整理成一份清单\n",
        "10. 所以我逐张在这个库上检查了每一张图的来源和它的授权情况，然后整理成一份清单\n",
        "- 所以我逐张在这个库上检查了每一张图的来源\n  - 所以我逐张在这个库上检查了每一张图的来源\n",
        "## 一个很长的中文标题它应该也会折行而不是在右边缘被截断掉后面的字\n",
        "> 这是一段很长的中文引用它应该也会在窗口右边缘折行而不是被直接截断\n",
        "- 所以我逐张检查了 **7 张图** 和 `code`，链接 [这里](https://example.com/a/b/c)\n",
        "| 中文表头 | 另一个中文表头 |\n| --- | --- |\n| 所以我逐张在这个库上检查 | 每一张图的来源 |\n",
    ];
    for body in bodies {
        for width in [80usize, 40, 20] {
            let mut app = App::new("m", "s");
            assistant(&mut app, body);
            let modes = modes(ReasoningMode::Expanded);
            let blocks = blocks::parse_blocks(body);
            let lines = blocks::layout(&app.entries[0], &blocks, modes, width);
            for line in &lines {
                assert!(
                    unicode_width::UnicodeWidthStr::width(line.text.as_str()) <= width,
                    "body {body:?} width {width}: line {:?} overflows",
                    line.text
                );
            }
        }
    }
}

/// The owner's truncation report (2026-10-14): a long ordered-list item in a wide window
/// (~160-200 columns) looked cut at the right edge. This is the message's **structure**
/// (anonymized): `##` headings, a bullet list with inline code, and long ordered-list items
/// with bold and inline code. Every line wraps in display columns and the whole text reaches
/// the screen at 120/160/200 - the report did not reproduce on this layout.
#[test]
fn a_long_ordered_list_item_wraps_in_a_wide_window() {
    let body = "图全部完成并实测通过。下面是结果。\n\n## 我做了什么\n\n在仓库里新增两个文件（`git status` 只有 `?? docs/notes/`，没有改动任何代码）：\n\n- `docs/notes/architecture-map.md` — 11 节的结构图与说明\n- `docs/notes/check-mermaid-blocks.py` — 校验脚本\n\n## 核对过的两个关键点\n\n1. **工作区不是 main**：当前在 `example-repo`（分支 `feature/example`，提交 `abc1234`）。我比对了 `git diff --stat main -- crates/`：`crates/` **无差异**，两边都有 ADR 0001–0032。所以这份图对 main 也成立（仓库比记忆里那次更新了一代：0032 已存在）。\n2. **图用项目自己的渲染器验证**：项目在 transcript 里画图用的是 vendored 渲染器，超宽或语法不支持就退回源码。所以这一项也要按显示列宽核对而不是按字符数。\n";
    for width in [120usize, 160, 200] {
        let mut app = App::new("m", "s");
        assistant(&mut app, body);
        let modes = modes(ReasoningMode::Expanded);
        let blocks = blocks::parse_blocks(body);
        let lines = blocks::layout(&app.entries[0], &blocks, modes, width);
        for line in &lines {
            assert!(
                unicode_width::UnicodeWidthStr::width(line.text.as_str()) <= width,
                "width {width}: line {:?} overflows",
                line.text
            );
        }
        // The tail of ordered item 1 reaches the screen (the report was that it vanished).
        let terminal = render(&mut app, width as u16, 60);
        let rendered: String = screen(&terminal).split_whitespace().collect();
        for needle in ["0032", "已存在", "渲染器验证"] {
            assert!(
                rendered.contains(needle),
                "width {width}: {needle:?} is missing from the frame"
            );
        }
    }
}
