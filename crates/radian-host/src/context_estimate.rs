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

use agent_core::{ContentPart, Message, ModelRequest};

/// One estimated category, in display order.
#[derive(Debug, Clone, PartialEq)]
pub struct Category {
    pub name: &'static str,
    pub tokens: u64,
    /// Share of the total in percent (0-100). Zero when the total is zero.
    pub share: f64,
}

/// The breakdown of one next request.
#[derive(Debug, Clone, PartialEq)]
pub struct Breakdown {
    pub categories: Vec<Category>,
    /// The total estimate `T` (the sum of the categories).
    pub total_tokens: u64,
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
        let mut comma = Size::default();
        comma.add_str(",");
        size = comma.plus({
            let mut inner_size = Size::default();
            inner_size.add_str(inner);
            inner_size
        });
    }
    size
}

/// Splits the next request's shape into the spec's categories. `request` is
/// the request as it would be sent (system prompt with all hooks applied,
/// transcript messages, tool specs, settings); `system` names which part of
/// `request.system_prompt` is context files and which is memory, so the three
/// system categories add up to the prompt byte for byte.
///
/// Returns the categories in display order; the last entry is always the
/// total. `reasoning` is listed only when `count_reasoning` is on.
pub fn breakdown(
    request: &ModelRequest,
    system: &SystemSplit,
    bytes_per_token: u32,
    count_reasoning: bool,
) -> Breakdown {
    let bpt = bytes_per_token.max(1) as u64;

    // System: three additive string pieces.
    let base_size = size_of_str(&system.base);
    let context_size = size_of_str(&system.context_files);
    let memory_size = size_of_str(&system.memory);
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
    // messages, else dropped (no messages: framing is `[]` = 2 bytes ≈ 1
    // token, kept in the total through the whole-array size below).
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
    // No fractional parts at all (or empty request): the remainder can only
    // be the `[]`/`null` framing; put it on the first non-empty category, or
    // base when everything is empty.
    if remainder > 0 {
        let target = parts
            .iter()
            .position(|(_, s)| s.ascii > 0 || s.non_ascii_chars > 0)
            .unwrap_or(0);
        tokens[target] += remainder as u64;
    }

    let categories = parts
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

    Breakdown {
        categories,
        total_tokens,
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
/// share, then the total `T / B`, then the reported line, the message count
/// and the compaction count.
#[allow(clippy::too_many_arguments)]
pub fn notice_lines(
    breakdown: &Breakdown,
    budget: Option<u64>,
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
    match budget {
        Some(b) => out.push(format!(
            "  total: {} / {}",
            format_tokens(breakdown.total_tokens),
            format_tokens(b)
        )),
        None => out.push(format!("  total: {}", format_tokens(breakdown.total_tokens))),
    }
    match reported {
        Some(input) => out.push(format!("reported: {} (last provider input)", format_tokens(input))),
        None => out.push("reported: none yet".to_owned()),
    }
    out.push(format!("messages: {message_count}"));
    out.push(format!("compactions this session: {compactions}"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{ContentPart, MessageMeta, ToolSpec};
    use serde_json::json;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: format!("does {name}"),
            input_schema: json!({"type": "object"}),
            concurrency: agent_core::ToolConcurrency::Exclusive,
        }
    }

    fn request() -> (ModelRequest, SystemSplit) {
        let base = "<runtime>rt</runtime>\n\n<role>role</role>\n".to_owned();
        let context_files =
            "<project_instructions path=\"AGENTS.md\">\nbody\n</project_instructions>\n".to_owned();
        let memory = "\n\n## Memory (ADR 0021)\n- [global] (fact): note\n".to_owned();
        let system_prompt = format!("{base}{context_files}{memory}");
        let messages = vec![
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
        ];
        let request = ModelRequest {
            system_prompt: Some(system_prompt),
            messages,
            tools: vec![spec("read"), spec("write")],
            settings: serde_json::Value::Null,
        };
        let system = SystemSplit {
            base,
            context_files,
            memory,
        };
        (request, system)
    }

    #[test]
    fn the_categories_sum_to_the_total() {
        for count_reasoning in [false, true] {
            let (request, system) = request();
            let breakdown = breakdown(&request, &system, 4, count_reasoning);
            let sum: u64 = breakdown.categories.iter().map(|c| c.tokens).sum();
            assert_eq!(sum, breakdown.total_tokens, "{breakdown:?}");
            assert_eq!(
                breakdown.total_tokens,
                total_of(&request, 4, count_reasoning),
                "the total is the estimator's total"
            );
        }
    }

    #[test]
    fn every_message_and_tool_definition_is_counted_once() {
        let (mut request, system) = request();
        let full = breakdown(&request, &system, 4, false);
        // Dropping a message shrinks the total by exactly its category's
        // contribution: nothing is double-counted, nothing is lost.
        let dropped_user = {
            let mut without = request.clone();
            without.messages.remove(0);
            breakdown(&without, &system, 4, false)
        };
        assert!(
            dropped_user.total_tokens < full.total_tokens,
            "a dropped user message must shrink the total"
        );
        let dropped_tool_result = {
            let mut without = request.clone();
            without.messages.pop();
            breakdown(&without, &system, 4, false)
        };
        assert!(
            dropped_tool_result.total_tokens < full.total_tokens,
            "a dropped tool result must shrink the total"
        );
        request.tools.pop();
        let fewer_tools = breakdown(&request, &system, 4, false);
        assert!(
            fewer_tools.total_tokens < full.total_tokens,
            "a dropped tool definition must shrink the total"
        );
        // The tool category without tools is just the empty array (`[]`).
        let no_tools = ModelRequest {
            tools: vec![],
            ..request.clone()
        };
        let empty_tools = breakdown(&no_tools, &system, 4, false);
        let tools = empty_tools
            .categories
            .iter()
            .find(|c| c.name == "tool definitions")
            .unwrap();
        assert!(tools.tokens <= 1, "{empty_tools:?}");
    }

    #[test]
    fn reasoning_is_listed_only_when_counted() {
        let (request, system) = request();
        let without = breakdown(&request, &system, 4, false);
        assert!(
            without.categories.iter().all(|c| c.name != "reasoning"),
            "{without:?}"
        );
        let with = breakdown(&request, &system, 4, true);
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
        let (request, system) = request();
        let breakdown = breakdown(&request, &system, 4, false);
        let lines = notice_lines(&breakdown, Some(100_000), Some(12_000), 3, 1);
        assert!(lines.iter().any(|l| l.contains("estimated")), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("reported") && l.contains("12")),
            "{lines:?}"
        );
        let unreported = notice_lines(&breakdown, Some(100_000), None, 0, 0);
        assert!(
            unreported.iter().any(|l| l.contains("none yet")),
            "{unreported:?}"
        );
    }
}
