// Scratch evaluation: render the same eight samples with candidate crates.
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

pub const SAMPLES: &[(&str, &str)] = &[
    ("chain-6 TD", "flowchart TD\n  A[Start] --> B[Parse config]\n  B --> C[Open session]\n  C --> D[Run turn]\n  D --> E[Write journal]\n  E --> F[Exit]"),
    ("diamond fork/merge", "flowchart TD\n  A[User input] --> B{Tool call?}\n  B -- yes --> C[Run tool]\n  B -- no --> D[Reply]\n  C --> E[Append result]\n  E --> A\n  D --> F[Done]"),
    ("LR chain", "flowchart LR\n  TUI --> Host --> Core --> Provider"),
    ("radian crates (12 nodes)", "flowchart TD\n  cli[agent-cli] --> host[radian-host]\n  tui[radian-tui] --> host\n  host --> core[agent-core]\n  host --> sess[radian-session]\n  host --> rt[radian-runtime]\n  host --> perm[radian-permissions]\n  host --> sb[radian-sandbox]\n  host --> coding[radian-coding]\n  host --> mem[radian-memory]\n  host --> ext[agent-extensions]\n  ext --> core\n  coding --> sb"),
    ("sequence", "sequenceDiagram\n  participant U as User\n  participant T as TUI\n  participant H as Host\n  participant M as Model\n  U->>T: prompt\n  T->>H: turn\n  H->>M: request\n  M-->>H: tool_use\n  H->>H: run tool (sandbox)\n  H->>M: tool_result\n  M-->>H: text\n  H-->>T: events\n  T-->>U: render"),
    ("state", "stateDiagram-v2\n  [*] --> Idle\n  Idle --> Streaming: prompt\n  Streaming --> Tool: tool_use\n  Tool --> Streaming: result\n  Streaming --> Idle: end_turn\n  Idle --> [*]"),
    ("subgraph", "flowchart TD\n  subgraph Host\n    A --> B\n  end\n  B --> C"),
    ("long labels", "flowchart LR\n  A[Read AGENTS.md and the ADRs that touch the change] --> B[Write the failing test first] --> C[Implement until the Verify commands pass]"),
];

fn show(text: &str) {
    let w = text.lines().map(|l| l.width()).max().unwrap_or(0);
    println!("[{w} cols x {} rows]", text.lines().count());
    println!("{text}");
}

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "mermaid-text".into());
    for width in [80usize, 120] {
        println!("\n######## {which} · width {width} ########");
        for (name, src) in SAMPLES {
            let t = Instant::now();
            let out: Result<String, String> = match which.as_str() {
                "mermaid-text" => mermaid_text::render_with_width(src, Some(width)).map_err(|e| e.to_string()),
                "merman" => merman_render(src, width),
                _ => Err("unknown renderer".into()),
            };
            println!("\n=== {name} · {:.2?}", t.elapsed());
            match out { Ok(text) => show(&text), Err(e) => println!("ERROR: {e}") }
        }
    }
}

fn merman_render(src: &str, width: usize) -> Result<String, String> {
    use merman::ascii::{AsciiLayoutProfile, AsciiRenderOptions, AsciiViewportPolicy, OverflowPolicy};
    use merman::{AsciiRequest, OperationControl, RenderOutput, RenderRequest, Renderer};
    let options = AsciiRenderOptions::unicode().with_layout_profile(AsciiLayoutProfile::Canonical);
    let renderer = Renderer::new().with_parse_options(merman::ParseOptions::strict());
    let output = renderer
        .render(RenderRequest::ascii(
            src,
            OperationControl::new(),
            AsciiRequest { options, viewport: AsciiViewportPolicy::with_max_width(width).overflow(OverflowPolicy::Fallback), ..Default::default() },
        ))
        .map_err(|e| e.to_string())?;
    match output {
        RenderOutput::Ascii(Some(report)) => Ok(format!("{:?}\n{}", report.outcome, report.text)),
        RenderOutput::Ascii(None) => Err("no diagram detected".into()),
        _ => Err("unexpected output".into()),
    }
}
