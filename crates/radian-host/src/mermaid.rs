//! `mermaid_check`: the read-only tool that renders a Mermaid block with the same bounded
//! renderer the TUI uses (ADR 0028 R8's generation side, ADR 0025).
//!
//! It exists because a model cannot see the terminal: before this, a model drawing Radian's
//! architecture spent most of a turn reading the vendored renderer, compiling its example and
//! measuring widths by hand. The check returns what the transcript would show — the rendered
//! preview plus its real size — or the renderer's reason and a one-line fix.
//!
//! Read-only and side-effect free, so it is `allow` in every tier (ADR 0012, like `skill`).

use agent_core::{
    ToolCall, ToolConcurrency, ToolContext, ToolError, ToolFailure, ToolOutput, ToolProvider,
    ToolSession, ToolSpec,
};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

/// The default width when the caller does not give one: a comfortable terminal.
pub const DEFAULT_WIDTH: usize = 100;
/// The widest the caller may ask for. Wider than any real terminal, and still far below the
/// renderer's 65 536-cell canvas cap for any diagram that fits the node/edge limits.
pub const MAX_WIDTH: usize = 500;
/// The preview is bounded: at most this many rendered lines, then a `… N more lines` note. A
/// model needs the shape, not the whole canvas, and the result stays cheap to read.
pub const PREVIEW_LINES: usize = 40;

#[derive(Clone, Copy)]
pub struct MermaidCheckTools;

impl MermaidCheckTools {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MermaidCheckTools {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolProvider for MermaidCheckTools {
    type Session = MermaidCheckSession;

    async fn open_session(&self) -> Result<Self::Session, ToolError> {
        Ok(MermaidCheckSession {
            specs: vec![spec()],
        })
    }
}

pub struct MermaidCheckSession {
    specs: Vec<ToolSpec>,
}

impl ToolSession for MermaidCheckSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        call: &ToolCall,
        _cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        if call.name != "mermaid_check" {
            return Err(ToolFailure::new(
                "unknown_tool",
                format!("`{}` is not the mermaid_check tool", call.name),
            ));
        }
        let source = call
            .arguments
            .get("source")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolFailure::new(
                    "invalid_arguments",
                    "`source` is required and must be a string",
                )
            })?;
        let width = match call.arguments.get("width") {
            None | Some(Value::Null) => DEFAULT_WIDTH,
            Some(Value::Number(number)) => {
                number.as_u64().map(|width| width as usize).ok_or_else(|| {
                    ToolFailure::new("invalid_arguments", "`width` must be a positive integer")
                })?
            }
            Some(_) => {
                return Err(ToolFailure::new(
                    "invalid_arguments",
                    "`width`, when given, must be an integer",
                ));
            }
        };
        // A width of 0 or over the cap is clamped, not an error: the check is advisory and a
        // model that asks for a huge width should still get an answer.
        let width = width.clamp(1, MAX_WIDTH);
        Ok(ToolOutput::text(check(source, width)))
    }
}

/// Renders `source` and describes the result, or returns the renderer's reason as a tool-result
/// error. Pure, so the tests call it directly.
pub fn check(source: &str, width: usize) -> String {
    match codex_mermaid::render(source, width) {
        Ok(text) => {
            let lines: Vec<&str> = text.lines().collect();
            let height = lines.len();
            let actual = lines
                .iter()
                .map(|line| UnicodeWidthStr::width(*line))
                .max()
                .unwrap_or(0);
            let mut out = format!("ok: {actual}x{height}\n");
            for line in lines.iter().take(PREVIEW_LINES) {
                out.push_str(line);
                out.push('\n');
            }
            if height > PREVIEW_LINES {
                out.push_str(&format!("… {} more lines\n", height - PREVIEW_LINES));
            }
            out
        }
        Err(error) => {
            let (code, message) = failure(error);
            format!("{code}: {message}")
        }
    }
}

/// The tool-result error for a render failure: the code the tests and the model see, and the
/// message with a one-line fix. Kept public so the reason mapping is testable.
pub fn failure(error: codex_mermaid::RenderError) -> (&'static str, String) {
    let (code, hint) = match error {
        codex_mermaid::RenderError::Unsupported => (
            "unsupported",
            "this syntax is not in Radian's Mermaid subset; see the `diagrams` skill \
             (subgraph, autonumber, activation, styling, HTML and extra node shapes are not \
             supported)",
        ),
        codex_mermaid::RenderError::Limit => (
            "limit",
            "the diagram is over a hard limit (16 nodes, 24 edges, 8 participants, 64 sequence \
             events, 16 KiB source, 40-cell labels); split it into smaller diagrams",
        ),
        codex_mermaid::RenderError::TooWide => (
            "too_wide",
            "shorten the labels or use fewer side-by-side nodes, or switch to a \
             sequenceDiagram; raising `width` (up to 500) can also help",
        ),
    };
    (code, format!("{hint} ({error})"))
}

fn spec() -> ToolSpec {
    ToolSpec {
        name: "mermaid_check".into(),
        description: "Render a Mermaid diagram with Radian's bounded renderer and report whether \
                      the transcript would show it: on success the rendered size and a preview, \
                      on failure the reason and how to fix it. Use this instead of reading the \
                      renderer source or guessing; see the `diagrams` skill."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "string",
                    "description": "The Mermaid source, starting with a supported header (flowchart/graph, sequenceDiagram, stateDiagram-v2, classDiagram, erDiagram)."
                },
                "width": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_WIDTH,
                    "description": format!("The available display width in cells (default {DEFAULT_WIDTH}, max {MAX_WIDTH}).")
                }
            },
            "required": ["source"],
            "additionalProperties": false
        }),
        concurrency: ToolConcurrency::Shared,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR 0028 R8: a `width` over the cap is clamped to `MAX_WIDTH`, so a huge request still
    /// gets the renderer's honest answer instead of an unbounded render. A node wider than the
    /// cap makes the clamp observable: at `MAX_WIDTH` it is `too_wide`, at a million it would
    /// render. Removing the clamp makes this test return `ok`.
    #[tokio::test]
    async fn a_width_over_the_cap_is_clamped() {
        use agent_core::{
            CancellationToken, ContentPart, NoopSink, ToolCall, ToolContext, ToolProvider,
            ToolSession,
        };
        let session = MermaidCheckTools::new()
            .open_session()
            .await
            .expect("a session");
        // 16 nodes, each a 40-cell label (the label cap), in a chain: the natural width is
        // well over MAX_WIDTH, so at 500 it is `too_wide` and at a million it would render.
        let label = "x".repeat(40);
        let mut source = String::from("flowchart LR\n");
        for i in 0..16 {
            source.push_str(&format!("    A{i}[{label}]\n"));
        }
        for i in 0..15 {
            source.push_str(&format!("    A{i} --> A{}\n", i + 1));
        }
        let call = ToolCall {
            id: "t".into(),
            name: "mermaid_check".into(),
            arguments: serde_json::json!({ "source": source, "width": 1_000_000 }),
        };
        let cx = ToolContext {
            cancellation: CancellationToken::new(),
            events: &NoopSink,
            index: 0,
        };
        let out = session.execute(&call, cx).await.expect("a result");
        let text: String = out
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            text.contains("too_wide"),
            "a width over the cap must be clamped to {MAX_WIDTH}: {text}"
        );
    }

    #[test]
    fn a_small_flowchart_reports_ok_and_a_preview() {
        let out = check("flowchart TD\n    A[Start] --> B[Done]\n", DEFAULT_WIDTH);
        assert!(out.starts_with("ok: "), "{out}");
        assert!(out.contains("Start") && out.contains("Done"), "{out}");
    }

    #[test]
    fn each_failure_has_its_own_reason_and_a_hint() {
        let unsupported = check("flowchart TD\n    subgraph X\n    end\n", DEFAULT_WIDTH);
        assert!(unsupported.contains("unsupported"), "{unsupported}");
        assert!(unsupported.contains("diagrams"), "{unsupported}");

        let too_wide = check(
            "flowchart LR\n    A[aaaaaaaaaaaaaaaaaaaa] --> B[bbbbbbbbbbbbbbbbbbbb]\n",
            10,
        );
        assert!(too_wide.contains("too_wide"), "{too_wide}");
        assert!(too_wide.contains("sequenceDiagram"), "{too_wide}");

        // Over the node limit.
        let mut big = String::from("flowchart TD\n");
        for i in 0..20 {
            big.push_str(&format!("    A{i}[n{i}]\n"));
        }
        let limit = check(&big, DEFAULT_WIDTH);
        assert!(limit.contains("limit"), "{limit}");
    }

    #[test]
    fn the_preview_is_bounded() {
        // A sequence with many events renders tall; the preview stops at PREVIEW_LINES.
        let mut source = String::from("sequenceDiagram\n    participant A\n    participant B\n");
        for i in 0..40 {
            source.push_str(&format!("    A->>B: message {i}\n"));
        }
        let out = check(&source, 200);
        assert!(out.contains("more lines"), "{out}");
        assert!(
            out.lines().count() <= PREVIEW_LINES + 3,
            "{}",
            out.lines().count()
        );
    }
}
