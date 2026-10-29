//! The inspector overlay: one tabbed `jobs | tools | mcp | hooks` panel (ADR 0023 U4).
//!
//! One [`Overlay::Inspector`] layer over the transcript; the input box and the status bar
//! stay visible below it (like the picker). Only the jobs page writes, through the same
//! path as `/jobs kill` (`Effect::Jobs`); everything else is read-only.
//!
//! `update` stays pure: the snapshot the overlay shows is loaded by the loop (an effect)
//! and fed back as an [`Input`]. Refreshes are event-driven (a job settle, a tool-list
//! change), never polled, so an open panel is idle-zero-redraw like an idle screen.

use radian_coding::{JobState, JobSummary};
use radian_core::ToolSpec;
use serde_json::Value;

use crate::command::InspectorPage;

/// One job row: what the jobs page lists, plus the tail the detail shows.
#[derive(Debug, Clone, PartialEq)]
pub struct JobEntry {
    pub id: String,
    pub command: String,
    pub state_label: String,
    pub elapsed_secs: u64,
    pub running: bool,
    pub output_tail: String,
}

/// One tool row: the model-visible definition and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolEntry {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// The `{"name","description","input_schema"}` compact JSON size, UTF-8 bytes.
    pub bytes: usize,
    /// Estimated tokens (`bytes / 4`, rounded up): the same heuristic the budget uses for
    /// text, documented here rather than invented per tool.
    pub tokens: usize,
    /// `builtin`, `skill`, or `mcp:<server>`.
    pub source: String,
}

/// One MCP server row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpEntry {
    pub id: String,
    /// The `--mcp-config` file the server came from, or `"—"` when unknown.
    pub source: String,
    /// `running` when the session opened with the server; `"—"` otherwise (a later
    /// failure is not tracked anywhere today).
    pub state: String,
    /// How many model-visible tools this server contributes.
    pub tools: usize,
    /// Whether the 64 KiB result cap truncated anything. Not tracked per server today,
    /// so always `"—"`.
    pub truncated: String,
}

/// One hook row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookEntry {
    /// `PreToolUse`, `PostToolUse` or `Stop`.
    pub event: String,
    /// The matcher as configured (`*` when empty/missing).
    pub matcher: String,
    pub command: String,
    pub timeout_secs: u64,
    /// User level only today: always `user config.toml`.
    pub source: String,
    /// Always `user` today; project hooks are not loaded.
    pub level: String,
    /// False when the matcher does not compile (it never runs).
    pub effective: bool,
}

/// Everything the overlay shows. Loaded by the loop, held by the overlay.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InspectorSnapshot {
    pub jobs: Vec<JobEntry>,
    pub tools: Vec<ToolEntry>,
    pub mcp: Vec<McpEntry>,
    pub hooks: Vec<HookEntry>,
    /// The async hooks running right now, in spawn order (the hooks page's Running
    /// section). They are not jobs: `/jobs` and the `job` tool never show them.
    pub hook_running: Vec<radian_host::RunningHook>,
    /// The session's recent hook runs, newest first (the hooks page's Recent runs section,
    /// the manual's "Recent runs"). In memory only.
    pub hook_runs: Vec<radian_host::HookRunRecord>,
    /// The `--mcp-config` path, when one was given. `None` renders as `"—"`.
    pub mcp_config_path: Option<String>,
}

/// The overlay's state: the page, the selection, the detail toggle and the snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Inspector {
    pub page: InspectorPage,
    pub selected: usize,
    /// Whether the selected row's detail is shown.
    pub detail: bool,
    /// A `k` waiting for `y`/`n` (jobs page only). `Some(id)` renders the confirm line;
    /// `y` kills through the `/jobs kill` path, `n`/`Esc` cancels.
    pub pending_kill: Option<String>,
    pub snapshot: InspectorSnapshot,
}

impl Inspector {
    pub fn new(page: InspectorPage, snapshot: InspectorSnapshot) -> Self {
        Self {
            page,
            selected: 0,
            detail: false,
            pending_kill: None,
            snapshot,
        }
    }

    /// The rows on the current page (the hooks page counts its running and run rows too:
    /// they scroll with the list).
    pub fn len(&self) -> usize {
        match self.page {
            InspectorPage::Jobs => self.snapshot.jobs.len(),
            InspectorPage::Tools => self.snapshot.tools.len(),
            InspectorPage::Mcp => self.snapshot.mcp.len(),
            InspectorPage::Hooks => {
                self.snapshot.hooks.len()
                    + self.snapshot.hook_running.len()
                    + self.snapshot.hook_runs.len()
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Switches to `page`, resetting the selection and closing the detail and any pending
    /// confirm. Tab order is [`InspectorPage::ALL`].
    pub fn switch(&mut self, page: InspectorPage) {
        if self.page != page {
            self.page = page;
            self.selected = 0;
            self.detail = false;
            self.pending_kill = None;
        }
    }

    pub fn next_page(&mut self) {
        let order = InspectorPage::ALL;
        let index = order
            .iter()
            .position(|page| *page == self.page)
            .unwrap_or(0);
        self.switch(order[(index + 1) % order.len()]);
    }

    pub fn prev_page(&mut self) {
        let order = InspectorPage::ALL;
        let index = order
            .iter()
            .position(|page| *page == self.page)
            .unwrap_or(0);
        self.switch(order[(index + order.len() - 1) % order.len()]);
    }

    pub fn move_selection(&mut self, delta: i32) {
        let len = self.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected as i32 + delta).rem_euclid(len as i32) as usize;
    }

    /// The row the shared list draws its cursor on. The hooks page puts non-selectable
    /// headings before the Running rows and the Recent runs rows, so a selection in those
    /// sections maps past the headings above it.
    fn list_selected(&self) -> usize {
        if self.page != InspectorPage::Hooks {
            return self.selected;
        }
        let hooks = self.snapshot.hooks.len();
        let running = self.snapshot.hook_running.len();
        let mut offset = 0;
        if self.selected >= hooks && running > 0 {
            offset += 1;
        }
        if self.selected >= hooks + running && !self.snapshot.hook_runs.is_empty() {
            offset += 1;
        }
        self.selected + offset
    }

    /// The id the jobs page would kill (`k`), or `None` on another page or an empty list.
    pub fn selected_job_id(&self) -> Option<String> {
        if self.page != InspectorPage::Jobs {
            return None;
        }
        self.snapshot
            .jobs
            .get(self.selected)
            .map(|job| job.id.clone())
    }

    /// Replaces the snapshot (an event-driven refresh), keeping the page and clamping the
    /// selection. A pending `k` confirm for a job that is gone is dropped.
    pub fn refresh(&mut self, snapshot: InspectorSnapshot) {
        self.snapshot = snapshot;
        let len = self.len();
        if len == 0 {
            self.selected = 0;
            self.detail = false;
        } else {
            self.selected = self.selected.min(len - 1);
        }
        if let Some(id) = &self.pending_kill
            && !self.snapshot.jobs.iter().any(|job| &job.id == id)
        {
            self.pending_kill = None;
        }
    }
}

/// Builds the jobs rows from the facade's list and tails. Running jobs come first in
/// registry order, then settled ones (the panel shows every entry the registry holds; the
/// `/jobs` text form stays running-only).
pub fn job_entries(jobs: &[JobSummary], tails: &[(String, JobState, String)]) -> Vec<JobEntry> {
    let tail_of = |id: &str| {
        tails
            .iter()
            .find(|(tail_id, _, _)| tail_id == id)
            .map(|(_, _, tail)| tail.clone())
            .unwrap_or_default()
    };
    let mut out: Vec<JobEntry> = jobs
        .iter()
        .map(|job| JobEntry {
            id: job.id.clone(),
            command: job.command.clone(),
            state_label: job.state.label().to_owned(),
            elapsed_secs: job.elapsed.as_secs(),
            running: job.state.is_running(),
            output_tail: tail_of(&job.id),
        })
        .collect();
    out.sort_by_key(|job| (!job.running, job.id.clone()));
    out
}

/// The model-visible tools: the provider's specs, in the same order.
pub fn tool_entries(specs: &[ToolSpec], mcp_ids: &[String]) -> Vec<ToolEntry> {
    specs
        .iter()
        .map(|spec| {
            let bytes = definition_bytes(spec);
            ToolEntry {
                name: spec.name.clone(),
                // MCP tools are `server__tool`; anything else named `skill` is the skill
                // tool, and the rest is built-in (coding, memory, ask, subagent).
                source: tool_source(&spec.name, mcp_ids),
                description: spec.description.clone(),
                input_schema: spec.input_schema.clone(),
                tokens: bytes.div_ceil(4),
                bytes,
            }
        })
        .collect()
}

/// The `{"name","description","input_schema"}` compact JSON size, UTF-8 bytes — the same
/// shape the `shell`/`job` byte-cap test measures.
pub fn definition_bytes(spec: &ToolSpec) -> usize {
    serde_json::json!({
        "name": spec.name,
        "description": spec.description,
        "input_schema": spec.input_schema,
    })
    .to_string()
    .len()
}

/// Where one tool came from: `mcp:<server>` for an MCP alias, `skill` for the skill tool,
/// `builtin` for the rest.
pub fn tool_source(name: &str, mcp_ids: &[String]) -> String {
    if let Some((prefix, _)) = name.split_once("__")
        && mcp_ids.iter().any(|id| id == prefix)
    {
        return format!("mcp:{prefix}");
    }
    if name == "skill" {
        return "skill".to_owned();
    }
    "builtin".to_owned()
}

/// Builds the MCP rows: one per configured server, in config order. The tool count is how
/// many model-visible tools carry the server's `id__` prefix; the state is `running` (the
/// session opened with every server — a failed open leaves no session to inspect). The
/// 64 KiB truncation is not tracked per server anywhere today, so it renders as `"—"`.
pub fn mcp_entries(
    servers: &[agent_extensions::McpServerConfig],
    tool_names: &[String],
    config_path: Option<&str>,
) -> Vec<McpEntry> {
    let source = config_path.unwrap_or("—").to_owned();
    servers
        .iter()
        .map(|server| {
            let prefix = format!("{}__", server.id);
            let tools = tool_names
                .iter()
                .filter(|name| name.starts_with(&prefix))
                .count();
            McpEntry {
                id: server.id.clone(),
                source: source.clone(),
                state: "running".to_owned(),
                tools,
                truncated: "—".to_owned(),
            }
        })
        .collect()
}

/// Builds the hooks rows: every configured entry in config order, with the matcher shown
/// as `*` when empty/missing and the effective flag following the same rule the runtime
/// uses (an invalid pattern or an out-of-set observe matcher never runs).
pub fn hook_entries(config: &radian_host::config::HooksConfig) -> Vec<HookEntry> {
    let mut out = Vec::new();
    let mut push = |event: &str,
                    matcher: &radian_host::config::HookMatcher,
                    effective: bool,
                    observe: bool| {
        for hook in &matcher.hooks {
            let raw = matcher.matcher.clone().unwrap_or_default();
            let display = if raw.is_empty() { "*" } else { raw.as_str() };
            out.push(HookEntry {
                event: event.to_owned(),
                matcher: display.to_owned(),
                command: hook.command.clone(),
                timeout_secs: if observe {
                    hook.observe_timeout().as_secs()
                } else {
                    hook.timeout().as_secs()
                },
                source: "user config.toml".to_owned(),
                level: "user".to_owned(),
                effective,
            });
        }
    };
    for matcher in &config.pre_tool_use {
        push(
            "PreToolUse",
            matcher,
            matcher_effective(matcher.matcher.as_deref()),
            false,
        );
    }
    for matcher in &config.permission_request {
        push(
            "PermissionRequest",
            matcher,
            matcher_effective(matcher.matcher.as_deref()),
            false,
        );
    }
    for matcher in &config.post_tool_use {
        push(
            "PostToolUse",
            matcher,
            matcher_effective(matcher.matcher.as_deref()),
            false,
        );
    }
    for matcher in &config.stop {
        push("Stop", matcher, true, false);
    }
    for matcher in &config.stop_failure {
        push(
            "StopFailure",
            matcher,
            radian_host::config::observe_matcher_effective(
                radian_host::config::STOP_FAILURE_MATCHERS,
                matcher.matcher.as_deref(),
            ),
            true,
        );
    }
    for matcher in &config.notification {
        push(
            "Notification",
            matcher,
            radian_host::config::observe_matcher_effective(
                radian_host::config::NOTIFICATION_TYPES,
                matcher.matcher.as_deref(),
            ),
            true,
        );
    }
    out
}

/// Whether one matcher runs, through the host's compile (no second regex dependency).
fn matcher_effective(matcher: Option<&str>) -> bool {
    radian_host::config::matcher_effective(matcher)
}

/// Draws the inspector into `area`: the shared [`crate::panel::Panel`] frame, the tab row,
/// the list through [`crate::select_list`], the selected row's detail, and the key hints.
pub fn draw(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    inspector: &Inspector,
    theme: &crate::theme::Theme,
    glyphs: crate::footer::Glyphs,
) {
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Paragraph;

    frame.render_widget(ratatui::widgets::Clear, area);
    let ascii = glyphs == crate::footer::Glyphs::Ascii;
    let width = area.width as usize;
    let inner = width.saturating_sub(4).max(1);
    let mut panel = crate::panel::Panel::new("inspector");
    panel = panel.row(tab_line(inspector, theme, ascii));
    panel = panel.divider();
    let items = list_items(inspector);
    // Budget: the area minus the panel's two borders, the tab row, the divider, and — when
    // shown — the detail divider plus the detail rows and the confirm row. The list takes
    // what is left, at least one row, so the bottom hints are never clipped. The hooks
    // page's Recent runs rows are part of this one list, so the window follows the selection
    // through every retained run.
    let mut reserved = 4usize;
    let detail_lines = detail_text(inspector, inner);
    if !detail_lines.is_empty() {
        reserved += 1 + detail_lines.len();
    }
    if inspector.pending_kill.is_some() {
        reserved += 1;
    }
    let budget = (area.height as usize).saturating_sub(reserved).max(1);
    let list = crate::select_list::view(
        theme,
        &items,
        "",
        inspector.list_selected(),
        budget,
        inner,
        ascii,
    );
    for line in list.lines {
        panel = panel.row(line);
    }
    if list.empty {
        panel = panel.row(Line::from(Span::styled(
            empty_message(inspector),
            theme.hint,
        )));
    }
    if !detail_lines.is_empty() {
        panel = panel.divider();
        for line in detail_lines {
            panel = panel.row(line);
        }
    }
    if let Some(id) = &inspector.pending_kill {
        panel = panel.row(Line::from(vec![
            Span::styled(format!("kill {id}? "), theme.status_warn),
            Span::styled("[y]", theme.text),
            Span::raw(" kill "),
            Span::styled("[n]", theme.text),
            Span::raw(" cancel"),
        ]));
    }
    panel = panel
        .hint("\u{2190}/\u{2192}", "tabs")
        .hint("\u{2191}/\u{2193}", "select");
    panel = panel.hint("\u{23ce}", "detail");
    if inspector.page == InspectorPage::Jobs {
        panel = panel.hint("k", "kill");
    }
    panel = panel.hint("esc", "close");
    frame.render_widget(Paragraph::new(panel.lines(width, theme, glyphs)), area);
}

/// The tab row: `jobs | tools | mcp | hooks`, the active page in the text role.
fn tab_line(
    inspector: &Inspector,
    theme: &crate::theme::Theme,
    _ascii: bool,
) -> ratatui::text::Line<'static> {
    use ratatui::text::{Line, Span};
    let mut spans = Vec::new();
    for (index, page) in InspectorPage::ALL.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" | ", theme.hint));
        }
        let label = page.label();
        if *page == inspector.page {
            spans.push(Span::styled(label.to_owned(), theme.text));
        } else {
            spans.push(Span::styled(label.to_owned(), theme.hint));
        }
    }
    Line::from(spans)
}

/// The list rows for the current page.
fn list_items(inspector: &Inspector) -> Vec<crate::select_list::Item> {
    match inspector.page {
        InspectorPage::Jobs => inspector
            .snapshot
            .jobs
            .iter()
            .map(|job| {
                crate::select_list::Item::new(job.id.clone()).description(format!(
                    "{} \u{b7} {}s \u{b7} {}",
                    job.state_label, job.elapsed_secs, job.command
                ))
            })
            .collect(),
        InspectorPage::Tools => inspector
            .snapshot
            .tools
            .iter()
            .map(|tool| {
                crate::select_list::Item::new(tool.name.clone()).description(format!(
                    "{} B \u{b7} ~{} tok \u{b7} {}",
                    tool.bytes, tool.tokens, tool.source
                ))
            })
            .collect(),
        InspectorPage::Mcp => inspector
            .snapshot
            .mcp
            .iter()
            .map(|server| {
                crate::select_list::Item::new(server.id.clone()).description(format!(
                    "{} \u{b7} {} tools \u{b7} trunc {}",
                    server.state, server.tools, server.truncated
                ))
            })
            .collect(),
        InspectorPage::Hooks => {
            let mut items: Vec<crate::select_list::Item> = inspector
                .snapshot
                .hooks
                .iter()
                .map(|hook| {
                    crate::select_list::Item::new(format!("{} {}", hook.event, hook.matcher))
                        .description(format!(
                            "{} \u{b7} {} \u{b7} {}",
                            hook.command,
                            hook.level,
                            if hook.effective { "on" } else { "off" }
                        ))
                })
                .collect();
            // The Running section: one heading row, then one line per async hook in
            // spawn order. Async hooks are not jobs, so this page is their only listing.
            if !inspector.snapshot.hook_running.is_empty() {
                items.push(crate::select_list::Item::new("Running"));
                items.extend(
                    inspector
                        .snapshot
                        .hook_running
                        .iter()
                        .map(|run| crate::select_list::Item::new(run.line())),
                );
            }
            // The Recent runs section: one heading row, then the last runs newest first,
            // each the one record line (`<event>:<tool> [<command>] <end> (<n>ms)`). They
            // share the configured entries' scrolling list so every retained run is
            // reachable; the selection skips the headings (`list_selected`).
            if !inspector.snapshot.hook_runs.is_empty() {
                items.push(crate::select_list::Item::new("Recent runs"));
                items.extend(
                    inspector
                        .snapshot
                        .hook_runs
                        .iter()
                        .map(|run| crate::select_list::Item::new(run.line())),
                );
            }
            items
        }
    }
}

/// The selected row's detail, as panel rows. Empty when the detail is closed or the list
/// is empty. Lines are pre-truncated to `inner` width; the jobs tail shows the last 10
/// lines, the tools page the full definition.
fn detail_text(inspector: &Inspector, inner: usize) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::{Line, Span};
    if !inspector.detail {
        return Vec::new();
    }
    let truncate = |text: &str| {
        if text.len() <= inner {
            text.to_owned()
        } else if inner == 0 {
            String::new()
        } else {
            format!("{}\u{2026}", &text[..inner.saturating_sub(1)])
        }
    };
    match inspector.page {
        InspectorPage::Jobs => {
            let Some(job) = inspector.snapshot.jobs.get(inspector.selected) else {
                return Vec::new();
            };
            let mut out = vec![Line::from(Span::raw(truncate(&format!(
                "{} \u{b7} {} \u{b7} {}s",
                job.id, job.state_label, job.elapsed_secs
            ))))];
            out.push(Line::from(Span::raw(truncate(&job.command))));
            let tail: Vec<&str> = job.output_tail.lines().rev().take(10).collect();
            if tail.is_empty() {
                out.push(Line::from(Span::raw("(no output yet)")));
            } else {
                for line in tail.into_iter().rev() {
                    out.push(Line::from(Span::raw(truncate(line))));
                }
            }
            out
        }
        InspectorPage::Tools => {
            let Some(tool) = inspector.snapshot.tools.get(inspector.selected) else {
                return Vec::new();
            };
            let mut out = vec![
                Line::from(Span::raw(truncate(&tool.description))),
                Line::from(Span::raw(truncate(&format!(
                    "source {} \u{b7} {} B \u{b7} ~{} tok",
                    tool.source, tool.bytes, tool.tokens
                )))),
            ];
            let schema = serde_json::to_string_pretty(&tool.input_schema)
                .unwrap_or_else(|_| "{}".to_owned());
            for line in schema.lines().take(12) {
                out.push(Line::from(Span::raw(truncate(line))));
            }
            out
        }
        InspectorPage::Mcp => {
            let Some(server) = inspector.snapshot.mcp.get(inspector.selected) else {
                return Vec::new();
            };
            let tools: Vec<&str> = inspector
                .snapshot
                .tools
                .iter()
                .filter(|tool| tool.source == format!("mcp:{}", server.id))
                .map(|tool| tool.name.as_str())
                .collect();
            let mut out = vec![
                Line::from(Span::raw(truncate(&format!("source {}", server.source)))),
                Line::from(Span::raw(truncate(&format!(
                    "state {} \u{b7} {} tools \u{b7} trunc {}",
                    server.state, server.tools, server.truncated
                )))),
            ];
            if tools.is_empty() {
                out.push(Line::from(Span::raw("(no tools)")));
            } else {
                out.push(Line::from(Span::raw(truncate(&tools.join(", ")))));
            }
            out
        }
        InspectorPage::Hooks => {
            if let Some(hook) = inspector.snapshot.hooks.get(inspector.selected) {
                return vec![
                    Line::from(Span::raw(truncate(&format!("event {}", hook.event)))),
                    Line::from(Span::raw(truncate(&format!("match {}", hook.matcher)))),
                    Line::from(Span::raw(truncate(&hook.command))),
                    Line::from(Span::raw(truncate(&format!(
                        "timeout {}s \u{b7} {} \u{b7} {} \u{b7} {}",
                        hook.timeout_secs,
                        hook.source,
                        hook.level,
                        if hook.effective {
                            "effective"
                        } else {
                            "off (bad matcher)"
                        }
                    )))),
                ];
            }
            // A running row's detail: the async hook's own fields.
            let hooks = inspector.snapshot.hooks.len();
            let running = inspector.snapshot.hook_running.len();
            if let Some(run) = inspector
                .snapshot
                .hook_running
                .get(inspector.selected.saturating_sub(hooks))
                .filter(|_| inspector.selected >= hooks && inspector.selected < hooks + running)
            {
                return vec![
                    Line::from(Span::raw(truncate(&format!(
                        "event {} \u{b7} {}",
                        run.event,
                        match &run.tool {
                            Some(tool) => format!("tool {tool}"),
                            None => "no tool (Stop)".to_owned(),
                        }
                    )))),
                    Line::from(Span::raw(truncate(&run.command))),
                    Line::from(Span::raw(truncate(&format!("running \u{b7} {}", run.id)))),
                ];
            }
            // A run row's detail: the record's own fields.
            let Some(run) = inspector
                .snapshot
                .hook_runs
                .get(inspector.selected - hooks - running)
            else {
                return Vec::new();
            };
            vec![
                Line::from(Span::raw(truncate(&format!(
                    "event {} \u{b7} {}",
                    run.event,
                    match &run.tool {
                        Some(tool) => format!("tool {tool}"),
                        None => "no tool (Stop)".to_owned(),
                    }
                )))),
                Line::from(Span::raw(truncate(&run.command))),
                Line::from(Span::raw(truncate(&format!(
                    "end {} \u{b7} {}ms",
                    run.end.label(),
                    run.millis
                )))),
            ]
        }
    }
}

/// The message shown when the current page has no rows.
fn empty_message(inspector: &Inspector) -> &'static str {
    match inspector.page {
        InspectorPage::Jobs => "no background jobs",
        InspectorPage::Tools => "no tools",
        InspectorPage::Mcp => "no MCP servers",
        InspectorPage::Hooks => "no hooks configured",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `k` is jobs-only: another page or an empty list offers no id.
    #[test]
    fn the_kill_key_is_jobs_only() {
        let mut inspector = Inspector::new(InspectorPage::Tools, InspectorSnapshot::default());
        assert_eq!(inspector.selected_job_id(), None);
        inspector.switch(InspectorPage::Jobs);
        assert_eq!(inspector.selected_job_id(), None);
    }

    /// Tab order follows the tab row; a switch resets the selection and the detail.
    #[test]
    fn tab_cycles_and_resets() {
        let mut inspector = Inspector::new(InspectorPage::Jobs, InspectorSnapshot::default());
        inspector.selected = 3;
        inspector.detail = true;
        inspector.next_page();
        assert_eq!(inspector.page, InspectorPage::Tools);
        assert_eq!(inspector.selected, 0);
        assert!(!inspector.detail);
        inspector.prev_page();
        assert_eq!(inspector.page, InspectorPage::Jobs);
    }

    /// The hooks page lists running async hooks under a "Running" heading, before the
    /// recent runs; both headings are skipped by the selection, and a running row's detail
    /// names the hook (ADR 0045 §5).
    #[test]
    fn the_hooks_page_lists_running_async_hooks() {
        let snapshot = InspectorSnapshot {
            hook_running: vec![radian_host::RunningHook {
                id: "hook-1".to_owned(),
                event: "PostToolUse".to_owned(),
                tool: Some("patch".to_owned()),
                command: "make lint".to_owned(),
                started: std::time::Instant::now(),
            }],
            hook_runs: vec![radian_host::HookRunRecord {
                event: "Stop".to_owned(),
                tool: None,
                command: "make check".to_owned(),
                end: radian_host::HookRunEnd::Status(0),
                millis: 3,
            }],
            ..InspectorSnapshot::default()
        };
        let mut inspector = Inspector::new(InspectorPage::Hooks, snapshot);
        assert_eq!(inspector.len(), 2, "one running hook and one run");
        let items = list_items(&inspector);
        let labels: Vec<&str> = items.iter().map(|item| item.label.as_str()).collect();
        assert_eq!(labels[0], "Running", "{labels:?}");
        assert!(labels[1].contains("PostToolUse:patch"), "{labels:?}");
        assert!(labels[1].contains("running"), "{labels:?}");
        assert_eq!(labels[2], "Recent runs", "{labels:?}");

        inspector.selected = 0;
        inspector.detail = true;
        assert_eq!(inspector.list_selected(), 1, "past the Running heading");
        assert!(
            detail_text(&inspector, 80)
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.content.contains("hook-1")),
            "the running row's detail names the hook: {:?}",
            detail_text(&inspector, 80)
        );
        inspector.selected = 1;
        assert_eq!(inspector.list_selected(), 3, "past both headings");
        assert!(
            detail_text(&inspector, 80)
                .iter()
                .flat_map(|line| line.spans.iter())
                .any(|span| span.content.contains("end status 0")),
            "the run row's detail is unchanged: {:?}",
            detail_text(&inspector, 80)
        );
    }
}
