//! The `/btw` side-question overlay (backlog `tui-btw-side-question`): one
//! [`Overlay::Btw`] layer over the transcript, streaming the side answer. It is
//! ADR 0023 U4's overlay tier — temporary, never above a modal — and it holds no
//! transcript state: the answer lives here (and in this process only), the
//! transcript and journal are never touched.

/// One finished side answer: the question, the streamed text, and the lines the
/// overlay's last row shows. In-memory only; the process exit drops it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtwAnswer {
    pub question: String,
    pub text: String,
    /// The overlay's last line: this call's tokens (and fee when priced).
    pub usage_line: String,
    /// Whether the model returned tool calls (none was executed).
    pub tools_note: bool,
}

/// The overlay's lifecycle: streaming, then done or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BtwState {
    /// The side request is in flight; text streams in.
    Streaming,
    /// The answer finished; the last row shows the usage line.
    Done,
    /// The side request failed; the overlay shows only this.
    Failed(String),
}

/// The `/btw` overlay's state: the question, the streamed answer, the scroll,
/// and whether the request is still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtwOverlay {
    pub question: String,
    pub text: String,
    /// The body's first visible row. The renderer clamps it, like the
    /// approval box's scroll (a key can never park the window past the end).
    pub scroll: usize,
    pub state: BtwState,
    /// Set when the answer finished: the usage line and the tools note.
    pub answer: Option<BtwAnswer>,
}

impl BtwOverlay {
    /// A fresh overlay for a side question that is about to be sent.
    pub fn asking(question: String) -> Self {
        Self {
            question,
            text: String::new(),
            scroll: 0,
            state: BtwState::Streaming,
            answer: None,
        }
    }

    /// Reopens a finished answer: the text and usage line, not streaming.
    pub fn reopen(answer: BtwAnswer) -> Self {
        Self {
            question: answer.question.clone(),
            text: answer.text.clone(),
            scroll: 0,
            state: BtwState::Done,
            answer: Some(answer),
        }
    }

    /// Whether the request behind this overlay is still running.
    pub fn is_streaming(&self) -> bool {
        self.state == BtwState::Streaming
    }

    /// Appends streamed text; a finished request's late delta is dropped.
    pub fn push_text(&mut self, delta: &str) {
        if self.is_streaming() {
            self.text.push_str(delta);
        }
    }

    pub fn scroll_by(&mut self, delta: i32) {
        if delta < 0 {
            self.scroll = self.scroll.saturating_sub((-delta) as usize);
        } else {
            self.scroll = self.scroll.saturating_add(delta as usize);
        }
    }
}

/// The overlay's last line: the usage line, plus the tools note when the model
/// returned tool calls (none was executed).
pub fn last_line(answer: &BtwAnswer) -> String {
    if answer.tools_note {
        format!("{} · {}", answer.usage_line, radian_host::btw::NO_TOOL_LINE)
    } else {
        answer.usage_line.clone()
    }
}

/// The usage line for one finished side call: the provider-reported counters
/// that exist (never a zero invented for a missing one, ADR 0007 J1) and the
/// fee at the route in force (`None` hides the fee, never invents one).
pub fn usage_line(usage: Option<&radian_core::ModelUsage>, fee_micros: Option<u64>) -> String {
    let Some(usage) = usage else {
        return match fee_micros {
            Some(micros) => format!(
                "usage not reported · {}",
                crate::timing::format_cost(micros)
            ),
            None => "usage not reported".to_owned(),
        };
    };
    let mut parts = Vec::new();
    if let Some(input) = usage.prompt_total() {
        parts.push(format!("{input} in"));
    }
    if let Some(output) = usage.output {
        parts.push(format!("{output} out"));
    }
    if let Some(total) = usage.total {
        parts.push(format!("{total} total"));
    }
    if parts.is_empty() {
        parts.push("usage not reported".to_owned());
    }
    if let Some(micros) = fee_micros {
        parts.push(crate::timing::format_cost(micros));
    }
    parts.join(" · ")
}

/// Draws the overlay into `area`: the shared [`crate::panel::Panel`] frame, the
/// question as the title row, the wrapped answer, and the last line (the usage
/// line, or the failure). Streaming shows what has arrived so far.
pub fn draw(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    overlay: &BtwOverlay,
    theme: &crate::theme::Theme,
    glyphs: crate::footer::Glyphs,
) {
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    frame.render_widget(ratatui::widgets::Clear, area);
    let width = area.width as usize;
    let inner = width.saturating_sub(4).max(1);
    let mut panel = crate::panel::Panel::new("btw");
    panel = panel.row(Line::from(vec![
        Span::styled("q: ", theme.hint),
        Span::styled(overlay.question.clone(), theme.text),
    ]));
    panel = panel.divider();
    let mut rows: Vec<String> = crate::render::wrap(&overlay.text, inner);
    if overlay.text.is_empty() && overlay.is_streaming() {
        rows = vec!["…".to_owned()];
    }
    let tail: Option<Line> = match &overlay.state {
        BtwState::Streaming => None,
        BtwState::Done => overlay
            .answer
            .as_ref()
            .map(|answer| Line::from(Span::styled(last_line(answer), theme.hint))),
        BtwState::Failed(error) => Some(Line::from(vec![
            Span::styled("side question failed: ", theme.status_warn),
            Span::styled(error.clone(), theme.text),
        ])),
    };
    // Budget: the area minus the panel's two borders, the question row, the
    // divider and the tail row when shown. The answer window takes what is
    // left, at least one row, so the bottom hints are never clipped.
    let reserved = 4 + usize::from(tail.is_some());
    let budget = (area.height as usize).saturating_sub(reserved).max(1);
    let total = rows.len().max(1);
    let first = overlay.scroll.min(total.saturating_sub(1));
    let last = (first + budget).min(total);
    if rows.is_empty() {
        panel = panel.row(Line::from(Span::styled("(no answer text)", theme.hint)));
    } else {
        for row in &rows[first..last] {
            panel = panel.row(Line::from(Span::styled(row.clone(), theme.text)));
        }
    }
    if total > budget {
        panel = panel.right_title(format!("lines {}–{} of {total}", first + 1, last));
    }
    if let Some(tail) = tail {
        panel = panel.divider();
        panel = panel.row(tail);
    }
    panel = panel.hint("esc", "close");
    if overlay.is_streaming() {
        panel = panel.hint("esc", "cancel");
    } else {
        panel = panel.hint("↑/↓", "scroll");
    }
    frame.render_widget(Paragraph::new(panel.lines(width, theme, glyphs)), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The last line names the usage and the tools note only when the model
    /// returned tool calls.
    #[test]
    fn the_last_line_names_usage_and_the_tools_note() {
        let plain = BtwAnswer {
            question: "q".into(),
            text: "a".into(),
            usage_line: "10 in".into(),
            tools_note: false,
        };
        assert_eq!(last_line(&plain), "10 in");
        let with_tools = BtwAnswer {
            tools_note: true,
            ..plain
        };
        assert!(last_line(&with_tools).contains("10 in"));
        assert!(last_line(&with_tools).contains(radian_host::btw::NO_TOOL_LINE));
    }

    /// The usage line reports only what the provider reported (ADR 0007 J1: no
    /// invented zeros), and the fee only when priced.
    #[test]
    fn the_usage_line_reports_only_reported_counters() {
        assert_eq!(usage_line(None, None), "usage not reported");
        let usage = radian_core::ModelUsage {
            input: Some(10),
            output: Some(3),
            ..Default::default()
        };
        assert_eq!(usage_line(Some(&usage), None), "10 in · 3 out");
        assert_eq!(
            usage_line(Some(&usage), Some(1_000_000)),
            format!("10 in · 3 out · {}", crate::timing::format_cost(1_000_000))
        );
        let empty = radian_core::ModelUsage::default();
        assert_eq!(usage_line(Some(&empty), None), "usage not reported");
    }

    /// A late delta after the answer finished is dropped, never appended.
    #[test]
    fn a_late_delta_after_done_is_dropped() {
        let answer = BtwAnswer {
            question: "q".into(),
            text: "a".into(),
            usage_line: "u".into(),
            tools_note: false,
        };
        let mut overlay = BtwOverlay::reopen(answer);
        overlay.push_text("late");
        assert_eq!(overlay.text, "a");
    }
}
