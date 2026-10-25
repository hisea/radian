//! Transcript images (ADR 0028 R10): the fixed row run under the placeholder line, the rule
//! that it draws under every fold, and the promise that selection, copy and search never see
//! the image itself.

use radian_core::{ContentPart, Message, MessageMeta, ToolResult};
use radian_tui::app::{App, EntryKind};
use radian_tui::blocks;
use radian_tui::render;
use radian_tui::select::{self, Selection};
use radian_tui::tools::{DisplayModes, ToolDisplay, ToolFold};
use serde_json::json;

/// A 1×1 GIF, the smallest decodable image the pipeline accepts (43 bytes).
const GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00,
    0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

/// The transcript area's height these fixtures are laid out at. 24 rows is the image
/// minimum's boundary: half is 12, exactly `IMAGE_MIN_ROWS`.
const HEIGHT: usize = 24;

/// The image run the layout reserves at [`HEIGHT`].
fn run_rows() -> usize {
    usize::from(radian_tui::media::image_height_rows(HEIGHT))
}

fn base64(bytes: &[u8]) -> String {
    radian_coding::image::base64_encode(bytes)
}

fn image_part(name: &str) -> ContentPart {
    ContentPart::Image {
        mime_type: "image/gif".to_owned(),
        data: base64(GIF),
        annotations: Some(json!({
            "kind": radian_coding::image::IMAGE_KIND,
            "name": name,
            "width": 1,
            "height": 1,
            "bytes": GIF.len(),
        })),
        meta: None,
    }
}

fn modes(fold: ToolFold) -> DisplayModes {
    DisplayModes {
        tools: ToolDisplay {
            fold,
            preview_lines: 5,
        },
        ..DisplayModes::default()
    }
}

fn user_app(parts: Vec<ContentPart>) -> App {
    let mut app = App::new("m", "s");
    app.push_message(&Message::User {
        content: parts,
        meta: MessageMeta::default(),
    });
    app
}

fn tool_app() -> App {
    let mut app = App::new("m", "s");
    let result = ToolResult::success(
        "c1".to_owned(),
        vec![
            ContentPart::text("Read image file [image/gif] 1×1"),
            image_part("shot.gif"),
        ],
    );
    app.push_message(&Message::Tool {
        result,
        meta: MessageMeta::default(),
    });
    app
}

/// A tool result whose text is `lines` one-row lines, followed by `parts` (images). A generic
/// tool (no call paired), so the fold applies to the text body exactly as it does for a real
/// one.
fn tool_result_app(lines: usize, parts: Vec<ContentPart>) -> App {
    let text = (1..=lines)
        .map(|n| format!("row {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut content = vec![ContentPart::text(text)];
    content.extend(parts);
    let mut app = App::new("m", "s");
    app.push_message(&Message::Tool {
        result: ToolResult::success("c1".to_owned(), content),
        meta: MessageMeta::default(),
    });
    app
}

/// The non-empty body text rows a layout draws (the synthesized placeholder rows are `hint`).
fn text_rows(lines: &[radian_tui::blocks::BodyLine]) -> Vec<String> {
    lines
        .iter()
        .filter(|line| line.image.is_none() && !line.hint && !line.text.is_empty())
        .map(|line| line.text.clone())
        .collect()
}

fn image_entry(app: &App) -> &radian_tui::Entry {
    app.session
        .entries
        .iter()
        .find(|entry| !entry.images.is_empty())
        .expect("an entry with an image")
}

/// ADR 0028 R10: the run is directly under the placeholder, `IMAGE_MIN_ROWS` rows of no
/// text at this transcript height, and the counting and the drawing agree.
#[test]
fn a_user_image_gets_a_fixed_run_under_its_placeholder() {
    let app = user_app(vec![
        ContentPart::text("look at this"),
        image_part("shot.gif"),
    ]);
    let entry = image_entry(&app);
    assert_eq!(entry.kind, EntryKind::User);
    let all = modes(ToolFold::Expanded);
    let lines = blocks::layout(entry, &[], all, 80, HEIGHT);
    let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
    let placeholder = texts
        .iter()
        .position(|text| text.starts_with("[image: shot.gif"))
        .expect("the placeholder row");
    assert_eq!(
        texts[placeholder], "[image: shot.gif, 1×1, 1 KB]",
        "{texts:?}"
    );
    let rows: Vec<&radian_tui::blocks::BodyLine> =
        lines.iter().filter(|line| line.image.is_some()).collect();
    assert_eq!(rows.len(), run_rows());
    assert!(rows.iter().all(|line| line.text.is_empty()), "{texts:?}");
    assert_eq!(
        lines[placeholder + 1].image,
        Some(0),
        "the run starts directly under the placeholder: {texts:?}"
    );
    // The measure pass, the layout and the text-only view agree.
    assert_eq!(blocks::line_count(entry, all, 80, HEIGHT), lines.len());
    assert_eq!(
        blocks::body_text_lines(entry, all, 80, HEIGHT).len(),
        lines.len()
    );
    // And all the text the transcript shows is the placeholder (the run is blank).
    assert!(
        blocks::body_text_lines(entry, all, 80, HEIGHT)
            .iter()
            .all(|text| text.is_empty() || text.starts_with("[image:") || text == "look at this"),
        "{:?}",
        blocks::body_text_lines(entry, all, 80, HEIGHT)
    );
}

/// The window reports an image run only when the whole run is on screen; the frame's own
/// clamp is what turns a clipped run into nothing.
#[test]
fn the_window_reports_a_run_only_when_the_whole_run_is_visible() {
    let mut app = user_app(vec![ContentPart::text("hi"), image_part("shot.gif")]);
    let all = modes(ToolFold::Expanded);
    let theme = radian_tui::theme::Theme::dark();
    let (total, _, _) = render::visible_lines_images(
        &mut app.session.entries,
        all,
        80,
        HEIGHT,
        0,
        0,
        &theme,
        radian_tui::spinner::Frame::default(),
    );
    let (count, lines, runs) = render::visible_lines_images(
        &mut app.session.entries,
        all,
        80,
        HEIGHT,
        0,
        total,
        &theme,
        radian_tui::spinner::Frame::default(),
    );
    assert!(count > 0);
    assert_eq!(runs.len(), 1, "the full run is visible: {lines:?}");
    assert!(
        lines[runs[0].row]
            .spans
            .iter()
            .all(|span| span.content.trim().is_empty()),
        "the run's first row draws no text"
    );
    // A window that cuts the run reports none (it would split the image across the edge).
    let (_, _, runs) = render::visible_lines_images(
        &mut app.session.entries,
        all,
        80,
        HEIGHT,
        0,
        runs[0].row + 2,
        &radian_tui::theme::Theme::dark(),
        radian_tui::spinner::Frame::default(),
    );
    assert!(runs.is_empty(), "a clipped run is not drawn");
}

/// ADR 0028 R10 (TK0120): a tool result's image draws whatever the fold. Each placeholder row
/// is followed by its run under `collapsed`, `errors` and `expanded`; the fold still governs
/// the **text**: `collapsed` hides every body row, `errors` keeps the last preview rows, and
/// `expanded` draws them all. Counting and drawing agree in each case.
#[test]
fn a_tool_result_image_draws_under_every_fold() {
    let app = tool_result_app(8, vec![image_part("shot.gif")]);
    let entry = image_entry(&app);
    let placeholder = entry.images[0].placeholder.clone();

    for (fold, want_text) in [
        (ToolFold::Collapsed, Vec::new()),
        (
            ToolFold::Errors,
            (4..=8).map(|n| format!("row {n}")).collect::<Vec<_>>(),
        ),
        (
            ToolFold::Expanded,
            (1..=8).map(|n| format!("row {n}")).collect::<Vec<_>>(),
        ),
    ] {
        let all = modes(fold);
        let lines = blocks::layout(entry, &[], all, 80, HEIGHT);
        let at = lines
            .iter()
            .position(|line| line.text == placeholder)
            .unwrap_or_else(|| panic!("the placeholder row under {fold:?}: {lines:?}"));
        let run: Vec<&radian_tui::blocks::BodyLine> =
            lines.iter().filter(|line| line.image.is_some()).collect();
        assert_eq!(run.len(), run_rows(), "the run draws under {fold:?}");
        assert!(
            run.iter().all(|line| line.image == Some(0)),
            "the run belongs to the image: {lines:?}"
        );
        assert_eq!(
            lines[at + 1].image,
            Some(0),
            "the run starts directly under the placeholder under {fold:?}: {lines:?}"
        );
        // The fold still hides text: the drawn text rows are exactly the rows the fold keeps.
        assert_eq!(text_rows(&lines), want_text, "the text rows under {fold:?}");
        // The count and the text-only path see the same shape as the drawing.
        assert_eq!(
            blocks::line_count(entry, all, 80, HEIGHT),
            lines.len(),
            "the count equals the drawing under {fold:?}"
        );
        assert_eq!(
            blocks::body_text_lines(entry, all, 80, HEIGHT).len(),
            lines.len(),
            "the text-only view equals the drawing under {fold:?}"
        );
    }
}

/// ADR 0028 R10 (TK0120): two images keep their order under every fold — each placeholder row
/// is followed by its own run, and the first image's run ends under its own placeholder.
#[test]
fn two_tool_images_keep_their_order_under_a_fold() {
    let app = tool_result_app(2, vec![image_part("one.gif"), image_part("two.gif")]);
    let entry = image_entry(&app);
    assert_eq!(entry.images.len(), 2);
    assert_ne!(entry.images[0].placeholder, entry.images[1].placeholder);

    for fold in [ToolFold::Collapsed, ToolFold::Errors, ToolFold::Expanded] {
        let all = modes(fold);
        let lines = blocks::layout(entry, &[], all, 80, HEIGHT);
        let one = lines
            .iter()
            .position(|line| line.text == entry.images[0].placeholder)
            .unwrap_or_else(|| panic!("the first placeholder under {fold:?}: {lines:?}"));
        let two = lines
            .iter()
            .position(|line| line.text == entry.images[1].placeholder)
            .unwrap_or_else(|| panic!("the second placeholder under {fold:?}: {lines:?}"));
        assert!(one < two, "the images keep their order under {fold:?}");
        assert_eq!(lines[one + 1].image, Some(0), "under the first placeholder");
        assert_eq!(
            lines[one + run_rows()].image,
            Some(0),
            "the first run's last row under {fold:?}: {lines:?}"
        );
        assert_eq!(
            lines[one + run_rows() + 1].text,
            entry.images[1].placeholder,
            "the second placeholder follows the first run under {fold:?}: {lines:?}"
        );
        assert_eq!(
            lines[two + 1].image,
            Some(1),
            "under the second placeholder"
        );
        assert_eq!(
            blocks::line_count(entry, all, 80, HEIGHT),
            lines.len(),
            "the count equals the drawing under {fold:?}"
        );
    }
}

/// ADR 0028 R10 (TK0120): a payload that does not decode draws only its placeholder row, under
/// every fold — the fold is not what decides it, and no blank hole is left.
#[test]
fn a_broken_tool_image_draws_only_its_placeholder_under_a_fold() {
    let mut broken = image_part("broken.gif");
    if let ContentPart::Image { data, .. } = &mut broken {
        *data = "not base64 at all".to_owned();
    }
    let app = tool_result_app(8, vec![broken]);
    let entry = image_entry(&app);
    assert!(!entry.images[0].decodable, "the payload is broken");

    for fold in [ToolFold::Collapsed, ToolFold::Errors, ToolFold::Expanded] {
        let all = modes(fold);
        let lines = blocks::layout(entry, &[], all, 80, HEIGHT);
        assert!(
            lines.iter().all(|line| line.image.is_none()),
            "a failed decode reserves no run under {fold:?}: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.text == entry.images[0].placeholder),
            "the placeholder stays under {fold:?}: {lines:?}"
        );
        assert_eq!(
            blocks::line_count(entry, all, 80, HEIGHT),
            lines.len(),
            "the count equals the drawing under {fold:?}"
        );
        assert_eq!(
            blocks::body_text_lines(entry, all, 80, HEIGHT).len(),
            lines.len(),
            "the text-only view equals the drawing under {fold:?}"
        );
    }
}

/// ADR 0028 R4 / R10: a failed result's body is never folded, so its text and its image rows
/// draw under every fold (unchanged by TK0120).
#[test]
fn a_failed_tool_result_draws_its_image_under_every_fold() {
    let mut failed = ToolResult::success(
        "c2".to_owned(),
        vec![
            ContentPart::text("the image could not be read"),
            image_part("broken.gif"),
        ],
    );
    failed.is_error = true;
    failed.error_code = Some("image_too_large".to_owned());
    let mut app = App::new("m", "s");
    app.push_message(&Message::Tool {
        result: failed,
        meta: MessageMeta::default(),
    });
    let failed_entry = image_entry(&app);
    for fold in [ToolFold::Collapsed, ToolFold::Errors, ToolFold::Expanded] {
        let lines = blocks::layout(failed_entry, &[], modes(fold), 80, HEIGHT);
        assert!(
            lines.iter().any(|line| line.image.is_some()),
            "a failed result keeps its image rows under {fold:?}: {lines:?}"
        );
    }
}

/// ADR 0028 R10: the image's own rows carry no characters. Selection copies the placeholder
/// row only, and Ctrl-F can match the placeholder but nothing inside the run.
#[test]
fn selection_and_search_never_see_the_image_rows() {
    let app = user_app(vec![ContentPart::text("look"), image_part("shot.gif")]);
    let entries = &app.session.entries;
    let all = modes(ToolFold::Expanded);
    let lines = blocks::layout(&entries[0], &[], all, 80, HEIGHT);
    let last_row = lines.len() - 1;
    let selection = Selection {
        anchor: select::locate(entries, all, 80, HEIGHT, 0, 0).expect("first line"),
        head: select::locate(entries, all, 80, HEIGHT, last_row, 200).expect("last image row"),
    };
    assert_eq!(
        select::logical_text(entries, &selection),
        "look\n\n[image: shot.gif, 1×1, 1 KB]"
    );

    // Searching for the payload (or anything the image stands for) matches nothing; the
    // placeholder itself is a real, searchable row.
    let part = image_part("shot.gif");
    let payload = match &part {
        ContentPart::Image { data, .. } => data[..16].to_owned(),
        _ => unreachable!("an image part"),
    };
    let (matches, _) = radian_tui::find::compute(entries, all, 80, HEIGHT, &payload);
    assert!(
        matches.is_empty(),
        "search must not match inside the image rows: {matches:?}"
    );
    let (matches, _) = radian_tui::find::compute(entries, all, 80, HEIGHT, "shot.gif");
    assert!(!matches.is_empty(), "the placeholder row is searchable");
    // Every match is on a row that draws text; none is on an image row.
    for hit in matches {
        let row = &blocks::layout(&entries[hit.entry], &[], all, 80, HEIGHT)[hit.line];
        assert!(row.image.is_none(), "a match landed on an image row");
    }
}

/// ADR 0028 R10's fallback, with two images whose placeholders read the same: the broken one
/// reserves no run, and the valid one still gets its run under **its own** placeholder row —
/// the broken image must not steal it.
#[test]
fn a_broken_image_does_not_steal_a_valid_twins_run() {
    let mut broken = image_part("shot.gif");
    if let ContentPart::Image { data, .. } = &mut broken {
        *data = "not base64 at all".to_owned();
    }
    // The annotations are identical, so both parts synthesize the same placeholder line.
    let app = user_app(vec![
        ContentPart::text("two shots"),
        broken,
        image_part("shot.gif"),
    ]);
    let entry = image_entry(&app);
    assert_eq!(entry.images.len(), 2, "both parts name themselves");
    assert!(!entry.images[0].decodable, "the first payload is broken");
    assert!(entry.images[1].decodable, "the second payload decodes");
    let all = modes(ToolFold::Expanded);
    let lines = blocks::layout(entry, &[], all, 80, HEIGHT);
    let placeholder = entry.images[0].placeholder.clone();
    let rows: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.text == placeholder)
        .map(|(at, _)| at)
        .collect();
    assert_eq!(rows.len(), 2, "both placeholder rows draw: {lines:?}");
    assert!(
        lines[rows[0] + 1].image.is_none(),
        "the broken image reserves no run: {lines:?}"
    );
    let run: Vec<&radian_tui::blocks::BodyLine> =
        lines.iter().filter(|line| line.image.is_some()).collect();
    assert_eq!(
        run.len(),
        run_rows(),
        "only the valid image has a run: {lines:?}"
    );
    assert!(
        run.iter().all(|line| line.image == Some(1)),
        "the run belongs to the valid image: {lines:?}"
    );
    assert_eq!(
        lines[rows[1] + 1].image,
        Some(1),
        "the run starts under the second placeholder: {lines:?}"
    );
}

/// ADR 0028 R10's fallback: a payload that does not decode draws only the placeholder row and
/// produces no error block, and it reserves no rows — the placeholder is the whole story, with
/// no blank hole where the image would have been. The cache remembers the failure instead of
/// retrying every frame.
#[test]
fn a_broken_image_draws_only_its_placeholder() {
    use radian_tui::media::{EntryImage, ImageCache};

    let mut part = image_part("broken.gif");
    if let ContentPart::Image { data, .. } = &mut part {
        *data = "not base64 at all".to_owned();
    }
    let app = user_app(vec![ContentPart::text("look"), part.clone()]);
    let entry = image_entry(&app);
    let all = modes(ToolFold::Expanded);
    let lines = blocks::layout(entry, &[], all, 80, HEIGHT);
    let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
    assert!(
        texts
            .iter()
            .any(|text| text.starts_with("[image: broken.gif")),
        "{texts:?}"
    );
    assert!(
        lines.iter().all(|line| line.image.is_none()),
        "a failed decode reserves no run: {texts:?}"
    );
    assert!(
        texts
            .iter()
            .all(|text| text.is_empty() || !text.contains("error")),
        "no error block: {texts:?}"
    );
    // The counting, the drawing and the text-only view agree on the shorter shape.
    assert_eq!(blocks::line_count(entry, all, 80, HEIGHT), lines.len());
    assert_eq!(
        blocks::body_text_lines(entry, all, 80, HEIGHT)
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        texts
    );

    let mut cache = ImageCache::default();
    let image = EntryImage::from_part(&part, &mut cache).expect("the placeholder still exists");
    assert!(!image.decodable);
    let picker = ratatui_image::picker::Picker::halfblocks();
    assert!(cache.protocol(&image, &picker).is_none());
    assert!(!cache.is_ready(&image));
}

/// ADR 0028 R10: a tool result's placeholder line is synthesized by the layout, but selection
/// and copy still see it. A click on the row anchors inside the selection text, and dragging
/// across the run copies the placeholder only — never the image's rows.
#[test]
fn a_tool_result_placeholder_is_copied_and_the_run_is_not() {
    let app = tool_app();
    let entries = &app.session.entries;
    let index = entries
        .iter()
        .position(|entry| !entry.images.is_empty())
        .expect("an entry with an image");
    let entry = &entries[index];
    let placeholder = entry.images[0].placeholder.clone();
    let spans = radian_tui::media::placeholder_spans(entry.text.len(), &entry.images);

    for fold in [ToolFold::Collapsed, ToolFold::Expanded] {
        let all = modes(fold);
        let lines = blocks::layout(entry, &[], all, 80, HEIGHT);
        let row = lines
            .iter()
            .position(|line| line.text == placeholder)
            .unwrap_or_else(|| panic!("the placeholder row under {fold:?}: {lines:?}"));
        // The placeholder row maps its first cell into the selection text, past the entry's own
        // text.
        let start = select::locate(entries, all, 80, HEIGHT, row, 2).expect("the placeholder row");
        assert_eq!(
            start.offset, spans[0].0,
            "clicking the placeholder anchors in it"
        );
        // Dragging to the end of the (folded: absent) run copies exactly the placeholder.
        let end_row = lines
            .iter()
            .rposition(|line| line.image.is_some())
            .unwrap_or(row);
        let head = select::locate(entries, all, 80, HEIGHT, end_row, 200).expect("past the run");
        let selection = Selection {
            anchor: start,
            head,
        };
        assert_eq!(
            select::logical_text(entries, &selection),
            placeholder,
            "the run contributes no characters ({fold:?})"
        );

        // From the body's first character to the end of the run, the copy carries the entry's
        // own text and then the placeholder line.
        if fold == ToolFold::Expanded {
            let selection = Selection {
                anchor: select::Anchor {
                    entry: index,
                    offset: 0,
                },
                head,
            };
            assert_eq!(
                select::logical_text(entries, &selection),
                format!("{}\n{placeholder}", entry.text),
                "the placeholder is part of the copied text"
            );
        }
    }
}

/// A tool result's placeholder row is the only text its image contributes, and it is drawn
/// inside the block's `│ ` body (a `Body` frame row), so the rail and the text column hold.
#[test]
fn a_tool_image_placeholder_is_a_body_row() {
    let app = tool_app();
    let entry = image_entry(&app);
    let lines = blocks::layout(entry, &[], modes(ToolFold::Collapsed), 80, HEIGHT);
    let placeholder = lines
        .iter()
        .find(|line| line.text.starts_with("[image: shot.gif"))
        .expect("the placeholder row");
    assert_eq!(placeholder.frame, radian_tui::blocks::Frame::Body);
    assert!(placeholder.hint, "synthesized, like a head's meta");
}

fn text(lines: &[ratatui::text::Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

/// ADR 0028 R10: the run is half the transcript area's height for 20, 50 and 100 rows (the
/// minimum, a middle value, the maximum).
#[test]
fn the_run_height_follows_the_transcript_area() {
    let app = user_app(vec![ContentPart::text("look"), image_part("shot.gif")]);
    let entry = image_entry(&app);
    let all = modes(ToolFold::Expanded);
    for (height, want) in [(20usize, 12usize), (50, 25), (100, 40)] {
        let lines = blocks::layout(entry, &[], all, 80, height);
        let rows = lines.iter().filter(|line| line.image.is_some()).count();
        assert_eq!(rows, want, "transcript area of {height} rows");
        assert_eq!(
            blocks::line_count(entry, all, 80, height),
            lines.len(),
            "the count equals the drawing at {height} rows"
        );
        assert_eq!(
            blocks::body_text_lines(entry, all, 80, height).len(),
            lines.len(),
            "the text-only view equals the drawing at {height} rows"
        );
    }
}

/// ADR 0028 R10: resizing the transcript from 50 to 100 rows recomputes the run, and every
/// window at the new height still equals the full wrap row for row (scrolling stays exact).
#[test]
fn a_resize_recomputes_the_run_and_keeps_scrolling_exact() {
    let mut app = user_app(vec![
        ContentPart::text("look at this"),
        image_part("shot.gif"),
    ]);
    let all = modes(ToolFold::Expanded);
    let theme = radian_tui::theme::Theme::dark();
    let mut totals = Vec::new();
    for height in [50usize, 100] {
        let full = text(&render::transcript_lines(
            &app.session.entries,
            all,
            80,
            height,
            &theme,
        ));
        // The image entry's run is part of the full wrap.
        let entry = image_entry(&app);
        let laid_out = blocks::layout(entry, &[], all, 80, height);
        assert_eq!(
            laid_out.len(),
            full.len(),
            "one entry: the user message and its run"
        );
        for start in 0..=full.len() {
            let (total, window, _) = render::visible_lines_images(
                &mut app.session.entries,
                all,
                80,
                height,
                start,
                full.len().max(1),
                &theme,
                radian_tui::spinner::Frame::default(),
            );
            assert_eq!(total, full.len(), "height {height}, start {start}");
            assert_eq!(
                text(&window),
                full[start..].to_vec(),
                "height {height}, start {start}"
            );
        }
        totals.push(full.len());
    }
    // The taller area's run is 15 rows longer (25 → 40), so the transcript grew by exactly that.
    assert_eq!(totals[1] - totals[0], 40 - 25);
}

/// ADR 0028 R10: a very wide image is scaled down to the body column's width. The reserved
/// run keeps its computed height (the drawn picture is shorter, top-aligned); row counting and
/// drawing stay on that one layout.
#[test]
fn a_very_wide_image_is_bounded_by_the_body_column() {
    let picker = ratatui_image::picker::Picker::halfblocks(); // a 10×20 px cell
    let mut image = radian_tui::media::EntryImage::from_part(
        &image_part("wide.gif"),
        &mut radian_tui::media::ImageCache::default(),
    )
    .expect("an image part");
    // A 40:1 image: at 40 rows it would want 40×20 px × 40 / 10 px = 3200 columns.
    image.width = 4000;
    image.height = 100;
    let rows = radian_tui::media::image_height_rows(100);
    assert_eq!(rows, 40);
    assert_eq!(
        radian_tui::media::image_columns(&picker, &image, 60, rows),
        60,
        "the body column bounds the width"
    );
    // The layout still reserves the full run: the width clamp does not shrink it.
    let app = user_app(vec![ContentPart::text("look"), image_part("wide.gif")]);
    let entry = image_entry(&app);
    let lines = blocks::layout(entry, &[], modes(ToolFold::Expanded), 80, 100);
    assert_eq!(
        lines.iter().filter(|line| line.image.is_some()).count(),
        usize::from(rows)
    );
}

/// ADR 0028 R10: a frame sizes the image run from the **transcript area** (the rows the
/// transcript can draw in), not from the whole terminal: the composer, footer and other
/// chrome are not part of the half. A tall fixture where the two differ is the proof.
#[test]
fn the_run_follows_the_transcript_area_not_the_whole_terminal() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = user_app(vec![ContentPart::text("look"), image_part("shot.gif")]);
    let mut terminal = Terminal::new(TestBackend::new(80, 50)).unwrap();
    terminal
        .draw(|frame| radian_tui::render::draw(frame, &mut app))
        .unwrap();
    let area = usize::from(app.session.view_height);
    assert!(area < 50, "the fixture leaves room for the chrome: {area}");
    assert_ne!(
        radian_tui::media::image_height_rows(area),
        radian_tui::media::image_height_rows(50),
        "the fixture must tell the transcript area from the whole terminal"
    );
    let modes = app.display_modes();
    let theme = app.theme.clone();
    let (total, _) = render::visible_lines(&mut app.session.entries, modes, 80, area, 0, 0, &theme);
    let (whole, _) = render::visible_lines(&mut app.session.entries, modes, 80, 50, 0, 0, &theme);
    assert_ne!(total, whole, "the fixture must tell the two layouts apart");
    assert_eq!(
        app.session.total_lines, total,
        "the frame measured at the transcript area's height"
    );
}
