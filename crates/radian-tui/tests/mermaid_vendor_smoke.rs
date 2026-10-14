//! Smoke test for the vendored `codex-mermaid` crate (`third_party/codex-mermaid`,
//! ADR 0028 R8). The TUI wiring comes later; this only pins the vendored API:
//! a minimal flowchart renders, and every output span carries one of the three
//! roles the TUI can theme — `Node`, `Edge`, `Text` — with no ANSI escapes.

use codex_mermaid::{Role, render_spans};

#[test]
fn minimal_flowchart_renders_with_only_node_edge_text_roles() {
    let lines = render_spans("flowchart LR; A[Start] -- go --> B[End]", 80)
        .expect("minimal flowchart renders in 80 columns");
    let spans: Vec<_> = lines.iter().flatten().collect();
    assert!(!spans.is_empty(), "render produced no spans");
    for span in &spans {
        assert!(
            matches!(span.role, Role::Node | Role::Edge | Role::Text),
            "unexpected role {:?} in span {:?}",
            span.role,
            span.text
        );
        assert!(
            !span.text.contains('\x1b'),
            "ANSI escape in span {:?}",
            span.text
        );
    }
    assert!(
        spans.iter().any(|span| span.role == Role::Node),
        "no Node span in output: {spans:?}"
    );
    assert!(
        spans.iter().any(|span| span.role == Role::Edge),
        "no Edge span in output: {spans:?}"
    );
}
