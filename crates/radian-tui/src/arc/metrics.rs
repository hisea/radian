//! The task page's metrics tab (ADR 0048 §11 and §13): what each attempt spent.
//!
//! One attempt header per attempt, newest first, then one row per run (`worker`, `review 1`,
//! `review 2`), then the attempt's gate times, diff and review findings; one total row over
//! every attempt ends the list. `j`/`k` move the `❯` over the run rows, `Enter` opens the
//! selected run's tool table and request log below it, and `Esc` closes that detail before it
//! leaves the page. A value no record reported draws `-` in the weak style.
//!
//! Numbers use [`crate::timing`]'s compact forms, the same the footer's `tokens` item draws.
//! Nothing here reads a file: the page state loaded the metrics through `radian_arc::read`.

use std::time::Duration;

use radian_arc::gate::Finding;
use radian_arc::read::{AttemptMetrics, AttemptView, MetricsTotal, RunMetrics, TaskMetrics};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::TaskPage;
use super::render::{fit_spans, rail};
use crate::theme::Theme;
use crate::timing::{format_cost, format_seconds, format_tokens};

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
        let mut rows: Vec<Line<'static>> = Vec::new();
        let mut run_rows: Vec<usize> = Vec::with_capacity(run_count(metrics));
        let mut index = 0;
        for attempt in metrics.attempts.iter().rev() {
            rows.push(rail(theme, attempt_header(attempt, content, theme)));
            run_rows.push(rows.len());
            rows.push(rail(
                theme,
                run_line(
                    &attempt.worker,
                    "worker",
                    index == page.metric_sel,
                    content,
                    theme,
                ),
            ));
            index += 1;
            for run in &attempt.reviews {
                let label = run
                    .run
                    .map_or_else(|| "review".to_owned(), |number| format!("review {number}"));
                run_rows.push(rows.len());
                rows.push(rail(
                    theme,
                    run_line(run, &label, index == page.metric_sel, content, theme),
                ));
                index += 1;
            }
            push_attempt_rows(&mut rows, attempt, &page.attempts, content, theme);
        }
        rows.push(rail(theme, total_line(&metrics.total, content, theme)));
        // The selected run's detail sits directly below its row.
        if page.metric_open
            && let Some((attempt, run)) = run_at(metrics, page.metric_sel)
            && let Some(position) = run_rows.get(page.metric_sel)
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

/// One styled piece of a metrics row. A `weak` piece is a value no record reported.
struct Cell {
    text: String,
    weak: bool,
}

impl Cell {
    fn reported(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            weak: false,
        }
    }

    fn missing() -> Self {
        Self {
            text: "-".to_owned(),
            weak: true,
        }
    }

    fn header(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            weak: true,
        }
    }

    fn optional(value: Option<String>) -> Self {
        value.map_or_else(Self::missing, Self::reported)
    }
}

/// `attempt 2 · queue 3s`.
fn attempt_header(attempt: &AttemptMetrics, content: usize, theme: &Theme) -> Line<'static> {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            format!("attempt {}", attempt.attempt),
            theme.notice_label.add_modifier(Modifier::BOLD),
        ),
    ];
    let mut parts = vec![cell_part(
        "queue",
        attempt.queue_wait_secs.map(|secs| seconds(secs as f64)),
    )];
    push_parts(&mut spans, &mut parts, theme);
    Line::from(fit_spans(spans, content))
}

/// One run's row: the route, the token counts and hit rate, the cost, wall time, peak context,
/// compactions and the anomaly counts that are above zero.
fn run_line(
    run: &RunMetrics,
    label: &str,
    selected: bool,
    content: usize,
    theme: &Theme,
) -> Line<'static> {
    let metrics = run.metrics.as_ref();
    let mut spans = vec![
        if selected {
            Span::styled("❯ ", theme.model)
        } else {
            Span::raw("  ")
        },
        Span::styled(
            label.to_owned(),
            if selected {
                theme.notice_label.add_modifier(Modifier::BOLD)
            } else {
                theme.notice_label
            },
        ),
    ];
    let mut parts = vec![
        cell_part("model", model_text(run)),
        cell_part(
            "req",
            counter(metrics, "requests").map(|value| value.to_string()),
        ),
        cell_part("in", counter(metrics, "input_uncached").map(format_tokens)),
        cache_part(metrics),
        cell_part("out", counter(metrics, "output").map(format_tokens)),
        cell_part("think", counter(metrics, "reasoning").map(format_tokens)),
        cell_part("cost", counter(metrics, "fee_micros").map(format_cost)),
        cell_part("wall", run.wall_secs.map(seconds)),
        context_part(metrics),
        compaction_part(metrics, run.wall_secs),
    ];
    if let Some(count) = counter(metrics, "max_tokens_responses").filter(|count| *count > 0) {
        parts.push(Cell::reported(format!("truncated {count}")));
    }
    if let Some(count) = counter(metrics, "model_errors").filter(|count| *count > 0) {
        parts.push(Cell::reported(format!("errors {count}")));
    }
    if let Some(count) = counter(metrics, "retries").filter(|count| *count > 0) {
        parts.push(Cell::reported(format!("retries {count}")));
    }
    push_parts(&mut spans, &mut parts, theme);
    Line::from(fit_spans(spans, content))
}

/// `cache 4K 29%`: the cache-read tokens and their share of the whole prompt, a whole percent.
fn cache_part(metrics: Option<&Value>) -> Cell {
    let Some(read) = counter(metrics, "cache_read") else {
        return Cell::missing();
    };
    let mut text = format_tokens(read);
    if let Some(rate) = hit_rate(metrics) {
        text.push(' ');
        text.push_str(&rate);
    }
    Cell::reported(format!("cache {text}"))
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

/// `ctx 83K/1.0M`: the peak context against the window the run resolved.
fn context_part(metrics: Option<&Value>) -> Cell {
    let Some(peak) = counter(metrics, "peak_context") else {
        return Cell::missing();
    };
    match counter(metrics, "context_window") {
        Some(window) => Cell::reported(format!(
            "ctx {}/{}",
            format_tokens(peak),
            format_tokens(window)
        )),
        None => Cell::reported(format!("ctx {}/-", format_tokens(peak))),
    }
}

/// `compactions 1 15s 25%`: the count, the summed summary time and its share of the run's wall
/// time. A run with no compaction draws its count and no time.
fn compaction_part(metrics: Option<&Value>, wall_secs: Option<f64>) -> Cell {
    let Some(count) = counter(metrics, "compactions") else {
        return Cell::missing();
    };
    let mut text = count.to_string();
    if count > 0
        && let Some(millis) = counter(metrics, "compaction_ms")
    {
        let secs = millis as f64 / 1_000.0;
        text.push(' ');
        text.push_str(&seconds(secs));
        if let Some(wall) = wall_secs
            && wall > 0.0
        {
            text.push(' ');
            text.push_str(&percent(secs, wall));
        }
    }
    Cell::reported(format!("compactions {text}"))
}

/// The total row over every attempt: the reported requests, the token sums, `total_cost_usd`
/// (rework included), the wall time, the queue wait, and the runs that reported nothing.
fn total_line(total: &MetricsTotal, content: usize, theme: &Theme) -> Line<'static> {
    let mut spans = vec![
        Span::raw("  "),
        Span::styled("total", theme.notice_label.add_modifier(Modifier::BOLD)),
    ];
    let mut parts = vec![
        cell_part("requests", total.requests.map(|value| value.to_string())),
        cell_part("in", total.input_uncached.map(format_tokens)),
        cell_part("cache", total.cache_read.map(format_tokens)),
        cell_part("out", total.output.map(format_tokens)),
        cell_part("think", total.reasoning.map(format_tokens)),
        cell_part("cost", total.total_cost_usd.map(cost_usd)),
        cell_part("wall", total.wall_secs.map(seconds)),
        cell_part(
            "queue",
            total.queue_wait_secs.map(|secs| seconds(secs as f64)),
        ),
    ];
    if total.unreported_runs > 0 {
        parts.push(Cell::reported(format!(
            "{} runs not reported",
            total.unreported_runs
        )));
    }
    push_parts(&mut spans, &mut parts, theme);
    Line::from(fit_spans(spans, content))
}

/// The attempt's gate rows: every command's time with its line, the integration and review
/// stage times, the diff and increment, and the review's findings by severity.
fn push_attempt_rows(
    rows: &mut Vec<Line<'static>>,
    attempt: &AttemptMetrics,
    attempts: &[AttemptView],
    content: usize,
    theme: &Theme,
) {
    for command in &attempt.commands {
        let text = format!(
            "  command {} {}",
            seconds(command.duration_ms as f64 / 1_000.0),
            command.line
        );
        rows.push(rail(
            theme,
            Line::from(fit_spans(vec![Span::styled(text, theme.text)], content)),
        ));
    }
    rows.push(rail(
        theme,
        labeled_line(
            "integration",
            attempt.integration_wall_secs.map(seconds),
            content,
            theme,
        ),
    ));
    rows.push(rail(
        theme,
        labeled_line(
            "review",
            attempt.review_wall_secs.map(seconds),
            content,
            theme,
        ),
    ));
    rows.push(rail(
        theme,
        labeled_line("diff", attempt.diff.map(diff_text), content, theme),
    ));
    rows.push(rail(
        theme,
        labeled_line(
            "increment",
            attempt.increment.map(diff_text),
            content,
            theme,
        ),
    ));
    let findings = review_findings(attempts, attempt.attempt);
    rows.push(rail(
        theme,
        labeled_line("findings", findings.map(findings_text), content, theme),
    ));
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
                        Cell::reported(calls(tool).to_string()),
                        Cell::reported(format_tokens(
                            tool.get("result_bytes")
                                .and_then(Value::as_u64)
                                .unwrap_or(0),
                        )),
                        Cell::reported(
                            tool.get("failed")
                                .and_then(Value::as_u64)
                                .unwrap_or(0)
                                .to_string(),
                        ),
                        Cell::reported(
                            tool.get("denied")
                                .and_then(Value::as_u64)
                                .unwrap_or(0)
                                .to_string(),
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
    }
    Line::from(fit_spans(spans, content))
}

/// `label value` after the row's two-cell indent; a missing value is weak.
fn labeled_line(
    label: &str,
    value: Option<String>,
    content: usize,
    theme: &Theme,
) -> Line<'static> {
    let weak = value.is_none();
    let text = format!("  {label} {}", value.unwrap_or_else(|| "-".to_owned()));
    plain(text, cell_style(weak, theme), content)
}

fn plain(text: impl Into<String>, style: Style, content: usize) -> Line<'static> {
    Line::from(fit_spans(vec![Span::styled(text.into(), style)], content))
}

fn cell_style(weak: bool, theme: &Theme) -> Style {
    if weak { theme.hint } else { theme.text }
}

fn push_parts(spans: &mut Vec<Span<'static>>, parts: &mut Vec<Cell>, theme: &Theme) {
    for cell in parts.drain(..) {
        spans.push(Span::styled(" · ", theme.hint));
        spans.push(Span::styled(cell.text, cell_style(cell.weak, theme)));
    }
}

fn cell_part(label: &str, value: Option<String>) -> Cell {
    match value {
        Some(value) => Cell::reported(format!("{label} {value}")),
        None => Cell {
            text: format!("{label} -"),
            weak: true,
        },
    }
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

fn calls(tool: &Value) -> u64 {
    tool.get("calls").and_then(Value::as_u64).unwrap_or(0)
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

/// `critical 1 · major 2 · minor 0 · pre-existing 1`.
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
    format!(
        "critical {} · major {} · minor {} · pre-existing {pre_existing}",
        count("critical"),
        count("major"),
        count("minor")
    )
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
            compaction_part(Some(&one), Some(60.0)).text,
            "compactions 1 15s 25%"
        );
        assert_eq!(compaction_part(Some(&one), None).text, "compactions 1 15s");
        let none = json!({"compactions": 0});
        assert_eq!(
            compaction_part(Some(&none), Some(60.0)).text,
            "compactions 0"
        );
        assert!(compaction_part(None, None).weak);
    }

    #[test]
    fn an_unreported_metric_is_weak() {
        let line = run_line(
            &RunMetrics {
                run: None,
                session_id: None,
                started_at: None,
                ended_at: None,
                wall_secs: Some(61.0),
                provider: None,
                model: None,
                metrics: None,
            },
            "worker",
            false,
            200,
            &Theme::default(),
        );
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("model -"), "{text}");
        assert!(text.contains("wall 1m01s"), "{text}");
    }
}
