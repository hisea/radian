//! Transcript images (ADR 0028 R10): the fixed row run under the placeholder line, the fold
//! rule, and the promise that selection, copy and search never see the image itself.

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

fn image_entry(app: &App) -> &radian_tui::Entry {
    app.session
        .entries
        .iter()
        .find(|entry| !entry.images.is_empty())
        .expect("an entry with an image")
}

/// ADR 0028 R10: the run is directly under the placeholder, `IMAGE_HEIGHT_ROWS` rows of no
/// text, and the counting and the drawing agree.
#[test]
fn a_user_image_gets_a_fixed_run_under_its_placeholder() {
    let app = user_app(vec![
        ContentPart::text("look at this"),
        image_part("shot.gif"),
    ]);
    let entry = image_entry(&app);
    assert_eq!(entry.kind, EntryKind::User);
    let all = modes(ToolFold::Expanded);
    let lines = blocks::layout(entry, &[], all, 80);
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
    assert_eq!(
        rows.len(),
        usize::from(radian_tui::media::IMAGE_HEIGHT_ROWS)
    );
    assert!(rows.iter().all(|line| line.text.is_empty()), "{texts:?}");
    assert_eq!(
        lines[placeholder + 1].image,
        Some(0),
        "the run starts directly under the placeholder: {texts:?}"
    );
    // The measure pass, the layout and the text-only view agree.
    assert_eq!(blocks::line_count(entry, all, 80), lines.len());
    assert_eq!(blocks::body_text_lines(entry, all, 80).len(), lines.len());
    // And all the text the transcript shows is the placeholder (the run is blank).
    assert!(
        blocks::body_text_lines(entry, all, 80)
            .iter()
            .all(|text| text.is_empty() || text.starts_with("[image:") || text == "look at this"),
        "{:?}",
        blocks::body_text_lines(entry, all, 80)
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
        0,
        0,
        &theme,
        radian_tui::spinner::Frame::default(),
    );
    let (count, lines, runs) = render::visible_lines_images(
        &mut app.session.entries,
        all,
        80,
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
        0,
        runs[0].row + 2,
        &radian_tui::theme::Theme::dark(),
        radian_tui::spinner::Frame::default(),
    );
    assert!(runs.is_empty(), "a clipped run is not drawn");
}

/// ADR 0028 R10's fold rule: a tool result's placeholder row always shows; the run only when
/// the body is drawn in full. (A user message has no fold, so its run always shows — the test
/// above.)
#[test]
fn a_folded_tool_result_shows_the_placeholder_but_not_the_image() {
    let app = tool_app();
    let entry = image_entry(&app);

    let collapsed = blocks::layout(entry, &[], modes(ToolFold::Collapsed), 80);
    let texts: Vec<&str> = collapsed.iter().map(|line| line.text.as_str()).collect();
    assert!(
        texts
            .iter()
            .any(|text| text.starts_with("[image: shot.gif")),
        "the placeholder stays in the folded block: {texts:?}"
    );
    assert!(
        collapsed.iter().all(|line| line.image.is_none()),
        "the folded block draws no image rows: {texts:?}"
    );
    assert_eq!(collapsed.len(), 1, "only the placeholder row: {texts:?}");

    let expanded = blocks::layout(entry, &[], modes(ToolFold::Expanded), 80);
    let rows = expanded.iter().filter(|line| line.image.is_some()).count();
    assert_eq!(rows, usize::from(radian_tui::media::IMAGE_HEIGHT_ROWS));
    assert!(
        expanded
            .iter()
            .any(|line| line.text.starts_with("[image: shot.gif")),
        "the placeholder stays above the run"
    );

    // The default `errors` fold is folded for a successful result.
    let errors = blocks::layout(entry, &[], modes(ToolFold::Errors), 80);
    assert!(errors.iter().all(|line| line.image.is_none()), "{errors:?}");

    // A failed result's body is never folded (ADR 0028 R4), so its image draws under every
    // fold.
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
        let lines = blocks::layout(failed_entry, &[], modes(fold), 80);
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
    let lines = blocks::layout(&entries[0], &[], all, 80);
    let last_row = lines.len() - 1;
    let selection = Selection {
        anchor: select::locate(entries, all, 80, 0, 0).expect("first line"),
        head: select::locate(entries, all, 80, last_row, 200).expect("last image row"),
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
    let (matches, _) = radian_tui::find::compute(entries, all, 80, &payload);
    assert!(
        matches.is_empty(),
        "search must not match inside the image rows: {matches:?}"
    );
    let (matches, _) = radian_tui::find::compute(entries, all, 80, "shot.gif");
    assert!(!matches.is_empty(), "the placeholder row is searchable");
    // Every match is on a row that draws text; none is on an image row.
    for hit in matches {
        let row = &blocks::layout(&entries[hit.entry], &[], all, 80)[hit.line];
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
    let lines = blocks::layout(entry, &[], all, 80);
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
        usize::from(radian_tui::media::IMAGE_HEIGHT_ROWS),
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
    let lines = blocks::layout(entry, &[], all, 80);
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
    assert_eq!(blocks::line_count(entry, all, 80), lines.len());
    assert_eq!(
        blocks::body_text_lines(entry, all, 80)
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
        let lines = blocks::layout(entry, &[], all, 80);
        let row = lines
            .iter()
            .position(|line| line.text == placeholder)
            .unwrap_or_else(|| panic!("the placeholder row under {fold:?}: {lines:?}"));
        // The placeholder row maps its first cell into the selection text, past the entry's own
        // text.
        let start = select::locate(entries, all, 80, row, 2).expect("the placeholder row");
        assert_eq!(
            start.offset, spans[0].0,
            "clicking the placeholder anchors in it"
        );
        // Dragging to the end of the (folded: absent) run copies exactly the placeholder.
        let end_row = lines
            .iter()
            .rposition(|line| line.image.is_some())
            .unwrap_or(row);
        let head = select::locate(entries, all, 80, end_row, 200).expect("past the run");
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
    let lines = blocks::layout(entry, &[], modes(ToolFold::Collapsed), 80);
    let placeholder = lines
        .iter()
        .find(|line| line.text.starts_with("[image: shot.gif"))
        .expect("the placeholder row");
    assert_eq!(placeholder.frame, radian_tui::blocks::Frame::Body);
    assert!(placeholder.hint, "synthesized, like a head's meta");
}
