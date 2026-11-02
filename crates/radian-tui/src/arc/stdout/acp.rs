//! An ACP worker's `result.jsonl` as the conversation tab draws it (ADR 0048 §13).
//!
//! The fold itself is the host's ([`radian_host::transcript::AcpFold`], ADR 0023 U1): the same
//! code the hub folds an arc attempt with, so a remote client and this page see the same
//! entries. This module **mirrors** the fold's entries into the TUI's render entries and draws
//! them through the attach view's renderer — Markdown, thinking blocks, tool cards and diffs
//! instead of one row per token. The fold keeps at most `MAX_ENTRIES` entries, each text capped
//! at `MAX_ENTRY_BYTES`, and drops the oldest in batches, so a worker that prints without bound
//! costs a fixed amount of memory (ADR 0048 §13).

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::{App, Entry};
use crate::theme::Theme;

/// The fold's window bounds, re-exported for the tab's tests.
#[cfg(test)]
pub(super) use radian_host::transcript::acp::{DROP, MAX_ENTRIES, MAX_ENTRY_BYTES};

/// The folded transcript of one ACP attempt: the host's fold plus the render entries the tab
/// draws and the scroll state a screen owns.
pub(super) struct AcpFold {
    inner: radian_host::transcript::AcpFold,
    pub(super) app: App,
}

impl AcpFold {
    pub(super) fn new() -> Self {
        let mut app = App::new("", "");
        app.session.follow = true;
        Self {
            inner: radian_host::transcript::AcpFold::new(),
            app,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub(super) fn scroll_lines(&mut self, delta: i32) {
        self.app.scroll_by(delta);
    }

    pub(super) fn follow_end(&mut self) {
        self.app.scroll_to_bottom();
    }

    /// Draws the folded transcript inside the conversation tab's frame, through the attach
    /// view's renderer and theme (ADR 0048 §13).
    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        crate::attach::draw_entries(&mut self.app, frame, area, theme);
    }

    /// Folds one complete line of the attempt's `result.jsonl`.
    pub(super) fn push_line(&mut self, raw: &str) {
        self.inner.push_line(raw);
        self.sync();
    }

    /// Mirrors what the fold changed into the render entries. Only the entries from the fold's
    /// lowest changed index are converted, so a stream costs one conversion per new or rewritten
    /// entry, not a rebuild per line.
    fn sync(&mut self) {
        let Some(change) = self.inner.take_change() else {
            return;
        };
        let folded = self.inner.entries();
        let from = if change.reset { 0 } else { change.from };
        let from = from.min(folded.len()).min(self.app.session.entries.len());
        self.app.session.entries.truncate(from);
        let app = &mut self.app;
        for entry in &folded[from..] {
            let converted = Entry::from_fold(entry, &mut app.images);
            app.session.entries.push(converted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::EntryKind;
    use crate::blocks::Card;
    use crate::tools;
    use serde_json::Value;
    use serde_json::json;

    fn envelope(update: Value) -> String {
        json!({
            "type": "acp",
            "message": {
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {"sessionId": "s", "update": update},
            },
        })
        .to_string()
    }

    fn chunk(kind: &str, text: &str) -> String {
        envelope(json!({
            "sessionUpdate": kind,
            "content": {"type": "text", "text": text},
        }))
    }

    fn tool(kind: &str, id: &str, mut fields: Value) -> String {
        fields["sessionUpdate"] = json!(kind);
        fields["toolCallId"] = json!(id);
        envelope(fields)
    }

    /// One `diff` content block, as ACP writes it.
    fn diff_block(path: &str, old: &str, new: &str) -> Value {
        json!({"type": "diff", "path": path, "oldText": old, "newText": new})
    }

    /// One `content` block carrying text, as ACP writes it.
    fn text_block(text: &str) -> Value {
        json!({"type": "content", "content": {"type": "text", "text": text}})
    }

    fn permission_request(id: Value, tool: &str) -> String {
        json!({
            "type": "acp",
            "message": {
                "jsonrpc": "2.0",
                "id": id,
                "method": "session/request_permission",
                "params": {"sessionId": "s", "toolCall": {"toolCallId": tool}, "options": []},
            },
        })
        .to_string()
    }

    fn permission_answer(id: Value, kind: &str) -> String {
        json!({
            "type": "acp",
            "answer": {"requestId": id, "kind": kind, "optionId": null},
        })
        .to_string()
    }

    fn result(outcome: &str, text: &str) -> String {
        json!({"type": "Result", "outcome": outcome, "text": text}).to_string()
    }

    fn folded(lines: &[String]) -> AcpFold {
        let mut fold = AcpFold::new();
        for line in lines {
            fold.push_line(line);
        }
        fold
    }

    fn texts(fold: &AcpFold) -> Vec<String> {
        fold.app
            .session
            .entries
            .iter()
            .map(|entry| entry.text.clone())
            .collect()
    }

    fn kinds(fold: &AcpFold) -> Vec<EntryKind> {
        fold.app
            .session
            .entries
            .iter()
            .map(|entry| entry.kind)
            .collect()
    }

    /// Consecutive message chunks are one Markdown message, consecutive thought chunks one
    /// thinking block, and a change of kind starts a new block (ADR 0048 §13).
    #[test]
    fn chunks_merge_into_one_block_and_thought_and_message_alternate() {
        let text: String = "你好世界😀".chars().cycle().take(200).collect();
        let mut lines = Vec::new();
        for ch in text.chars() {
            lines.push(chunk("agent_message_chunk", &ch.to_string()));
        }
        let fold = folded(&lines);
        assert_eq!(kinds(&fold), vec![EntryKind::Assistant]);
        assert_eq!(fold.app.session.entries[0].text, text);

        let fold = folded(&[
            chunk("agent_thought_chunk", "think "),
            chunk("agent_thought_chunk", "hard"),
            chunk("agent_message_chunk", "the answer"),
            chunk("agent_message_chunk", " continues"),
            chunk("agent_thought_chunk", "and another thought"),
            chunk("user_message_chunk", "hello"),
            chunk("user_message_chunk", " there"),
        ]);
        assert_eq!(
            kinds(&fold),
            vec![
                EntryKind::Reasoning,
                EntryKind::Assistant,
                EntryKind::Reasoning,
                EntryKind::User,
            ]
        );
        assert_eq!(
            texts(&fold),
            vec![
                "think hard".to_owned(),
                "the answer continues".to_owned(),
                "and another thought".to_owned(),
                "hello there".to_owned(),
            ]
        );
    }

    /// A `tool_call` opens a card keyed by `toolCallId`; its updates merge title, kind, status
    /// and output. An update for an unknown id creates the card where the `tool_call` would
    /// have (ADR 0048 §13).
    #[test]
    fn tool_updates_merge_by_id_and_an_out_of_order_update_creates_the_card() {
        let fold = folded(&[
            tool(
                "tool_call_update",
                "c1",
                json!({"title": "Edit image.txt", "kind": "edit", "status": "pending"}),
            ),
            tool("tool_call", "c1", json!({"status": "in_progress"})),
        ]);
        assert_eq!(kinds(&fold), vec![EntryKind::ToolCall]);
        let call = &fold.app.session.entries[0];
        assert_eq!(call.text, "Edit image.txt");
        assert_eq!(call.card, Card::Solo);
        assert_eq!(
            call.tool.as_ref().unwrap().status.as_deref(),
            Some("running")
        );

        let mut fold = fold;
        fold.push_line(&tool(
            "tool_call_update",
            "c1",
            json!({
                "status": "completed",
                "content": [{"type": "content", "content": {"type": "text", "text": "wrote 4 bytes"}}],
            }),
        ));
        assert_eq!(
            kinds(&fold),
            vec![EntryKind::ToolCall, EntryKind::ToolResult]
        );
        assert_eq!(texts(&fold), vec!["Edit image.txt", "wrote 4 bytes"]);
        let call = &fold.app.session.entries[0];
        assert_eq!(call.card, Card::Head);
        assert_eq!(call.tool.as_ref().unwrap().status.as_deref(), Some("done"));
        assert_eq!(call.tool.as_ref().unwrap().result_lines, Some(1));
        assert_eq!(fold.app.session.entries[1].card, Card::Tail);
    }

    /// A `diff` content block draws the same body the patch card draws, with the ACP title as
    /// the call line (ADR 0048 §13).
    #[test]
    fn diff_content_draws_the_patch_body() {
        let fold = folded(&[tool(
            "tool_call",
            "c1",
            json!({
                "title": "Edit image.txt",
                "kind": "edit",
                "status": "completed",
                "content": [{
                    "type": "diff",
                    "path": "image.txt",
                    "oldText": "one\ntwo\n",
                    "newText": "one\nTWO\n",
                }],
            }),
        )]);
        assert_eq!(
            kinds(&fold),
            vec![EntryKind::ToolCall, EntryKind::ToolResult]
        );
        let call = &fold.app.session.entries[0];
        assert_eq!(call.text, "Edit image.txt");
        let card = call.tool.as_ref().unwrap();
        assert_eq!(card.name, "patch");
        assert_eq!(card.arguments["path"], "image.txt");
        assert_eq!(fold.app.session.entries[1].text, "-one\n-two\n+one\n+TWO\n");
        assert_eq!(
            fold.app.session.entries[1].tool.as_ref().unwrap().name,
            "patch"
        );
    }

    /// A `diff` block and a `content` text block in one `tool_call_update` both draw: the diff
    /// first, then the text as ordinary body rows (ADR 0048 §13).
    #[test]
    fn a_diff_and_content_text_in_one_update_both_draw() {
        let fold = folded(&[tool(
            "tool_call",
            "c1",
            json!({
                "title": "Edit image.txt",
                "kind": "edit",
                "status": "completed",
                "content": [
                    diff_block("image.txt", "one\n", "ONE\n"),
                    text_block("applied 1 hunk"),
                ],
            }),
        )]);
        assert_eq!(
            kinds(&fold),
            vec![EntryKind::ToolCall, EntryKind::ToolResult]
        );
        let result = &fold.app.session.entries[1];
        assert_eq!(result.text, "-one\n+ONE\n\napplied 1 hunk");
        let card = result.tool.as_ref().unwrap();
        assert_eq!(card.name, "patch");
        assert_eq!(card.arguments["path"], "image.txt");
        assert_eq!(card.trailing_from, Some("-one\n+ONE\n".len()));

        // The diff rows keep their roles and the text after them is a plain row.
        let body = tools::renderer_for(&card.name).body(card, &result.text);
        assert_eq!(
            body.iter()
                .filter_map(|line| match line {
                    tools::BodyLine::Text { role, .. } => Some(*role),
                    tools::BodyLine::Diff(_) => None,
                })
                .collect::<Vec<_>>(),
            vec![
                tools::LineRole::DiffRemoved,
                tools::LineRole::DiffAdded,
                tools::LineRole::Normal,
                tools::LineRole::Normal,
            ]
        );
        let last = body.last().unwrap();
        let tools::BodyLine::Text { source, .. } = last else {
            panic!("the text row is a text row: {last:?}");
        };
        assert_eq!(&result.text[source.clone()], "applied 1 hunk");
    }

    /// The order inside the `content` array does not matter: a `content` text block before the
    /// `diff` still draws the diff first and the text after it (ADR 0048 §13).
    #[test]
    fn content_text_before_a_diff_draws_the_same_way() {
        let fold = folded(&[tool(
            "tool_call",
            "c1",
            json!({
                "title": "Edit image.txt",
                "kind": "edit",
                "status": "completed",
                "content": [
                    text_block("applied 2 hunks"),
                    diff_block("image.txt", "one\n", "ONE\n"),
                ],
            }),
        )]);
        let result = &fold.app.session.entries[1];
        assert_eq!(result.text, "-one\n+ONE\n\napplied 2 hunks");
        assert_eq!(
            result.tool.as_ref().unwrap().trailing_from,
            Some("-one\n+ONE\n".len())
        );
    }

    /// Two `diff` blocks in one update draw both files, each under its own header, instead of
    /// only the first (ADR 0048 §13).
    #[test]
    fn two_diffs_in_one_update_draw_both_files() {
        let fold = folded(&[tool(
            "tool_call",
            "c1",
            json!({
                "title": "Edit two files",
                "kind": "edit",
                "status": "completed",
                "content": [
                    diff_block("one.txt", "a\n", "A\n"),
                    diff_block("two.txt", "b\n", "B\n"),
                ],
            }),
        )]);
        assert_eq!(
            kinds(&fold),
            vec![EntryKind::ToolCall, EntryKind::ToolResult]
        );
        let result = &fold.app.session.entries[1];
        assert_eq!(
            result.text,
            "--- a/one.txt\n+++ b/one.txt\n-a\n+A\n--- a/two.txt\n+++ b/two.txt\n-b\n+B\n"
        );
        let card = result.tool.as_ref().unwrap();
        assert_eq!(card.name, "patch");
        assert_eq!(card.arguments["path"], "one.txt");
        // Both files' rows are there, and the second file's header names its path.
        let body = tools::renderer_for(&card.name).body(card, &result.text);
        let removed: Vec<String> = body
            .iter()
            .filter_map(|line| match line {
                tools::BodyLine::Text {
                    source,
                    role: tools::LineRole::DiffRemoved,
                    ..
                } => Some(result.text[source.clone()].to_owned()),
                _ => None,
            })
            .collect();
        assert_eq!(removed, vec!["-a".to_owned(), "-b".to_owned()]);
    }

    /// A later `tool_call_update` that replaces the content with text only shows the new text:
    /// the old diff does not stay behind (ADR 0048 §13).
    #[test]
    fn a_later_text_only_update_replaces_the_diff() {
        let mut fold = folded(&[tool(
            "tool_call",
            "c1",
            json!({
                "title": "Edit image.txt",
                "kind": "edit",
                "status": "completed",
                "content": [
                    diff_block("image.txt", "one\n", "ONE\n"),
                    text_block("applied 1 hunk"),
                ],
            }),
        )]);
        fold.push_line(&tool(
            "tool_call_update",
            "c1",
            json!({
                "status": "completed",
                "content": [text_block("nothing to change")],
            }),
        ));
        assert_eq!(
            kinds(&fold),
            vec![EntryKind::ToolCall, EntryKind::ToolResult]
        );
        let result = &fold.app.session.entries[1];
        assert_eq!(result.text, "nothing to change");
        assert_eq!(result.tool.as_ref().unwrap().name, "edit");
        assert_eq!(result.tool.as_ref().unwrap().trailing_from, None);
    }

    /// A text-only update with no diff still renders through the built-in renderer for its
    /// kind: a `write` card shows the content it wrote, not the tool's summary text (ADR 0028).
    #[test]
    fn text_only_update_keeps_the_builtin_renderer_body() {
        let fold = folded(&[tool(
            "tool_call",
            "c1",
            json!({
                "title": "Write a.txt",
                "kind": "write",
                "status": "completed",
                "rawInput": {"path": "a.txt", "content": "l1\nl2"},
                "content": [text_block("wrote 5 bytes to a.txt")],
            }),
        )]);
        assert_eq!(
            kinds(&fold),
            vec![EntryKind::ToolCall, EntryKind::ToolResult]
        );
        let result = &fold.app.session.entries[1];
        let card = result.tool.as_ref().unwrap();
        assert_eq!(card.name, "write");
        assert_eq!(result.text, "l1\nl2");
        assert_eq!(card.trailing_from, None);
    }

    /// The entry cap cuts a huge text after the diff, never the diff itself (ADR 0048 §13).
    #[test]
    fn a_huge_text_after_a_diff_is_cut_and_keeps_the_diff() {
        let text = "x".repeat(MAX_ENTRY_BYTES + 500);
        let fold = folded(&[tool(
            "tool_call",
            "c1",
            json!({
                "title": "Edit image.txt",
                "kind": "edit",
                "status": "completed",
                "content": [
                    diff_block("image.txt", "one\n", "ONE\n"),
                    text_block(&text),
                ],
            }),
        )]);
        let result = &fold.app.session.entries[1];
        assert_eq!(result.text.len(), MAX_ENTRY_BYTES);
        assert!(result.text.starts_with("-one\n+ONE\n\n"), "{}", result.text);
        assert_eq!(
            result.tool.as_ref().unwrap().trailing_from,
            Some("-one\n+ONE\n".len())
        );
        // The diff rows are still the first rows of the drawn body.
        let card = result.tool.as_ref().unwrap();
        let body = tools::renderer_for(&card.name).body(card, &result.text);
        assert!(matches!(
            body[0],
            tools::BodyLine::Text {
                role: tools::LineRole::DiffRemoved,
                ..
            }
        ));
    }

    /// A `terminal` block has only an id: the title and the status are the whole card.
    #[test]
    fn terminal_content_draws_only_the_title_and_status() {
        let fold = folded(&[tool(
            "tool_call",
            "c1",
            json!({
                "title": "Run the tests",
                "kind": "execute",
                "status": "completed",
                "content": [{"type": "terminal", "terminalId": "t1"}],
            }),
        )]);
        assert_eq!(kinds(&fold), vec![EntryKind::ToolCall]);
        assert_eq!(fold.app.session.entries[0].text, "Run the tests");
        assert_eq!(
            fold.app.session.entries[0]
                .tool
                .as_ref()
                .unwrap()
                .status
                .as_deref(),
            Some("done")
        );
    }

    /// A plan is one list replaced by the next plan update, not a row per entry (ADR 0048 §13).
    #[test]
    fn a_plan_replaces_itself() {
        let mut fold = folded(&[envelope(json!({
            "sessionUpdate": "plan",
            "entries": [
                {"status": "pending", "content": "write the file"},
                {"status": "in_progress", "content": "run the test"},
                {"status": "completed", "content": "report"},
            ],
        }))]);
        assert_eq!(kinds(&fold), vec![EntryKind::Notice]);
        assert_eq!(
            fold.app.session.entries[0].text,
            "plan\npending  write the file\nin_progress  run the test\ncompleted  report"
        );
        fold.push_line(&envelope(json!({
            "sessionUpdate": "plan",
            "entries": [{"status": "completed", "content": "all done"}],
        })));
        assert_eq!(kinds(&fold), vec![EntryKind::Notice]);
        assert_eq!(
            fold.app.session.entries[0].text,
            "plan\ncompleted  all done"
        );
    }

    /// `usage_update` and `session_info_update` share one weak status line; the other hidden
    /// kinds and an unknown kind are not rows, and no JSON is dumped (ADR 0048 §13).
    #[test]
    fn hidden_update_kinds_leave_one_weak_status_line() {
        let mut fold = folded(&[
            envelope(json!({"sessionUpdate": "usage_update", "used": 1234, "size": 200000})),
            envelope(json!({"sessionUpdate": "session_info_update", "title": "Ship it"})),
            envelope(json!({"sessionUpdate": "available_commands_update", "commands": []})),
            envelope(json!({"sessionUpdate": "current_mode_update", "currentModeId": "code"})),
            envelope(json!({"sessionUpdate": "something_new", "value": 1})),
        ]);
        assert_eq!(kinds(&fold), vec![EntryKind::Notice]);
        assert_eq!(
            fold.app.session.entries[0].text,
            "context 1234/200000 · Ship it"
        );
        fold.push_line(&envelope(
            json!({"sessionUpdate": "usage_update", "used": 2000, "size": 200000}),
        ));
        assert_eq!(kinds(&fold), vec![EntryKind::Notice]);
        assert_eq!(
            fold.app.session.entries[0].text,
            "context 2000/200000 · Ship it"
        );
    }

    /// A permission answer is one weak line after the tool it answers, even when another tool
    /// is open and the tool's result arrives after the answer (ADR 0048 §13).
    #[test]
    fn a_permission_answer_sits_after_the_tool_it_answers() {
        let mut fold = folded(&[
            tool(
                "tool_call",
                "c1",
                json!({"title": "Write image.txt", "kind": "edit", "status": "pending"}),
            ),
            tool(
                "tool_call",
                "c2",
                json!({"title": "Read notes.txt", "kind": "read", "status": "pending"}),
            ),
            permission_request(json!(7), "c1"),
            permission_answer(json!(7), "allow_once"),
            tool(
                "tool_call_update",
                "c1",
                json!({
                    "status": "completed",
                    "content": [{"type": "content", "content": {"type": "text", "text": "written"}}],
                }),
            ),
        ]);
        assert_eq!(
            kinds(&fold),
            vec![
                EntryKind::ToolCall,
                EntryKind::ToolResult,
                EntryKind::Notice,
                EntryKind::ToolCall
            ]
        );
        assert_eq!(
            texts(&fold),
            vec![
                "Write image.txt",
                "written",
                "permission allow_once",
                "Read notes.txt"
            ]
        );

        // An answer whose request id is unknown attaches to the newest tool.
        fold.push_line(&tool(
            "tool_call_update",
            "c2",
            json!({"status": "in_progress"}),
        ));
        fold.push_line(&permission_answer(json!(99), "reject_once"));
        assert_eq!(
            fold.app.session.entries.last().unwrap().text,
            "permission reject_once"
        );
    }

    /// A JSON-RPC id may be a string, and the wrapper logs the agent's own `id` and `requestId`
    /// values unchanged, so a string request id must find its tool instead of falling back to
    /// the newest one (ADR 0048 §13).
    #[test]
    fn a_permission_answer_matches_a_string_request_id() {
        let fold = folded(&[
            tool(
                "tool_call",
                "c1",
                json!({"title": "Write image.txt", "kind": "edit", "status": "pending"}),
            ),
            tool(
                "tool_call",
                "c2",
                json!({"title": "Read notes.txt", "kind": "read", "status": "pending"}),
            ),
            permission_request(json!("perm-1"), "c1"),
            tool("tool_call_update", "c2", json!({"status": "in_progress"})),
            permission_answer(json!("perm-1"), "allow_once"),
        ]);
        assert_eq!(
            kinds(&fold),
            vec![EntryKind::ToolCall, EntryKind::Notice, EntryKind::ToolCall]
        );
        assert_eq!(
            texts(&fold),
            vec!["Write image.txt", "permission allow_once", "Read notes.txt"]
        );
    }

    /// The numeric id `7` and the string id `"7"` are different JSON-RPC requests; their
    /// answers attach to their own tools (ADR 0048 §13).
    #[test]
    fn a_numeric_and_a_string_request_id_do_not_collide() {
        let fold = folded(&[
            tool(
                "tool_call",
                "c1",
                json!({"title": "Write image.txt", "kind": "edit", "status": "pending"}),
            ),
            tool(
                "tool_call",
                "c2",
                json!({"title": "Read notes.txt", "kind": "read", "status": "pending"}),
            ),
            permission_request(json!(7), "c1"),
            permission_request(json!("7"), "c2"),
            tool("tool_call_update", "c1", json!({"status": "in_progress"})),
            permission_answer(json!("7"), "allow_once"),
            permission_answer(json!(7), "reject_once"),
        ]);
        assert_eq!(
            texts(&fold),
            vec![
                "Write image.txt",
                "permission reject_once",
                "Read notes.txt",
                "permission allow_once",
            ]
        );
    }

    /// Unanswered permission requests cannot grow the request-id map without bound: the fold
    /// promises fixed memory (ADR 0048 §13).
    #[test]
    fn outstanding_permission_requests_are_bounded() {
        let mut fold = AcpFold::new();
        for index in 0..200 {
            fold.push_line(&permission_request(json!(index), "c1"));
        }
        assert_eq!(fold.inner.outstanding_permissions(), 64);
    }

    /// A `Result` line is the outcome line in the done or the failed colour; an empty text adds
    /// nothing (ADR 0048 §13).
    #[test]
    fn a_result_line_is_the_outcome_line() {
        let fold = folded(&[result("completed", "all done"), result("error", "")]);
        assert_eq!(
            kinds(&fold),
            vec![
                EntryKind::Outcome { ok: true },
                EntryKind::Outcome { ok: false }
            ]
        );
        assert_eq!(
            texts(&fold),
            vec!["Result completed all done", "Result error"]
        );
    }

    /// A line that is neither ACP nor `Result` keeps the row view's text as one weak notice, so
    /// a mixed file stays readable (ADR 0048 §13).
    #[test]
    fn mixed_non_acp_lines_keep_the_row_text() {
        let fold = folded(&[
            "not json at all".to_owned(),
            json!({"type": "item.completed", "item": {"text": "the answer"}}).to_string(),
            result("error", ""),
        ]);
        assert_eq!(
            kinds(&fold),
            vec![
                EntryKind::Notice,
                EntryKind::Notice,
                EntryKind::Outcome { ok: false }
            ]
        );
        assert_eq!(
            texts(&fold),
            vec![
                "not json at all",
                "item.completed the answer",
                "Result error"
            ]
        );
    }

    /// The window is bounded: dropping is in batches, the newest entries stay, and one entry's
    /// text is capped (ADR 0048 §13).
    #[test]
    fn the_fold_keeps_a_bounded_window_and_capped_text() {
        let mut fold = AcpFold::new();
        for index in 0..2_000 {
            fold.push_line(&format!("row-{index:04}"));
        }
        assert!(fold.app.session.entries.len() <= MAX_ENTRIES);
        assert!(fold.app.session.entries.len() > MAX_ENTRIES - DROP);
        assert_eq!(fold.app.session.entries.last().unwrap().text, "row-1999");
        assert!(
            !texts(&fold).iter().any(|text| text == "row-0000"),
            "the oldest rows are gone"
        );

        let long = "x".repeat(MAX_ENTRY_BYTES + 100);
        let fold = folded(&[chunk("agent_message_chunk", &long)]);
        assert_eq!(fold.app.session.entries[0].text.len(), MAX_ENTRY_BYTES);
    }

    /// **Edge case 2 (the page half).** The conversation page shows exactly the entries the
    /// host's fold produced for an arc attempt's `result.jsonl`: one entry each, same kind, text,
    /// card and call id (ADR 0023 U1, ADR 0048 §13).
    ///
    /// The fold's own mapping is pinned by `radian-host`'s fixture test; this one pins the
    /// mirror, which is what the page actually draws.
    ///
    /// Mutation: stop truncating the mirrored tail in `sync` (a rewritten entry then keeps its
    /// old status) and this FAILS.
    #[test]
    fn the_conversation_page_shows_the_folds_entries() {
        let fixture = vec![
            chunk("agent_thought_chunk", "Looking at the file."),
            tool(
                "tool_call",
                "c1",
                json!({"title": "Read main.rs", "kind": "read", "status": "pending"}),
            ),
            permission_request(json!(1), "c1"),
            permission_answer(json!(1), "allow_once"),
            tool(
                "tool_call_update",
                "c1",
                json!({
                    "status": "completed",
                    "content": [{"type": "content", "content": {"type": "text", "text": "1: fn main() {}"}}],
                }),
            ),
            chunk("agent_message_chunk", "Done."),
            chunk("agent_message_chunk", " No further changes."),
            result("completed", "finished"),
        ];
        let mut page = AcpFold::new();
        let mut direct = radian_host::transcript::AcpFold::new();
        for line in &fixture {
            page.push_line(line);
            direct.push_line(line);
        }
        let shown = &page.app.session.entries;
        assert_eq!(shown.len(), direct.entries().len());
        for (index, (shown, folded)) in shown.iter().zip(direct.entries()).enumerate() {
            assert_eq!(shown.kind, folded.kind, "entry {index}");
            assert_eq!(shown.text, folded.text, "entry {index}");
            assert_eq!(shown.card, folded.card, "entry {index}");
            assert_eq!(shown.result, folded.result, "entry {index}");
            assert_eq!(
                shown.tool.as_ref().map(|card| card.call_id.clone()),
                folded.tool.as_ref().map(|card| card.call_id.clone()),
                "entry {index}"
            );
            assert_eq!(
                shown.tool.as_ref().and_then(|card| card.status.clone()),
                folded.tool.as_ref().and_then(|card| card.status.clone()),
                "entry {index}"
            );
            assert_eq!(
                shown.tool.as_ref().and_then(|card| card.result_lines),
                folded.tool.as_ref().and_then(|card| card.result_lines),
                "entry {index}"
            );
        }
    }
}
