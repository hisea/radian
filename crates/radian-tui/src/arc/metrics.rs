//! The task page's metrics tab (ADR 0048 §11 and §13): what each attempt spent.
//!
//! The total row over every attempt opens the list, then one attempt header per attempt, newest
//! first, with one row per run (`worker`, `review 1`, `review 2`) and the attempt's gate rows and
//! review findings. Every stage row shares one layout: a label column, a right-aligned duration
//! column and the detail. The counters decide the run's shape: a run that reported a counter takes
//! two rows, the route and cost then its labelled counters, and a record with no counter to draw
//! (no metrics at all, or only a fee or a cache write) is one plain row. A value a record did not
//! report is left out rather than drawn as `-`; the one exception is the context window
//! (`ctx 83K/-`), whose missing window is a weak `-`.
//! `j`/`k` move the `❯` over the run rows, `Enter` opens the selected run's tool table and request
//! log below its last row, and `Esc` closes that detail before it leaves the page.
//!
//! Numbers use [`crate::timing`]'s compact forms, the same the footer's `tokens` item draws.
//! Nothing here reads a file: the page state loaded the metrics through `radian_arc::read`.

use std::path::Path;
use std::time::Duration;

use radian_arc::gate::Finding;
use radian_arc::read::{AttemptMetrics, AttemptView, MetricsTotal, RunMetrics, TaskMetrics};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::TaskPage;
use super::render::{fit_spans, rail};
use crate::theme::Theme;
use crate::timing::{format_cost, format_seconds, format_tokens};

/// The duration column's width: `13m08s`, `50m48s`, `1h18m` right-aligned.
const DURATION_WIDTH: usize = 7;
/// The label column never grows past this many columns, however long a command label is.
const MAX_LABEL_WIDTH: usize = 16;
/// The spaces between the duration column and the detail column.
const DETAIL_GAP: usize = 3;

/// How many run rows the tab offers: one worker run per attempt and one per review run, the
/// newest attempt first.
pub fn run_count(metrics: &TaskMetrics) -> usize {
    metrics
        .attempts
        .iter()
        .map(|attempt| 1 + attempt.reviews.len())
        .sum()
}

/// The `index`th run row, newest attempt first: the attempt it belongs to, and the review run
/// (`None` for the attempt's worker run). `None` when the index is past the last row.
pub fn run_at(
    metrics: &TaskMetrics,
    index: usize,
) -> Option<(&AttemptMetrics, Option<&RunMetrics>)> {
    let mut at = 0;
    for attempt in metrics.attempts.iter().rev() {
        if at == index {
            return Some((attempt, None));
        }
        at += 1;
        for run in &attempt.reviews {
            if at == index {
                return Some((attempt, Some(run)));
            }
            at += 1;
        }
    }
    None
}

/// Draws the tab into the frame's rows. `width` is the frame's body width (the area minus the
/// two marker columns), and a rail spends two more, so every row is cut to `width - 2` cells.
pub fn draw(
    lines: &mut Vec<Line<'static>>,
    page: &mut TaskPage,
    width: usize,
    height: usize,
    theme: &Theme,
) {
    let content = width.saturating_sub(2);
    let (rows, run_rows) = {
        let Some(metrics) = page.metrics.as_ref() else {
            let (text, style) = match &page.metrics_error {
                Some(error) => (error.clone(), theme.status_error),
                None => ("loading…".to_owned(), theme.hint),
            };
            lines.push(rail(
                theme,
                Line::from(fit_spans(vec![Span::styled(text, style)], content)),
            ));
            return;
        };
        if metrics.attempts.is_empty() {
            lines.push(rail(
                theme,
                Line::from(Span::styled("no attempts yet", theme.hint)),
            ));
            return;
        }
        let layout = Layout {
            label_width: label_width(metrics),
            content,
        };
        let mut rows: Vec<Line<'static>> = Vec::new();
        let mut run_rows: Vec<usize> = Vec::with_capacity(run_count(metrics));
        let mut run_last: Vec<usize> = Vec::with_capacity(run_count(metrics));
        let mut index = 0;
        rows.push(rail(
            theme,
            total_line(&metrics.total, metrics.attempts.len(), content, theme),
        ));
        rows.push(rail(theme, Line::default()));
        let attempts = metrics.attempts.len();
        for (position, attempt) in metrics.attempts.iter().rev().enumerate() {
            rows.push(rail(theme, attempt_header(attempt, content, theme)));
            let (first, last) = push_run(
                &mut rows,
                &attempt.worker,
                "worker",
                index == page.metric_sel,
                &layout,
                theme,
            );
            run_rows.push(first);
            run_last.push(last);
            index += 1;
            for run in &attempt.reviews {
                let label = run
                    .run
                    .map_or_else(|| "review".to_owned(), |number| format!("review {number}"));
                let (first, last) = push_run(
                    &mut rows,
                    run,
                    &label,
                    index == page.metric_sel,
                    &layout,
                    theme,
                );
                run_rows.push(first);
                run_last.push(last);
                index += 1;
            }
            push_attempt_rows(&mut rows, attempt, &page.attempts, &layout, theme);
            if position + 1 < attempts {
                rows.push(rail(theme, plain("─────", theme.hint, content)));
            }
        }
        // The selected run's detail sits directly below the run's last row.
        if page.metric_open
            && let Some((attempt, run)) = run_at(metrics, page.metric_sel)
            && let Some(&position) = run_last.get(page.metric_sel)
        {
            let run = run.unwrap_or(&attempt.worker);
            let detail = detail_rows(run, content, theme);
            rows.splice(
                position + 1..position + 1,
                detail.into_iter().map(|line| rail(theme, line)),
            );
        }
        (rows, run_rows)
    };

    let count = run_rows.len();
    page.metric_sel = page.metric_sel.min(count.saturating_sub(1));
    let visible = height.saturating_sub(lines.len() + 1);
    let last = rows.len().saturating_sub(visible);
    // Opening the detail anchors the list on the selected run, so the detail starts right under
    // it instead of below the fold.
    if page.metric_open && !page.metric_open_shown {
        page.metric_scroll = run_rows.get(page.metric_sel).copied().unwrap_or(0);
        page.metric_open_shown = true;
    }
    if !page.metric_open {
        page.metric_open_shown = false;
    }
    // A moved selection brings its run row into view; a scroll keeps the place the user chose.
    if page.metric_shown != Some(page.metric_sel) {
        if let Some(start) = run_rows.get(page.metric_sel).copied() {
            if start < page.metric_scroll {
                page.metric_scroll = start;
            } else if visible > 0 && start >= page.metric_scroll + visible {
                page.metric_scroll = start + 1 - visible;
            }
        }
        page.metric_shown = Some(page.metric_sel);
    }
    if page.metric_follow {
        page.metric_scroll = last;
        page.metric_follow = false;
    } else {
        page.metric_scroll = page.metric_scroll.min(last);
    }
    for line in rows.into_iter().skip(page.metric_scroll).take(visible) {
        lines.push(line);
    }
}

/// One styled piece of the detail's tables. A `weak` piece is a value the record did not report,
/// and a `missing_tail` draws a weak `-` after reported text.
struct Cell {
    text: String,
    weak: bool,
    missing_tail: bool,
}

impl Cell {
    fn reported(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            weak: false,
            missing_tail: false,
        }
    }

    fn missing() -> Self {
        Self {
            text: "-".to_owned(),
            weak: true,
            missing_tail: false,
        }
    }

    fn header(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            weak: true,
            missing_tail: false,
        }
    }

    fn optional(value: Option<String>) -> Self {
        value.map_or_else(Self::missing, Self::reported)
    }
}

/// One counter of a run's second row. `missing_tail` is the one value the tab still marks with a
/// weak `-`: `ctx 83K/-` reports the peak and not the window.
struct Item {
    text: String,
    missing_tail: bool,
}

impl Item {
    fn reported(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            missing_tail: false,
        }
    }

    fn width(&self) -> usize {
        self.text.width() + usize::from(self.missing_tail)
    }
}

/// The stage rows' shared columns: the label column's width and the frame's text width.
struct Layout {
    label_width: usize,
    content: usize,
}

impl Layout {
    /// The column the detail starts at: the two-cell indent, the label column, the duration
    /// column and the gap between them.
    fn detail_col(&self) -> usize {
        2 + self.label_width + DURATION_WIDTH + DETAIL_GAP
    }
}

/// The longest label on the page, capped at [`MAX_LABEL_WIDTH`].
fn label_width(metrics: &TaskMetrics) -> usize {
    metrics
        .attempts
        .iter()
        .flat_map(attempt_labels)
        .map(|label| label.width())
        .max()
        .unwrap_or(0)
        .min(MAX_LABEL_WIDTH)
}

/// Every stage label the page draws for one attempt, in row order.
fn attempt_labels(attempt: &AttemptMetrics) -> Vec<String> {
    let mut labels = vec!["worker".to_owned()];
    for run in &attempt.reviews {
        labels.push(
            run.run
                .map_or_else(|| "review".to_owned(), |number| format!("review {number}")),
        );
    }
    for command in &attempt.commands {
        labels.push(cut_label(&command_label(&command.line), MAX_LABEL_WIDTH));
    }
    if attempt.integration_wall_secs.is_some() {
        labels.push("integration".to_owned());
    }
    if attempt.review_wall_secs.is_some() {
        labels.push("review".to_owned());
    }
    if attempt.diff.is_some() {
        labels.push("diff".to_owned());
    }
    if attempt.increment.is_some() {
        labels.push("increment".to_owned());
    }
    labels
}

/// `attempt 2` with `queue 3s` at the frame's right edge.
fn attempt_header(attempt: &AttemptMetrics, content: usize, theme: &Theme) -> Line<'static> {
    let title = format!("attempt {}", attempt.attempt);
    let (queue, weak) = match attempt.queue_wait_secs {
        Some(secs) => (format!("queue {}", seconds(secs as f64)), false),
        None => ("queue -".to_owned(), true),
    };
    let padding = content.saturating_sub(2 + title.width() + queue.width());
    Line::from(vec![
        Span::raw("  "),
        Span::styled(title, theme.notice_label.add_modifier(Modifier::BOLD)),
        Span::raw(" ".repeat(padding)),
        Span::styled(queue, cell_style(weak, theme)),
    ])
}

/// Draws one run: two rows when it reported a counter the second row can draw, one when it did
/// not. The classification and the second row come from the same counter list, so a run drawn as a
/// metrics run always has its counter row. Returns the rows its first and last line take, so the
/// selection and its detail land on the right lines.
fn push_run(
    rows: &mut Vec<Line<'static>>,
    run: &RunMetrics,
    label: &str,
    selected: bool,
    layout: &Layout,
    theme: &Theme,
) -> (usize, usize) {
    let first = rows.len();
    let items = run
        .metrics
        .as_ref()
        .map(|metrics| counter_items(metrics, run.wall_secs))
        .unwrap_or_default();
    let Some(metrics) = run.metrics.as_ref().filter(|_| !items.is_empty()) else {
        rows.push(rail(
            theme,
            run_plain_line(run, label, selected, layout, theme),
        ));
        return (first, first);
    };
    rows.push(rail(
        theme,
        run_first_line(run, metrics, label, selected, layout, theme),
    ));
    let mut last = first;
    for line in counter_lines(&items, layout, theme) {
        rows.push(rail(theme, line));
        last = rows.len() - 1;
    }
    (first, last)
}

/// A run that reported metrics, row one: the label, the wall time, the route and the cost.
fn run_first_line(
    run: &RunMetrics,
    metrics: &Value,
    label: &str,
    selected: bool,
    layout: &Layout,
    theme: &Theme,
) -> Line<'static> {
    let mut spans = vec![
        marker(selected, theme),
        Span::styled(
            pad_label(label, layout.label_width),
            if selected {
                theme.notice_label.add_modifier(Modifier::BOLD)
            } else {
                theme.notice_label
            },
        ),
        Span::styled(duration_cell(run.wall_secs.map(seconds)), theme.text),
    ];
    let mut detail: Vec<Span<'static>> = Vec::new();
    if let Some(model) = model_text(run) {
        detail.push(Span::styled(model, theme.text));
    }
    if let Some(cost) = counter(Some(metrics), "fee_micros").map(format_cost) {
        if !detail.is_empty() {
            detail.push(Span::styled(" · ", theme.hint));
        }
        detail.push(Span::styled(cost, theme.text));
    }
    if !detail.is_empty() {
        spans.push(Span::raw(" ".repeat(DETAIL_GAP)));
        spans.extend(detail);
    }
    Line::from(fit_spans(spans, layout.content))
}

/// A run that reported nothing: the label, the wall time when known and one weak detail.
fn run_plain_line(
    run: &RunMetrics,
    label: &str,
    selected: bool,
    layout: &Layout,
    theme: &Theme,
) -> Line<'static> {
    let spans = vec![
        marker(selected, theme),
        Span::styled(
            pad_label(label, layout.label_width),
            if selected {
                theme.notice_label.add_modifier(Modifier::BOLD)
            } else {
                theme.notice_label
            },
        ),
        Span::styled(duration_cell(run.wall_secs.map(seconds)), theme.text),
        Span::raw(" ".repeat(DETAIL_GAP)),
        Span::styled("no token metrics recorded", theme.hint),
    ];
    Line::from(fit_spans(spans, layout.content))
}

fn marker(selected: bool, theme: &Theme) -> Span<'static> {
    if selected {
        Span::styled("❯ ", theme.model)
    } else {
        Span::raw("  ")
    }
}

/// A duration right-aligned in the duration column, cut to it when it is longer (`1000h00m`).
/// The column stays exactly [`DURATION_WIDTH`] cells, so the detail column never shifts.
fn duration_cell(duration: Option<String>) -> String {
    let text = duration.map_or_else(String::new, |text| cut(&text, DURATION_WIDTH));
    format!("{text:>width$}", width = DURATION_WIDTH)
}

/// The run's second row: every counter it reported, each with its label. A counter the run did
/// not report is left out; the unresolved context window keeps a weak `-`. This list also decides
/// whether the run takes a second row at all: an empty list draws the one plain row.
fn counter_items(metrics: &Value, wall_secs: Option<f64>) -> Vec<Item> {
    let metrics = Some(metrics);
    let mut items = Vec::new();
    if let Some(requests) = counter(metrics, "requests") {
        items.push(Item::reported(format!("{requests} req")));
    }
    if let Some(input) = counter(metrics, "input_uncached") {
        items.push(Item::reported(format!("in {}", format_tokens(input))));
    }
    if let Some(read) = counter(metrics, "cache_read") {
        let mut text = format!("cache {}", format_tokens(read));
        if let Some(rate) = hit_rate(metrics) {
            text.push(' ');
            text.push_str(&rate);
        }
        items.push(Item::reported(text));
    }
    if let Some(output) = counter(metrics, "output") {
        items.push(Item::reported(format!("out {}", format_tokens(output))));
    }
    if let Some(reasoning) = counter(metrics, "reasoning") {
        items.push(Item::reported(format!(
            "think {}",
            format_tokens(reasoning)
        )));
    }
    if let Some(peak) = counter(metrics, "peak_context") {
        match counter(metrics, "context_window") {
            Some(window) => items.push(Item::reported(format!(
                "ctx {}/{}",
                format_tokens(peak),
                format_tokens(window)
            ))),
            None => items.push(Item {
                text: format!("ctx {}/", format_tokens(peak)),
                missing_tail: true,
            }),
        }
    }
    if let Some(count) = counter(metrics, "compactions") {
        items.push(compaction_item(count, metrics, wall_secs));
    }
    for (name, label) in [
        ("max_tokens_responses", "truncated"),
        ("model_errors", "errors"),
        ("retries", "retries"),
    ] {
        if let Some(count) = counter(metrics, name).filter(|count| *count > 0) {
            items.push(Item::reported(format!("{label} {count}")));
        }
    }
    items
}

/// `compactions 1 (15s, 25%)`: the count, the summed summary time and its share of the run's wall
/// time. A run with no compaction draws its count and no time.
fn compaction_item(count: u64, metrics: Option<&Value>, wall_secs: Option<f64>) -> Item {
    let mut text = format!("compactions {count}");
    if count > 0
        && let Some(millis) = counter(metrics, "compaction_ms")
    {
        let secs = millis as f64 / 1_000.0;
        text.push_str(" (");
        text.push_str(&seconds(secs));
        if let Some(wall) = wall_secs.filter(|wall| *wall > 0.0) {
            text.push_str(", ");
            text.push_str(&percent(secs, wall));
        }
        text.push(')');
    }
    Item::reported(text)
}

/// The cache-read share of the prompt: `cache_read / (input_uncached + cache_read +
/// cache_write)`, a whole percent. `None` when no run reported the counts or they sum to zero.
fn hit_rate(metrics: Option<&Value>) -> Option<String> {
    let input = counter(metrics, "input_uncached")?;
    let read = counter(metrics, "cache_read")?;
    let write = counter(metrics, "cache_write").unwrap_or(0);
    let total = input.saturating_add(read).saturating_add(write);
    (total > 0).then(|| percent(read as f64, total as f64))
}

/// Packs the counters into rows at the detail column, wrapping at the frame's width rather than
/// cutting the second row with `…`.
fn counter_lines(items: &[Item], layout: &Layout, theme: &Theme) -> Vec<Line<'static>> {
    let indent = layout.detail_col();
    let available = layout.content.saturating_sub(indent);
    let mut rows: Vec<Vec<&Item>> = Vec::new();
    let mut row: Vec<&Item> = Vec::new();
    let mut used = 0;
    for item in items {
        let width = item.width();
        let added = if row.is_empty() { width } else { 3 + width };
        if !row.is_empty() && used + added > available {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        used += if row.is_empty() { width } else { 3 + width };
        row.push(item);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows.into_iter()
        .map(|row| {
            let mut spans = vec![Span::raw(" ".repeat(indent))];
            for (index, item) in row.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled(" · ", theme.hint));
                }
                spans.push(Span::styled(item.text.clone(), theme.text));
                if item.missing_tail {
                    spans.push(Span::styled("-", theme.hint));
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// The total row over every attempt: `N attempts`, the cost (`total_cost_usd`, rework included),
/// the wall time, the queue wait, the request and token sums, then the runs that reported nothing.
/// A part no record reported is left out.
fn total_line(
    total: &MetricsTotal,
    attempts: usize,
    content: usize,
    theme: &Theme,
) -> Line<'static> {
    let mut parts = vec![if attempts == 1 {
        "1 attempt".to_owned()
    } else {
        format!("{attempts} attempts")
    }];
    if let Some(cost) = total.total_cost_usd {
        parts.push(cost_usd(cost));
    }
    if let Some(wall) = total.wall_secs {
        parts.push(format!("wall {}", seconds(wall)));
    }
    if let Some(queue) = total.queue_wait_secs {
        parts.push(format!("queue {}", seconds(queue as f64)));
    }
    if let Some(requests) = total.requests {
        parts.push(format!("{requests} req"));
    }
    if let Some(input) = total.input_uncached {
        parts.push(format!("in {}", format_tokens(input)));
    }
    if let Some(cache) = total.cache_read {
        parts.push(format!("cache {}", format_tokens(cache)));
    }
    if let Some(output) = total.output {
        parts.push(format!("out {}", format_tokens(output)));
    }
    if let Some(reasoning) = total.reasoning {
        parts.push(format!("think {}", format_tokens(reasoning)));
    }
    let mut spans = vec![
        Span::raw("  "),
        Span::styled("total", theme.notice_label.add_modifier(Modifier::BOLD)),
    ];
    for (index, part) in parts.into_iter().enumerate() {
        if index == 0 {
            spans.push(Span::raw("  "));
        } else {
            spans.push(Span::styled(" · ", theme.hint));
        }
        spans.push(Span::styled(part, theme.text));
    }
    if total.unreported_runs > 0 {
        spans.push(Span::styled(" · ", theme.hint));
        spans.push(Span::styled(
            format!("{} runs predate metrics", total.unreported_runs),
            theme.hint,
        ));
    }
    Line::from(fit_spans(spans, content))
}

/// The attempt's gate rows, in order: every command with its label and line, `integration` and
/// `review` when they recorded a duration (the review's detail is its findings), then `diff` and
/// `increment` when reported.
fn push_attempt_rows(
    rows: &mut Vec<Line<'static>>,
    attempt: &AttemptMetrics,
    attempts: &[AttemptView],
    layout: &Layout,
    theme: &Theme,
) {
    for command in &attempt.commands {
        let label = command_label(&command.line);
        let detail = strip_assignments(&command.line);
        rows.push(rail(
            theme,
            stage_line(
                &label,
                Some(seconds(command.duration_ms as f64 / 1_000.0)),
                detail,
                layout,
                theme,
            ),
        ));
    }
    if let Some(wall) = attempt.integration_wall_secs {
        rows.push(rail(
            theme,
            stage_line("integration", Some(seconds(wall)), "", layout, theme),
        ));
    }
    if let Some(wall) = attempt.review_wall_secs {
        let detail = review_findings(attempts, attempt.attempt)
            .map(findings_text)
            .unwrap_or_default();
        rows.push(rail(
            theme,
            stage_line("review", Some(seconds(wall)), &detail, layout, theme),
        ));
    }
    if let Some(diff) = attempt.diff {
        rows.push(rail(
            theme,
            stage_line("diff", None, &diff_text(diff), layout, theme),
        ));
    }
    if let Some(increment) = attempt.increment {
        rows.push(rail(
            theme,
            stage_line("increment", None, &diff_text(increment), layout, theme),
        ));
    }
}

/// One stage row: the label column, the duration column and the detail, cut to the frame.
fn stage_line(
    label: &str,
    duration: Option<String>,
    detail: &str,
    layout: &Layout,
    theme: &Theme,
) -> Line<'static> {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(pad_label(label, layout.label_width), theme.notice_label),
        Span::styled(duration_cell(duration), theme.text),
    ];
    if !detail.is_empty() {
        spans.push(Span::raw(" ".repeat(DETAIL_GAP)));
        spans.push(Span::styled(detail.to_owned(), theme.text));
    }
    Line::from(fit_spans(spans, layout.content))
}

/// A label cut to the label column and padded to it.
fn pad_label(label: &str, width: usize) -> String {
    let label = cut_label(label, width);
    let padding = width.saturating_sub(label.width());
    format!("{label}{}", " ".repeat(padding))
}

/// Cuts `label` to `width` columns. A label made of words drops whole words from its end before
/// it cuts a single word hard with `…`.
fn cut_label(label: &str, width: usize) -> String {
    if label.width() <= width {
        return label.to_owned();
    }
    let mut end = label.len();
    while let Some(space) = label[..end].rfind(' ') {
        end = space;
        let candidate = &label[..end];
        if candidate.width() <= width {
            return candidate.to_owned();
        }
    }
    cut(label, width)
}

/// Cuts `text` to `width` cells, ending in `…` when it was longer.
fn cut(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cell = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cell > width - 1 {
            break;
        }
        out.push(ch);
        used += cell;
    }
    out.push('…');
    out
}

/// A command's label: its line's first word (the basename) plus its first argument when that does
/// not start with `-`, after leading `NAME=value` assignments are stripped. A line built from a
/// shell construct is labelled `sh`. The label is not made unique: two commands with the same
/// label keep both rows.
fn command_label(line: &str) -> String {
    let rest = strip_assignments(line);
    if is_shell(rest) {
        return "sh".to_owned();
    }
    let mut words = rest.split_whitespace();
    let Some(first) = words.next() else {
        return "sh".to_owned();
    };
    let base = Path::new(first)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(first);
    let mut label = base.to_owned();
    if let Some(argument) = words.next()
        && !argument.starts_with('-')
    {
        label.push(' ');
        label.push_str(argument);
    }
    label
}

/// The line without its leading `NAME=value` environment assignments.
fn strip_assignments(line: &str) -> &str {
    let mut rest = line.trim_start();
    while let Some(equals) = rest.find('=') {
        let name = &rest[..equals];
        let valid = !name.is_empty()
            && name
                .chars()
                .next()
                .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        if !valid {
            break;
        }
        let value_end = rest[equals..]
            .find(char::is_whitespace)
            .map_or(rest.len(), |offset| equals + offset);
        rest = rest[value_end..].trim_start();
    }
    rest
}

/// Whether the line is a shell construct: a `set`/`if`/`for` statement, a subshell, or a first
/// word that carries `;`, `&&` or `|`.
fn is_shell(rest: &str) -> bool {
    if rest.starts_with("set ")
        || rest.starts_with("if ")
        || rest.starts_with("for ")
        || rest.starts_with('(')
    {
        return true;
    }
    rest.split_whitespace()
        .next()
        .is_some_and(|word| word.contains(';') || word.contains("&&") || word.contains('|'))
}

/// The selected run's detail: the tool table (by calls) and the request log.
fn detail_rows(run: &RunMetrics, content: usize, theme: &Theme) -> Vec<Line<'static>> {
    let metrics = run.metrics.as_ref();
    let mut rows = vec![plain("  tools", theme.hint, content)];
    match metrics
        .and_then(|metrics| metrics.get("tools"))
        .and_then(Value::as_object)
    {
        Some(tools) if !tools.is_empty() => {
            rows.push(cells_line(
                "  ",
                vec![
                    Cell::header("tool"),
                    Cell::header("calls"),
                    Cell::header("bytes"),
                    Cell::header("failed"),
                    Cell::header("denied"),
                ],
                content,
                theme,
            ));
            let mut entries: Vec<(&String, &Value)> = tools.iter().collect();
            entries.sort_by(|(one, left), (other, right)| {
                calls(right).cmp(&calls(left)).then_with(|| one.cmp(other))
            });
            for (name, tool) in entries {
                rows.push(cells_line(
                    "  ",
                    vec![
                        Cell::reported(name.clone()),
                        Cell::optional(calls(tool).map(|count| count.to_string())),
                        Cell::optional(
                            tool.get("result_bytes")
                                .and_then(Value::as_u64)
                                .map(format_tokens),
                        ),
                        Cell::optional(
                            tool.get("failed")
                                .and_then(Value::as_u64)
                                .map(|count| count.to_string()),
                        ),
                        Cell::optional(
                            tool.get("denied")
                                .and_then(Value::as_u64)
                                .map(|count| count.to_string()),
                        ),
                    ],
                    content,
                    theme,
                ));
            }
        }
        _ => rows.push(plain("  -", theme.hint, content)),
    }
    rows.push(plain("  requests", theme.hint, content));
    match metrics
        .and_then(|metrics| metrics.get("request_log"))
        .and_then(Value::as_array)
    {
        Some(log) if !log.is_empty() => {
            rows.push(cells_line(
                "  ",
                vec![
                    Cell::header("time"),
                    Cell::header("latency"),
                    Cell::header("kind"),
                    Cell::header("in"),
                    Cell::header("cache"),
                    Cell::header("out"),
                    Cell::header("think"),
                    Cell::header("fee"),
                    Cell::header("stop"),
                    Cell::header("model"),
                ],
                content,
                theme,
            ));
            for request in log {
                rows.push(cells_line(
                    "  ",
                    vec![
                        Cell::optional(string(request, "ts")),
                        Cell::optional(
                            request
                                .get("latency_ms")
                                .and_then(Value::as_u64)
                                .map(|ms| format!("{ms}ms")),
                        ),
                        Cell::optional(string(request, "kind")),
                        Cell::optional(count(request, "input_uncached").map(format_tokens)),
                        Cell::optional(count(request, "cache_read").map(format_tokens)),
                        Cell::optional(count(request, "output").map(format_tokens)),
                        Cell::optional(count(request, "reasoning").map(format_tokens)),
                        Cell::optional(
                            request
                                .get("fee_micros")
                                .and_then(Value::as_u64)
                                .map(format_cost),
                        ),
                        Cell::optional(string(request, "stop_reason")),
                        Cell::optional(string(request, "model")),
                    ],
                    content,
                    theme,
                ));
            }
        }
        _ => rows.push(plain("  -", theme.hint, content)),
    }
    if metrics
        .and_then(|metrics| metrics.get("request_log_truncated"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        rows.push(plain("  request log truncated", theme.hint, content));
    }
    rows
}

/// A row of `cells`, two spaces apart, its not-reported cells weak.
fn cells_line(indent: &str, cells: Vec<Cell>, content: usize, theme: &Theme) -> Line<'static> {
    let mut spans = vec![Span::raw(indent.to_owned())];
    for (index, cell) in cells.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", theme.hint));
        }
        spans.push(Span::styled(cell.text, cell_style(cell.weak, theme)));
        if cell.missing_tail {
            spans.push(Span::styled("-", theme.hint));
        }
    }
    Line::from(fit_spans(spans, content))
}

fn plain(text: impl Into<String>, style: Style, content: usize) -> Line<'static> {
    Line::from(fit_spans(vec![Span::styled(text.into(), style)], content))
}

fn cell_style(weak: bool, theme: &Theme) -> Style {
    if weak { theme.hint } else { theme.text }
}

/// A run's route: `provider/model`, the one that is when only one is known, else `-`.
fn model_text(run: &RunMetrics) -> Option<String> {
    match (&run.provider, &run.model) {
        (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
        (Some(provider), None) => Some(provider.clone()),
        (None, Some(model)) => Some(model.clone()),
        (None, None) => None,
    }
}

fn counter(metrics: Option<&Value>, name: &str) -> Option<u64> {
    metrics?.get(name)?.as_u64()
}

fn count(value: &Value, name: &str) -> Option<u64> {
    value.get(name)?.as_u64()
}

fn string(value: &Value, name: &str) -> Option<String> {
    value.get(name)?.as_str().map(str::to_owned)
}

fn calls(tool: &Value) -> Option<u64> {
    tool.get("calls").and_then(Value::as_u64)
}

/// `seconds` as the frame's whole-second duration: `15s`, `1m00s`.
fn seconds(secs: f64) -> String {
    format_seconds(Duration::from_secs_f64(secs.max(0.0)))
}

fn cost_usd(usd: f64) -> String {
    format_cost((usd * 1_000_000.0).round() as u64)
}

/// A whole percent, no decimals.
fn percent(part: f64, total: f64) -> String {
    format!("{}%", (part / total * 100.0).round() as u64)
}

/// `3 files +40 −12`.
fn diff_text(stat: radian_arc::read::DiffStatView) -> String {
    let files = if stat.files == 1 {
        "1 file".to_owned()
    } else {
        format!("{} files", stat.files)
    };
    format!("{files} +{} −{}", stat.added, stat.removed)
}

/// The findings of the attempt's `review` verdict. The read side already maps an old verdict's
/// `blocking` / `suggestions` shape to findings, so the counts go through the mapping the
/// attempts tab draws.
fn review_findings(attempts: &[AttemptView], attempt: u32) -> Option<&[Finding]> {
    let attempt = attempts.iter().find(|view| view.attempt == attempt)?;
    let review = attempt
        .verdicts
        .iter()
        .find(|verdict| verdict.stage == "review")?;
    Some(&review.findings)
}

/// `1 major · 1 minor · 2 pre-existing`: the severities above zero, highest first, then the
/// demoted findings. `no findings` when the review recorded findings and every count is zero.
fn findings_text(findings: &[Finding]) -> String {
    let count = |severity: &str| {
        findings
            .iter()
            .filter(|finding| finding.severity == severity)
            .count()
    };
    let pre_existing = findings
        .iter()
        .filter(|finding| finding.pre_existing)
        .count();
    findings_counts_text(
        count("critical"),
        count("major"),
        count("minor"),
        pre_existing,
    )
}

/// [`findings_text`] from the four counts: only the ones above zero, highest severity first.
fn findings_counts_text(
    critical: usize,
    major: usize,
    minor: usize,
    pre_existing: usize,
) -> String {
    let mut parts = Vec::new();
    for (severity, count) in [("critical", critical), ("major", major), ("minor", minor)] {
        if count > 0 {
            parts.push(format!("{count} {severity}"));
        }
    }
    if pre_existing > 0 {
        parts.push(format!("{pre_existing} pre-existing"));
    }
    if parts.is_empty() {
        "no findings".to_owned()
    } else {
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_hit_rate_is_a_whole_percent_over_every_prompt_token() {
        let metrics = json!({"input_uncached": 25, "cache_read": 50, "cache_write": 25});
        assert_eq!(hit_rate(Some(&metrics)).as_deref(), Some("50%"));

        let no_write = json!({"input_uncached": 1, "cache_read": 3});
        assert_eq!(hit_rate(Some(&no_write)).as_deref(), Some("75%"));

        let unread = json!({"input_uncached": 4});
        assert_eq!(hit_rate(Some(&unread)), None);
        let zero = json!({"input_uncached": 0, "cache_read": 0, "cache_write": 0});
        assert_eq!(hit_rate(Some(&zero)), None);
    }

    #[test]
    fn a_compaction_draws_its_share_only_with_a_wall_time() {
        let one = json!({"compactions": 1, "compaction_ms": 15_000});
        assert_eq!(
            compaction_item(1, Some(&one), Some(60.0)).text,
            "compactions 1 (15s, 25%)"
        );
        assert_eq!(
            compaction_item(1, Some(&one), None).text,
            "compactions 1 (15s)"
        );
        let none = json!({"compactions": 0});
        assert_eq!(
            compaction_item(0, Some(&none), Some(60.0)).text,
            "compactions 0"
        );
    }

    /// The duration column is exactly seven cells: a longer duration is cut rather than pushing
    /// the detail column right.
    #[test]
    fn a_duration_longer_than_the_column_is_cut_to_it() {
        let cell = duration_cell(Some("1000h00m".to_owned()));
        assert_eq!(cell.width(), DURATION_WIDTH, "{cell:?}");
        assert!(cell.ends_with('…'), "{cell:?}");

        let layout = Layout {
            label_width: 6,
            content: 120,
        };
        let line = stage_line(
            "worker",
            Some("1000h00m".to_owned()),
            "detail",
            &layout,
            &Theme::default(),
        );
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        let detail = text.find("detail").expect("the detail is drawn");
        assert_eq!(
            text[..detail].width(),
            layout.detail_col(),
            "the detail stays in its column: {text:?}"
        );
    }

    #[test]
    fn a_run_without_metrics_draws_one_weak_line() {
        let run = RunMetrics {
            run: None,
            session_id: None,
            started_at: None,
            ended_at: None,
            wall_secs: Some(61.0),
            provider: None,
            model: None,
            metrics: None,
        };
        let layout = Layout {
            label_width: 6,
            content: 200,
        };
        let line = run_plain_line(&run, "worker", false, &layout, &Theme::default());
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("worker"), "{text}");
        assert!(text.contains("1m01s"), "{text}");
        assert!(text.contains("no token metrics recorded"), "{text}");
        assert!(!text.contains("model -"), "{text}");
        assert_eq!(
            line.spans.last().map(|span| span.style.fg),
            Some(Theme::default().hint.fg),
            "the detail is weak: {text}"
        );
    }

    #[test]
    fn a_command_label_follows_the_first_word_and_its_argument() {
        assert_eq!(command_label("cargo fmt --all -- --check"), "cargo fmt");
        assert_eq!(
            command_label("CARGO_TARGET_DIR=/x cargo clippy --workspace"),
            "cargo clippy"
        );
        assert_eq!(
            command_label("set -e; base=$(git merge-base HEAD origin/main); if …"),
            "sh"
        );
        assert_eq!(
            command_label("/Users/x/.local/bin/arc-gate-lxc crates"),
            "arc-gate-lxc crates"
        );
        assert_eq!(command_label("make"), "make");
        assert_eq!(command_label("A=1 B=2 ./run.sh -v"), "run.sh");
    }

    #[test]
    fn findings_text_names_only_the_counts_above_zero() {
        assert_eq!(findings_counts_text(0, 1, 1, 0), "1 major · 1 minor");
        assert_eq!(
            findings_counts_text(1, 2, 3, 4),
            "1 critical · 2 major · 3 minor · 4 pre-existing"
        );
        assert_eq!(findings_counts_text(0, 0, 0, 2), "2 pre-existing");
        assert_eq!(findings_counts_text(0, 0, 0, 0), "no findings");
    }

    #[test]
    fn a_long_label_is_cut_to_the_label_column() {
        assert_eq!(
            cut_label("arc-gate-lxc crates", MAX_LABEL_WIDTH),
            "arc-gate-lxc"
        );
        assert_eq!(
            cut_label("command-name-here arg", MAX_LABEL_WIDTH),
            "command-name-he…"
        );
        assert_eq!(cut_label("cargo clippy", MAX_LABEL_WIDTH), "cargo clippy");
    }

    #[test]
    fn a_review_row_without_a_verdict_has_no_findings_detail() {
        let attempt = AttemptMetrics {
            attempt: 1,
            attempt_id: "t-1".to_owned(),
            worker: RunMetrics {
                run: None,
                session_id: None,
                started_at: None,
                ended_at: None,
                wall_secs: None,
                provider: None,
                model: None,
                metrics: None,
            },
            reviews: Vec::new(),
            review_wall_secs: Some(60.0),
            commands: Vec::new(),
            integration_wall_secs: None,
            diff: None,
            increment: None,
            cost_usd: None,
            review_cost_usd: None,
            queue_wait_secs: None,
        };
        let layout = Layout {
            label_width: 6,
            content: 200,
        };
        let mut rows = Vec::new();
        push_attempt_rows(&mut rows, &attempt, &[], &layout, &Theme::default());
        assert_eq!(rows.len(), 1, "only the review stage recorded a duration");
        let text: String = rows[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("review"), "{text}");
        assert!(!text.contains("no findings"), "{text}");
    }
}
