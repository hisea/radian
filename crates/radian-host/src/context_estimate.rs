//! `/context`: per-category estimate of the next request (ADR 0015 C1/C2).
//!
//! The estimator rule is ADR 0015 C2 (ASCII bytes rounded up ÷ `bytes_per_token`,
//! one token per non-ASCII char), the same rule `agent_core::budget` uses. This
//! module reimplements only the counting (it cannot reuse `budget`'s private
//! `add_str`/`add_json`), and pins the total against
//! `agent_core::budget::estimate_tokens_with` in tests, so the two cannot drift.
//!
//! The sum of the categories equals the total: sizes are summed first (one
//! `div_ceil`), then the remainder of the division is distributed to the
//! categories with the largest fractional part. A per-category `div_ceil` would
//! overcount by up to one token per category.
//!
//! The breakdown is a pure data struct the TUI renders (owner addendum
//! 2026-10-20): the grid round only changes the view. It carries the stacking
//! numbers (`categories` + `free` + `buffer` = `window`) and the per-item
//! detail (context files, skills, MCP/built-in tools, memory records).

use agent_core::{ContentPart, Message, ModelRequest, ToolSpec};

/// One estimated category, in display order.
#[derive(Debug, Clone, PartialEq)]
pub struct Category {
    pub name: &'static str,
    pub tokens: u64,
    /// Share of the total in percent (0-100). Zero when the total is zero.
    pub share: f64,
}

/// One context file's contribution (ADR 0036).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDetail {
    pub path: String,
    pub tokens: u64,
}

/// The per-item detail the grid's sections need. Token counts here are
/// estimates for display; only the top-level categories take part in the
/// exact stacking sum.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Details {
    /// One row per context file, in prompt order.
    pub context_files: Vec<FileDetail>,
    /// Tokens of the whole `<project_instructions>` section.
    pub context_files_tokens: u64,
    /// Listable skills and the tokens of the skills listing.
    pub skills_count: usize,
    pub skills_tokens: u64,
    /// Built-in tool definitions (everything that is not an MCP tool).
    pub builtin_tools_count: usize,
    pub builtin_tools_tokens: u64,
    /// MCP servers and their tools.
    pub mcp_servers: usize,
    pub mcp_tools_count: usize,
    pub mcp_tools_tokens: u64,
    /// Memory records in the fixed block and the block's tokens.
    pub memory_records: usize,
    pub memory_tokens: u64,
}

/// The breakdown of one next request.
#[derive(Debug, Clone, PartialEq)]
pub struct Breakdown {
    pub categories: Vec<Category>,
    /// The total estimate `T` (the sum of the categories).
    pub total_tokens: u64,
    /// `C`: the context window size (ADR 0018). `None` when unknown.
    pub window: Option<u64>,
    /// `B`: the auto-compaction threshold (ADR 0018). `None` when unconfigured.
    pub threshold: Option<u64>,
    /// `threshold - used` (saturating): the free space before auto-compaction.
    pub free_tokens: Option<u64>,
    /// `window - threshold` (saturating): the reserved compaction buffer.
    pub buffer_tokens: Option<u64>,
    pub details: Details,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Size {
    ascii: u64,
    non_ascii_chars: u64,
}

impl Size {
    fn add_str(&mut self, text: &str) {
        for ch in text.chars() {
            if ch.is_ascii() {
                self.ascii += 1;
            } else {
                self.non_ascii_chars += 1;
            }
        }
    }

    fn add_bytes(&mut self, bytes: &[u8]) {
        // JSON is UTF-8; count it the same way `budget` does.
        match std::str::from_utf8(bytes) {
            Ok(text) => self.add_str(text),
            Err(_) => self.ascii += bytes.len() as u64,
        }
    }

    fn add_json<T: serde::Serialize>(&mut self, value: &T) {
        if let Ok(bytes) = serde_json::to_vec(value) {
            self.add_bytes(&bytes);
        }
    }

    fn remove_str(&mut self, text: &str) {
        for ch in text.chars() {
            if ch.is_ascii() {
                self.ascii = self.ascii.saturating_sub(1);
            } else {
                self.non_ascii_chars = self.non_ascii_chars.saturating_sub(1);
            }
        }
    }

    fn plus(self, other: Size) -> Size {
        Size {
            ascii: self.ascii + other.ascii,
            non_ascii_chars: self.non_ascii_chars + other.non_ascii_chars,
        }
    }

    fn tokens(&self, bytes_per_token: u64) -> u64 {
        let bpt = bytes_per_token.max(1);
        self.ascii.div_ceil(bpt) + self.non_ascii_chars
    }
}

/// The system layers split for the estimate.
#[derive(Debug, Clone, Default)]
pub struct SystemSplit {
    /// Everything except the context files and the memory block (0a, 1, skills,
    /// 0b, settings framing).
    pub base: String,
    /// The `<project_instructions>` section (ADR 0036).
    pub context_files: String,
    /// The fixed memory block (ADR 0021).
    pub memory: String,
}

/// One context file's block, in prompt order: the display path and the exact
/// `<project_instructions>` block the section concatenates.
#[derive(Debug, Clone, Default)]
pub struct ContextBlock {
    pub path: String,
    pub block: String,
}

/// Everything one estimate needs. The grid round only changes the view, so
/// every number the grid needs is computed here.
pub struct EstimateInput<'a> {
    pub request: &'a ModelRequest,
    pub system: &'a SystemSplit,
    /// The skills listing section (a substring of `system.base`) and how many
    /// listable skills it names.
    pub skills_section: &'a str,
    pub skills_count: usize,
    /// The context files' blocks, in prompt order.
    pub context_blocks: &'a [ContextBlock],
    /// MCP server ids, to tell MCP tools (`<server>__<tool>`) apart from
    /// built-in ones (the inspector's rule).
    pub mcp_server_ids: &'a [String],
    /// How many memory records the fixed block carries.
    pub memory_records: usize,
    pub bytes_per_token: u32,
    pub count_reasoning: bool,
    /// `C` (ADR 0018). `None` when the window is unknown.
    pub window: Option<u64>,
    /// `B` (ADR 0018). `None` when no budget is configured.
    pub threshold: Option<u64>,
}

/// Whether a tool spec is an MCP tool: `server__tool` with a known server id.
fn is_mcp_tool(name: &str, mcp_server_ids: &[String]) -> bool {
    match name.split_once("__") {
        Some((prefix, _)) => mcp_server_ids.iter().any(|id| id == prefix),
        None => false,
    }
}

/// Counts `text` the way `budget` counts a system prompt.
fn size_of_str(text: &str) -> Size {
    let mut size = Size::default();
    size.add_str(text);
    size
}

/// The JSON contribution of one message, minus host `meta` (ADR 0005, like
/// `budget::request_size_with`). When `count_reasoning` is false an
/// assistant's `reasoning` field is subtracted too (ADR 0014 Q4).
fn message_size(message: &Message, count_reasoning: bool) -> Size {
    let mut size = Size::default();
    size.add_json(message);
    if !message.meta().is_empty() {
        let field = serde_json::json!({ "meta": message.meta() }).to_string();
        if let Some(inner) = field.strip_prefix('{').and_then(|f| f.strip_suffix('}')) {
            size.remove_str(",");
            size.remove_str(inner);
        }
    }
    if !count_reasoning
        && let Message::Assistant { reasoning, .. } = message
        && !reasoning.is_empty()
    {
        let field = serde_json::json!({ "reasoning": reasoning }).to_string();
        if let Some(inner) = field.strip_prefix('{').and_then(|f| f.strip_suffix('}')) {
            size.remove_str(",");
            size.remove_str(inner);
        }
    }
    size
}

/// The `,"reasoning":…` fragment's size inside an assistant message: the part
/// the `reasoning` category owns when `count_reasoning` is on.
fn reasoning_fragment_size(reasoning: &[ContentPart]) -> Size {
    if reasoning.is_empty() {
        return Size::default();
    }
    let field = serde_json::json!({ "reasoning": reasoning }).to_string();
    let mut size = Size::default();
    if let Some(inner) = field.strip_prefix('{').and_then(|f| f.strip_suffix('}')) {
        size.add_str(",");
        size.add_str(inner);
    }
    size
}

/// Splits the next request's shape into the spec's categories. `request` is
/// the request as it would be sent (system prompt with all hooks applied,
/// transcript messages, tool specs, settings); `system` names which part of
/// `request.system_prompt` is context files and which is memory, so the three
/// system categories add up to the prompt byte for byte.
///
/// Returns the categories in display order; `reasoning` is listed only when
/// `count_reasoning` is on.
pub fn estimate(input: &EstimateInput) -> Breakdown {
    let request = input.request;
    let bpt = input.bytes_per_token.max(1) as u64;
    let count_reasoning = input.count_reasoning;

    // System: three additive string pieces.
    let base_size = size_of_str(&input.system.base);
    let context_size = size_of_str(&input.system.context_files);
    let memory_size = size_of_str(&input.system.memory);
    // The request's prompt must be exactly these three concatenated; when it
    // is not (a hook rewrote it), the remainder stays in base so nothing is
    // lost and the sum still matches the request.
    let prompt_size = {
        let mut size = Size::default();
        if let Some(prompt) = &request.system_prompt {
            size.add_str(prompt);
        }
        size
    };
    let pieces = base_size.plus(context_size).plus(memory_size);
    let base_size = Size {
        ascii: base_size.ascii + prompt_size.ascii.saturating_sub(pieces.ascii),
        non_ascii_chars: base_size.non_ascii_chars
            + prompt_size
                .non_ascii_chars
                .saturating_sub(pieces.non_ascii_chars),
    };

    // Tools: the whole array is the category (each definition counted once).
    let mut tools_size = Size::default();
    tools_size.add_json(&request.tools);

    // Messages: per-message sizes, split into user / assistant / reasoning /
    // tool results. Array framing (`[` `]` `,`) is attributed to the tool
    // results when there are any, else to assistant text, else to user
    // messages, so the categories sum to the array byte for byte.
    let mut user_size = Size::default();
    let mut assistant_size = Size::default();
    let mut reasoning_size = Size::default();
    let mut tool_results_size = Size::default();
    let mut per_message_total = Size::default();
    for message in &request.messages {
        let full = message_size(message, count_reasoning);
        per_message_total = per_message_total.plus(full);
        match message {
            Message::User { .. } => {
                user_size = user_size.plus(full);
            }
            Message::Assistant { reasoning, .. } => {
                if count_reasoning && !reasoning.is_empty() {
                    let fragment = reasoning_fragment_size(reasoning);
                    reasoning_size = reasoning_size.plus(fragment);
                    assistant_size = assistant_size.plus(Size {
                        ascii: full.ascii.saturating_sub(fragment.ascii),
                        non_ascii_chars: full
                            .non_ascii_chars
                            .saturating_sub(fragment.non_ascii_chars),
                    });
                } else {
                    assistant_size = assistant_size.plus(full);
                }
            }
            Message::Tool { .. } => {
                tool_results_size = tool_results_size.plus(full);
            }
        }
    }
    let mut messages_array_size = Size::default();
    messages_array_size.add_json(&request.messages);
    // Remove the metas/reasoning from the array size the same way the
    // per-message sizes did, so the framing difference is exact.
    for message in &request.messages {
        if !message.meta().is_empty() {
            let field = serde_json::json!({ "meta": message.meta() }).to_string();
            if let Some(inner) = field.strip_prefix('{').and_then(|f| f.strip_suffix('}')) {
                messages_array_size.remove_str(",");
                messages_array_size.remove_str(inner);
            }
        }
        if !count_reasoning
            && let Message::Assistant { reasoning, .. } = message
            && !reasoning.is_empty()
        {
            let field = serde_json::json!({ "reasoning": reasoning }).to_string();
            if let Some(inner) = field.strip_prefix('{').and_then(|f| f.strip_suffix('}')) {
                messages_array_size.remove_str(",");
                messages_array_size.remove_str(inner);
            }
        }
    }
    let framing = Size {
        ascii: messages_array_size
            .ascii
            .saturating_sub(per_message_total.ascii),
        non_ascii_chars: messages_array_size
            .non_ascii_chars
            .saturating_sub(per_message_total.non_ascii_chars),
    };
    if framing.ascii > 0 || framing.non_ascii_chars > 0 {
        if tool_results_size.ascii > 0
            || tool_results_size.non_ascii_chars > 0
            || request.messages.iter().any(|m| matches!(m, Message::Tool { .. }))
        {
            tool_results_size = tool_results_size.plus(framing);
        } else if assistant_size.ascii > 0 || assistant_size.non_ascii_chars > 0 {
            assistant_size = assistant_size.plus(framing);
        } else {
            user_size = user_size.plus(framing);
        }
    }

    // Settings ride with base (they are not a listed category).
    let mut settings_size = Size::default();
    settings_size.add_json(&request.settings);
    let base_size = base_size.plus(settings_size);

    // Order: base, context files, memory, tools, user, assistant, [reasoning],
    // tool results.
    let mut parts: Vec<(&'static str, Size)> = vec![
        ("base system prompt", base_size),
        ("context files", context_size),
        ("memory injection", memory_size),
        ("tool definitions", tools_size),
        ("user messages", user_size),
        ("assistant text", assistant_size),
    ];
    if count_reasoning {
        parts.push(("reasoning", reasoning_size));
    }
    parts.push(("tool results", tool_results_size));

    // One ceil for the total; distribute the remainder to the largest
    // fractional parts so the tokens sum exactly.
    let total_ascii: u64 = parts.iter().map(|(_, s)| s.ascii).sum();
    let total_non_ascii: u64 = parts.iter().map(|(_, s)| s.non_ascii_chars).sum();
    let total_tokens = total_ascii.div_ceil(bpt) + total_non_ascii;

    let floors: Vec<u64> = parts
        .iter()
        .map(|(_, s)| s.ascii / bpt + s.non_ascii_chars)
        .collect();
    let floor_sum: u64 = floors.iter().sum();
    let mut remainder = total_tokens.saturating_sub(floor_sum) as usize;
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(parts[i].1.ascii % bpt));
    let mut tokens = floors;
    for i in order {
        if remainder == 0 {
            break;
        }
        // Only categories with a fractional part can take from the remainder.
        if parts[i].1.ascii % bpt > 0 {
            tokens[i] += 1;
            remainder -= 1;
        }
    }
    // No fractional parts at all (or an empty request): the remainder can only
    // be the `[]`/`null` framing; put it on the first non-empty category, or
    // base when everything is empty.
    if remainder > 0 {
        let target = parts
            .iter()
            .position(|(_, s)| s.ascii > 0 || s.non_ascii_chars > 0)
            .unwrap_or(0);
        tokens[target] += remainder as u64;
    }

    let categories: Vec<Category> = parts
        .into_iter()
        .enumerate()
        .map(|(i, (name, _))| Category {
            name,
            tokens: tokens[i],
            share: if total_tokens == 0 {
                0.0
            } else {
                tokens[i] as f64 * 100.0 / total_tokens as f64
            },
        })
        .collect();

    // The stacking numbers: used + free + buffer
    // is the window. `free` is 0 past the threshold.
    let (free_tokens, buffer_tokens) = match (input.window, input.threshold) {
        (Some(window), Some(threshold)) => (
            Some(threshold.saturating_sub(total_tokens)),
            Some(window.saturating_sub(threshold)),
        ),
        (Some(window), None) => (Some(window.saturating_sub(total_tokens)), Some(0)),
        (None, Some(threshold)) => (Some(threshold.saturating_sub(total_tokens)), Some(0)),
        (None, None) => (None, None),
    };

    // Per-item detail.
    let context_files: Vec<FileDetail> = {
        // The blocks concatenate to the section byte for byte; one ceil for
        // their total, distributed like the categories.
        let sizes: Vec<Size> = input
            .context_blocks
            .iter()
            .map(|block| size_of_str(&block.block))
            .collect();
        let total_ascii: u64 = sizes.iter().map(|s| s.ascii).sum();
        let total_non: u64 = sizes.iter().map(|s| s.non_ascii_chars).sum();
        let total = total_ascii.div_ceil(bpt) + total_non;
        let floors: Vec<u64> = sizes
            .iter()
            .map(|s| s.ascii / bpt + s.non_ascii_chars)
            .collect();
        let mut rem = total.saturating_sub(floors.iter().sum::<u64>()) as usize;
        let mut ord: Vec<usize> = (0..sizes.len()).collect();
        ord.sort_by_key(|&i| std::cmp::Reverse(sizes[i].ascii % bpt));
        let mut toks = floors;
        for i in ord {
            if rem == 0 {
                break;
            }
            if sizes[i].ascii % bpt > 0 {
                toks[i] += 1;
                rem -= 1;
            }
        }
        input
            .context_blocks
            .iter()
            .enumerate()
            .map(|(i, block)| FileDetail {
                path: block.path.clone(),
                tokens: toks[i],
            })
            .collect()
    };
    let context_files_tokens = categories
        .iter()
        .find(|c| c.name == "context files")
        .map(|c| c.tokens)
        .unwrap_or(0);
    let memory_tokens = categories
        .iter()
        .find(|c| c.name == "memory injection")
        .map(|c| c.tokens)
        .unwrap_or(0);
    let (builtin_tools_count, builtin_tools_tokens, mcp_tools_count, mcp_tools_tokens) = {
        let mut builtin: Vec<&ToolSpec> = Vec::new();
        let mut mcp: Vec<&ToolSpec> = Vec::new();
        for spec in &request.tools {
            if is_mcp_tool(&spec.name, input.mcp_server_ids) {
                mcp.push(spec);
            } else {
                builtin.push(spec);
            }
        }
        let mut builtin_size = Size::default();
        builtin_size.add_json(&builtin);
        let mut mcp_size = Size::default();
        mcp_size.add_json(&mcp);
        (
            builtin.len(),
            builtin_size.tokens(bpt),
            mcp.len(),
            mcp_size.tokens(bpt),
        )
    };
    let details = Details {
        context_files,
        context_files_tokens,
        skills_count: input.skills_count,
        skills_tokens: size_of_str(input.skills_section).tokens(bpt),
        builtin_tools_count,
        builtin_tools_tokens,
        mcp_servers: input.mcp_server_ids.len(),
        mcp_tools_count,
        mcp_tools_tokens,
        memory_records: input.memory_records,
        memory_tokens,
    };

    Breakdown {
        categories,
        total_tokens,
        window: input.window,
        threshold: input.threshold,
        free_tokens,
        buffer_tokens,
        details,
    }
}

/// The request this breakdown was computed from, for the total-equality test.
pub fn total_of(request: &ModelRequest, bytes_per_token: u32, count_reasoning: bool) -> u64 {
    agent_core::budget::estimate_tokens_with(request, bytes_per_token, count_reasoning)
}

fn format_tokens(n: u64) -> String {
    if n < 1000 {
        format!("{n}")
    } else if n < 10_000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else {
        format!("{}K", n / 1000)
    }
}

/// Renders the `/context` notice lines: one per category with tokens and
/// share, the free space and the compaction buffer, the total `T / B`
/// (window), the reported line (ADR 0015 C1), the message count, the
/// compaction count, and the per-item detail.
pub fn notice_lines(
    breakdown: &Breakdown,
    reported: Option<u64>,
    message_count: usize,
    compactions: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    out.push("context (estimated):".to_owned());
    for category in &breakdown.categories {
        out.push(format!(
            "  {}: {} ({:.1}%)",
            category.name,
            format_tokens(category.tokens),
            category.share
        ));
    }
    if let Some(free) = breakdown.free_tokens {
        out.push(format!("  free until auto-compact: {}", format_tokens(free)));
    }
    if let Some(buffer) = breakdown.buffer_tokens {
        out.push(format!("  compaction buffer (reserved): {}", format_tokens(buffer)));
    }
    match (breakdown.threshold, breakdown.window) {
        (Some(threshold), Some(window)) => out.push(format!(
            "  total: {} / {} (window {})",
            format_tokens(breakdown.total_tokens),
            format_tokens(threshold),
            format_tokens(window)
        )),
        (Some(threshold), None) => out.push(format!(
            "  total: {} / {}",
            format_tokens(breakdown.total_tokens),
            format_tokens(threshold)
        )),
        (None, Some(window)) => out.push(format!(
            "  total: {} (window {})",
            format_tokens(breakdown.total_tokens),
            format_tokens(window)
        )),
        (None, None) => out.push(format!("  total: {}", format_tokens(breakdown.total_tokens))),
    }
    match reported {
        Some(input) => out.push(format!("reported: {} (last provider input)", format_tokens(input))),
        None => out.push("reported: none yet".to_owned()),
    }
    out.push(format!("messages: {message_count}"));
    out.push(format!("compactions this session: {compactions}"));
    let details = &breakdown.details;
    if !details.context_files.is_empty() {
        out.push("context files:".to_owned());
        for file in &details.context_files {
            out.push(format!("  {}: {}", file.path, format_tokens(file.tokens)));
        }
    }
    if details.skills_count > 0 {
        out.push(format!(
            "skills: {} listed, {}",
            details.skills_count,
            format_tokens(details.skills_tokens)
        ));
    }
    out.push(format!(
        "tools: {} built-in ({}) + {} MCP in {} servers ({})",
        details.builtin_tools_count,
        format_tokens(details.builtin_tools_tokens),
        details.mcp_tools_count,
        details.mcp_servers,
        format_tokens(details.mcp_tools_tokens),
    ));
    out.push(format!(
        "memory: {} records, {}",
        details.memory_records,
        format_tokens(details.memory_tokens)
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{MessageMeta, ToolConcurrency};
    use serde_json::json;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: format!("does {name}"),
            input_schema: json!({"type": "object"}),
            concurrency: ToolConcurrency::Exclusive,
        }
    }

    fn system() -> SystemSplit {
        let base = "<runtime>rt</runtime>\n\n<role>role</role>\n".to_owned();
        let context_files =
            "<project_instructions path=\"AGENTS.md\">\nbody\n</project_instructions>\n".to_owned();
        let memory = "\n\n## Memory (ADR 0021)\n- [global] (fact): note\n".to_owned();
        SystemSplit {
            base,
            context_files,
            memory,
        }
    }

    fn request(system: &SystemSplit) -> ModelRequest {
        let system_prompt = format!("{}{}{}", system.base, system.context_files, system.memory);
        ModelRequest {
            system_prompt: Some(system_prompt),
            messages: vec![
                Message::user("hello"),
                Message::Assistant {
                    content: vec![ContentPart::text("answer")],
                    reasoning: vec![ContentPart::text("because")],
                    tool_calls: vec![],
                    provider_payload: None,
                    interrupted: false,
                    meta: MessageMeta::default(),
                },
                Message::Tool {
                    result: agent_core::ToolResult::success("c1", vec![ContentPart::text("output")]),
                    meta: MessageMeta::default(),
                },
            ],
            tools: vec![spec("read"), spec("srv__search")],
            settings: serde_json::Value::Null,
        }
    }

    fn input<'a>(
        request: &'a ModelRequest,
        system: &'a SystemSplit,
        blocks: &'a [ContextBlock],
    ) -> EstimateInput<'a> {
        EstimateInput {
            request,
            system,
            skills_section: "",
            skills_count: 0,
            context_blocks: blocks,
            mcp_server_ids: &[],
            memory_records: 1,
            bytes_per_token: 4,
            count_reasoning: false,
            window: Some(100_000),
            threshold: Some(90_000),
        }
    }

    #[test]
    fn the_categories_sum_to_the_total() {
        let system = system();
        let req = request(&system);
        let blocks = [ContextBlock {
            path: "AGENTS.md".to_owned(),
            block: system.context_files.clone(),
        }];
        for count_reasoning in [false, true] {
            let mut inp = input(&req, &system, &blocks);
            inp.count_reasoning = count_reasoning;
            let breakdown = estimate(&inp);
            let sum: u64 = breakdown.categories.iter().map(|c| c.tokens).sum();
            assert_eq!(sum, breakdown.total_tokens, "{breakdown:?}");
            assert_eq!(
                breakdown.total_tokens,
                total_of(&req, 4, count_reasoning),
                "the total is the estimator's total"
            );
        }
    }

    #[test]
    fn every_message_and_tool_definition_is_counted_once() {
        let system = system();
        let mut req = request(&system);
        let blocks = [ContextBlock {
            path: "AGENTS.md".to_owned(),
            block: system.context_files.clone(),
        }];
        let full = estimate(&input(&req, &system, &blocks));
        // Dropping a message shrinks the total by exactly its category's
        // contribution: nothing is double-counted, nothing is lost.
        let dropped_user = {
            let mut without = req.clone();
            without.messages.remove(0);
            estimate(&input(&without, &system, &blocks))
        };
        assert!(
            dropped_user.total_tokens < full.total_tokens,
            "a dropped user message must shrink the total"
        );
        let dropped_tool_result = {
            let mut without = req.clone();
            without.messages.pop();
            estimate(&input(&without, &system, &blocks))
        };
        assert!(
            dropped_tool_result.total_tokens < full.total_tokens,
            "a dropped tool result must shrink the total"
        );
        req.tools.pop();
        let fewer_tools = estimate(&input(&req, &system, &blocks));
        assert!(
            fewer_tools.total_tokens < full.total_tokens,
            "a dropped tool definition must shrink the total"
        );
        // The tool category without tools is just the empty array (`[]`).
        let no_tools = ModelRequest {
            tools: vec![],
            ..req.clone()
        };
        let empty_tools = estimate(&input(&no_tools, &system, &blocks));
        let tools = empty_tools
            .categories
            .iter()
            .find(|c| c.name == "tool definitions")
            .unwrap();
        assert!(tools.tokens <= 1, "{empty_tools:?}");
    }

    #[test]
    fn reasoning_is_listed_only_when_counted() {
        let system = system();
        let req = request(&system);
        let blocks = [ContextBlock {
            path: "AGENTS.md".to_owned(),
            block: system.context_files.clone(),
        }];
        let without_input = input(&req, &system, &blocks);
        let without = estimate(&without_input);
        assert!(
            without.categories.iter().all(|c| c.name != "reasoning"),
            "{without:?}"
        );
        let mut with_input = input(&req, &system, &blocks);
        with_input.count_reasoning = true;
        let with = estimate(&with_input);
        let reasoning = with
            .categories
            .iter()
            .find(|c| c.name == "reasoning")
            .expect("reasoning is listed when counted");
        assert!(reasoning.tokens > 0, "{with:?}");
        let sum: u64 = with.categories.iter().map(|c| c.tokens).sum();
        assert_eq!(sum, with.total_tokens);
    }

    #[test]
    fn the_notice_marks_reported_and_estimated_apart() {
        let system = system();
        let req = request(&system);
        let blocks = [ContextBlock {
            path: "AGENTS.md".to_owned(),
            block: system.context_files.clone(),
        }];
        let breakdown = estimate(&input(&req, &system, &blocks));
        let lines = notice_lines(&breakdown, Some(12_000), 3, 1);
        assert!(lines.iter().any(|l| l.contains("estimated")), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("reported") && l.contains("12")),
            "{lines:?}"
        );
        let unreported = notice_lines(&breakdown, None, 0, 0);
        assert!(
            unreported.iter().any(|l| l.contains("none yet")),
            "{unreported:?}"
        );
    }

    /// Owner addendum 2026-10-20: categories + free + buffer sum to the window.
    #[test]
    fn categories_plus_free_plus_buffer_sum_to_the_window() {
        let system = system();
        let req = request(&system);
        let blocks = [ContextBlock {
            path: "AGENTS.md".to_owned(),
            block: system.context_files.clone(),
        }];
        let breakdown = estimate(&input(&req, &system, &blocks));
        let window = breakdown.window.unwrap();
        let threshold = breakdown.threshold.unwrap();
        let free = breakdown.free_tokens.unwrap();
        let buffer = breakdown.buffer_tokens.unwrap();
        assert!(breakdown.total_tokens <= threshold);
        assert_eq!(breakdown.total_tokens + free + buffer, window);
        assert_eq!(buffer, window - threshold);
    }

    /// The detail rows name every context file, the skills listing, the MCP
    /// split and the memory records.
    #[test]
    fn the_notice_details_name_files_skills_mcp_and_memory() {
        let system = system();
        let req = request(&system);
        let blocks = [ContextBlock {
            path: "AGENTS.md".to_owned(),
            block: system.context_files.clone(),
        }];
        let mut inp = input(&req, &system, &blocks);
        let mcp_ids = ["srv".to_owned()];
        inp.mcp_server_ids = &mcp_ids;
        inp.skills_section = "\n\n## Skills (ADR 0025)\n- a\n";
        inp.skills_count = 3;
        let breakdown = estimate(&inp);
        assert_eq!(breakdown.details.context_files.len(), 1);
        assert_eq!(breakdown.details.context_files[0].path, "AGENTS.md");
        assert_eq!(breakdown.details.mcp_servers, 1);
        assert_eq!(breakdown.details.mcp_tools_count, 1);
        assert_eq!(breakdown.details.builtin_tools_count, 1);
        assert_eq!(breakdown.details.memory_records, 1);
        let lines = notice_lines(&breakdown, None, 3, 0);
        let text = lines.join("\n");
        assert!(text.contains("AGENTS.md"), "{text}");
        assert!(text.contains("skills: 3 listed"), "{text}");
        assert!(text.contains("1 MCP in 1 servers"), "{text}");
        assert!(text.contains("memory: 1 records"), "{text}");
        assert!(text.contains("free until auto-compact"), "{text}");
        assert!(text.contains("compaction buffer"), "{text}");
        assert!(text.contains("window"), "{text}");
    }
}
