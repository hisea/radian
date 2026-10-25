//! `radian arc mcp`: the MCP stdio server the lead writes the backlog through (ADR 0048
//! decisions 2 and 11).
//!
//! The server holds one [`Store`] and answers `tools/list` and `tools/call`. Every call is
//! one of the store's transactions, so it takes `state.lock`, never `runner.lock`, and
//! refuses a plan or task that changed outside arc with that reason in the tool error. That
//! is why the lead can add tasks while a scheduling loop runs (decision 5).
//!
//! The seven tools are in [`tools`]; there is no tool to confirm, start, merge, reject,
//! change the auto level, or approve (decision 11).
//!
//! With `--claude-experimental` (decision 11) the server also declares the `claude/channel`
//! capability and pushes task and plan transitions to the lead over the same stdio
//! connection: it reads the store every [`CHANNEL_INTERVAL_SECS`] seconds and sends one
//! [`CHANNEL_METHOD`] notification for each task whose `state.phase` entered
//! [`CHANNEL_PHASES`] (or that moved into `todo.ready` from [`CHANNEL_READY_FROM`]), and for
//! each plan field of [`CHANNEL_PLAN_FIELDS`] that changed. The channel is one-way; the read
//! tools are how the lead learns the details.

pub mod tools;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::time::Duration;

pub use tools::{TOOLS, ToolDef, names};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, CustomNotification,
    DiscoverResult, Implementation, InitializeResult, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ServerCapabilities, ServerNotification, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, Peer, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Map, Value, json};

use crate::store::model::TaskFile;
use crate::store::plan_ops::{CONFLICT_REWORK_SUFFIX, REVIEW_REWORK_SUFFIX, REWORK_HEADING};
use crate::store::{self, Error, Store};

/// The experiment capability Claude Code reads a channel declaration from.
pub const CHANNEL_CAPABILITY: &str = "claude/channel";
/// The capability the server does **not** declare: it never relays permission prompts.
pub const CHANNEL_PERMISSION_CAPABILITY: &str = "claude/channel/permission";
/// The notification method a channel event is sent with.
pub const CHANNEL_METHOD: &str = "notifications/claude/channel";
/// How often the poller reads every plan and its tasks.
pub const CHANNEL_INTERVAL_SECS: u64 = 5;
/// The newest protocol version the flagged server supports. A revision after this one (such
/// as `2026-07-28`) cannot carry channel messages, so Claude Code does not register a server
/// that negotiates it as a channel.
pub const CHANNEL_MAX_VERSION: ProtocolVersion = ProtocolVersion::V_2025_11_25;
/// The phases that cost the lead a turn when a task enters them. Every other change goes
/// unsent on purpose: each event starts one of the lead's model turns. This is the closed
/// list the manual's drift test reads.
pub const CHANNEL_PHASES: &[&str] = &[
    "todo.proposed",
    "review.awaiting_human",
    "review.rework_pending",
    "done.merged",
    "failed.exhausted",
    "failed.cancelled",
    "failed.irrecoverable",
];

/// The previous phases whose move into `todo.ready` also sends a task event: the owner's
/// confirm of a proposed task, and the lead's `add_rework` of a task in a `review.*` phase. The
/// owner's reject is not one of these: it moves the task to `review.rework_pending`, which
/// [`CHANNEL_PHASES`] sends with the note as its second line. An entry that ends in `.*` is a
/// prefix. This is the second closed task list the manual's drift test reads.
pub const CHANNEL_READY_FROM: &[&str] = &["todo.proposed", "review.*"];

/// The plan fields that cost the lead a turn when they change: the plan's `status`, and
/// whether it is `paused` (a `confirmed` plan whose `auto` is `off`). This is the closed list
/// the manual's drift test reads.
pub const CHANNEL_PLAN_FIELDS: &[&str] = &["status", "paused"];

/// The paragraph the flagged server adds to its `instructions`: what the lead receives and
/// how to read the details.
const CHANNEL_INSTRUCTIONS: &str = "Task transitions from arc arrive as `<channel>` blocks, one \
     per transition, with the meta attributes `task`, `plan`, `state`, `phase` and `previous`. \
     Plan transitions arrive the same way, with `plan`, `field`, `previous` and `value` and no \
     `task`; `field` is `status` or `paused`. They are one-way: there is no reply tool. Read \
     the details with `show`.";

/// The MCP server for one repository's arc store.
#[derive(Debug, Clone)]
pub struct ArcServer {
    store: Store,
    claude_experimental: bool,
}

impl ArcServer {
    /// A server over `store`, without the channel: today's tool server.
    pub fn new(store: Store) -> Self {
        Self {
            store,
            claude_experimental: false,
        }
    }

    /// A server over `store` that also pushes task transitions to the lead
    /// (`--claude-experimental`).
    pub fn with_channel(store: Store) -> Self {
        Self {
            store,
            claude_experimental: true,
        }
    }

    /// Answers one `tools/call`: the tool's JSON result, or the reason it failed. The reason
    /// is the same text the CLI prints, so a file that changed outside arc says so here too.
    pub fn dispatch(
        &self,
        name: &str,
        arguments: Option<Map<String, Value>>,
    ) -> Result<Value, String> {
        let mut args = Args::new(arguments);
        match name {
            "add_plan" => {
                let slug = args.required_string("slug")?;
                let title = args.required_string("title")?;
                let request = args.required_string("request")?;
                args.finish()?;
                let id = self
                    .store
                    .add_plan(&slug, &title, &request)
                    .map_err(reason)?;
                Ok(json!({ "id": id }))
            }
            "add_task" => {
                let plan = args.required_string("plan")?;
                let slug = args.required_string("slug")?;
                let title = args.required_string("title")?;
                let brief = args.required_string("brief")?;
                let after = args.optional_strings("after")?;
                let allow_noop = args.optional_bool("allow_noop")?;
                let extra_stages = args.optional_strings("extra_stages")?;
                args.finish()?;
                let id = self
                    .store
                    .add_task_proposed(
                        &plan,
                        &slug,
                        &title,
                        after,
                        allow_noop,
                        extra_stages,
                        &brief,
                    )
                    .map_err(reason)?;
                Ok(json!({ "id": id }))
            }
            "update_plan" => {
                let plan = args.required_string("plan")?;
                let title = args.optional_string("title")?;
                let request = args.optional_string("request")?;
                let change_record = args.optional_string("change_record")?;
                args.finish()?;
                store::plan_ops::update_plan(
                    &self.store,
                    &plan,
                    title.as_deref(),
                    request.as_deref(),
                    change_record.as_deref(),
                )
                .map_err(reason)?;
                store::cli::show_json(&self.store, &plan).map_err(reason)
            }
            "update_task" => {
                let task = args.required_string("task")?;
                let title = args.optional_string("title")?;
                let brief = args.optional_string("brief")?;
                let after = args.optional_strings_opt("after")?;
                let allow_noop = args.optional_flag("allow_noop")?;
                let extra_stages = args.optional_strings_opt("extra_stages")?;
                args.finish()?;
                store::plan_ops::update_task(
                    &self.store,
                    &task,
                    title.as_deref(),
                    brief.as_deref(),
                    after,
                    allow_noop,
                    extra_stages,
                )
                .map_err(reason)?;
                store::cli::show_json(&self.store, &task).map_err(reason)
            }
            "add_rework" => {
                let task = args.required_string("task")?;
                let brief = args.required_string("brief")?;
                args.finish()?;
                store::plan_ops::add_rework(&self.store, &task, &brief).map_err(reason)?;
                store::cli::show_json(&self.store, &task).map_err(reason)
            }
            "list" => {
                let plan = args.optional_string("plan")?;
                args.finish()?;
                // The manual's `list --json`: without `--all` a cancelled plan is hidden,
                // and the MCP `list` tool has no `--all`.
                store::cli::list_json(&self.store, plan.as_deref(), false).map_err(reason)
            }
            "show" => {
                let id = args.required_string("id")?;
                args.finish()?;
                store::cli::show_json(&self.store, &id).map_err(reason)
            }
            other => Err(format!(
                "no tool named `{other}`; tools are {}",
                names().join(", ")
            )),
        }
    }
}

/// The one line a tool error carries.
fn reason(error: Error) -> String {
    error.message()
}

impl ServerHandler for ArcServer {
    fn get_info(&self) -> InitializeResult {
        let mut capabilities = ServerCapabilities::builder().enable_tools().build();
        let mut instructions =
            "arc is the plan and task backlog of this repository. Write it with these \
             tools. Confirming a plan, starting a task, merging, and the auto level are \
             the owner's; you have no tool for them."
                .to_owned();
        if self.claude_experimental {
            capabilities.experimental = Some(BTreeMap::from([(
                CHANNEL_CAPABILITY.to_owned(),
                Map::new(),
            )]));
            instructions.push_str("\n\n");
            instructions.push_str(CHANNEL_INSTRUCTIONS);
        }
        InitializeResult::new(capabilities)
            .with_server_info(Implementation::new("radian-arc", env!("CARGO_PKG_VERSION")))
            .with_instructions(instructions)
    }

    /// Without the flag the server advertises every version this SDK knows, as it always
    /// has. With it the ceiling is the newest revision that carries channel messages: a
    /// later revision makes Claude Code decline to register the server as a channel.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        if self.claude_experimental {
            Cow::Borrowed(ProtocolVersion::known_up_to(&CHANNEL_MAX_VERSION))
        } else {
            Cow::Borrowed(ProtocolVersion::KNOWN_VERSIONS)
        }
    }

    /// `server/discover` is the probe that advertises the protocol versions. A flagged
    /// server has no version at or after `2026-07-28`, so the bootstrap probe is refused
    /// with the unsupported-protocol-version error instead of answered.
    fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<DiscoverResult, ErrorData>> + '_ {
        if !self.claude_experimental {
            return std::future::ready(Ok(DiscoverResult::from_server_info(
                self.supported_protocol_versions().into_owned(),
                self.get_info(),
            )));
        }
        let requested = context
            .meta
            .protocol_version()
            .unwrap_or(ProtocolVersion::V_2026_07_28);
        std::future::ready(Err(ErrorData::unsupported_protocol_version(
            requested,
            &self.supported_protocol_versions(),
        )))
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, ErrorData>> + '_ {
        let tools = TOOLS
            .iter()
            .map(|tool| {
                let schema: Map<String, Value> = match tool.schema() {
                    Value::Object(schema) => schema,
                    _ => Map::new(),
                };
                Tool::new(
                    Cow::Borrowed(tool.name),
                    Cow::Borrowed(tool.description),
                    schema,
                )
            })
            .collect();
        std::future::ready(Ok(ListToolsResult::with_all_items(tools)))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResponse, ErrorData>> + '_ {
        let result = self.call(request.name.as_ref(), request.arguments);
        std::future::ready(Ok(result.into()))
    }
}

/// Answers one `tools/call` as the client receives it: the tool's JSON result, or the reason
/// it failed. This is the one place the result's shape is decided, so `call_tool` and a test
/// cannot disagree about it.
impl ArcServer {
    /// One call's [`CallToolResult`]. The text content is always the tool's JSON, exactly what
    /// the CLI prints; [`success_result`] decides whether the value also rides in
    /// `structuredContent`.
    pub fn call(&self, name: &str, arguments: Option<Map<String, Value>>) -> CallToolResult {
        match self.dispatch(name, arguments) {
            Ok(value) => success_result(value),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        }
    }
}

/// One successful call's result. The text content is the value's JSON, the same document the
/// CLI prints. `structuredContent` is set only when that value is a JSON object: the MCP spec
/// requires a structured result to be an object, and a client that checks the field rejects a
/// result whose `structuredContent` is anything else. `list` answers with the array
/// `radian arc list --json` prints, so its result is text only.
fn success_result(value: Value) -> CallToolResult {
    if value.is_object() {
        CallToolResult::structured(value)
    } else {
        CallToolResult::success(vec![ContentBlock::text(value.to_string())])
    }
}

/// One task as the previous poll read it. `full` is `state.phase`, the value the channel
/// compares and reports; `state` and `phase` are its two parts for the meta.
#[derive(Debug, Clone)]
struct TaskSnapshot {
    plan: String,
    state: String,
    phase: String,
    full: String,
    /// Whether the newest rework heading is one arc wrote itself (a conflict with main, or a
    /// review that failed with blocking items), so the poller can tell it from the owner's or
    /// the lead's move into `todo.ready`.
    arc_rework: bool,
}

/// One plan as the previous poll read it. `paused` is a `confirmed` plan whose `auto` is
/// `off`: a `draft` plan waits for the owner's confirm and is not paused. `pause_record` is
/// the record that belongs to the pause the poller knows about: the one a read found while
/// the plan was paused, the one the automatic rework limit has written but not applied yet,
/// or a record an earlier pause left behind (see [`carry_pause_records`]).
#[derive(Debug, Clone)]
struct PlanSnapshot {
    status: String,
    paused: bool,
    /// A plan that is `cancelled`, by its status or by its tasks, is final and sends nothing.
    cancelled: bool,
    /// The record the automatic rework limit wrote (decision 12), when a task still carries
    /// one on its current attempt. The text goes only with the pause that record caused: the
    /// record stays after the owner resumes, so a later `radian arc pause` must not repeat it.
    pause_record: Option<PauseRecord>,
}

/// The newest `paused.json` record the automatic rework limit wrote for a plan: which record,
/// the reason it carries, and whether its pause has been applied yet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PauseRecord {
    /// The task, its attempt and the record's `at`: the same record read twice has the same
    /// identity, and a later attempt's record has a different one.
    id: String,
    /// The record's `text`, the pause's reason line.
    text: String,
    /// Whether the automatic rework limit has not applied this record's pause yet: it writes
    /// the record before it sets `auto` to `off` and marks the record applied after. A
    /// record a running plan shows is still waiting for its pause only while it is pending;
    /// one marked applied was read after that pause (and possibly its resume), so it is a
    /// pause the poller missed, not the one that follows.
    pending: bool,
}

/// Every plan and task the store holds, as one poll reads them.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    tasks: BTreeMap<String, TaskSnapshot>,
    plans: BTreeMap<String, PlanSnapshot>,
}

/// Every plan and task the store currently holds, by id. The read is the same read-only call
/// the CLI's `list` uses: it takes no lock.
fn snapshot(store: &Store) -> Result<Snapshot, Error> {
    let mut snapshot = Snapshot::default();
    for view in store.plans()? {
        // A file someone changed outside arc makes the round fail: arc refuses to act on it,
        // and the channel sends nothing from a read it cannot trust. The next round tries
        // again (the owner may adopt it).
        if view.plan.changed_outside_arc {
            return Err(Error::ChangedOutsideArc(view.plan.item.id.clone()));
        }
        let plan = &view.plan.item;
        let cancelled = view.is_cancelled();
        let paused = plan.status == "confirmed" && plan.auto == "off";
        snapshot.plans.insert(
            plan.id.clone(),
            PlanSnapshot {
                status: plan.status.clone(),
                paused,
                cancelled,
                pause_record: pause_record(store, &plan.id, &view.tasks),
            },
        );
        for task in &view.tasks {
            if task.changed_outside_arc {
                return Err(Error::ChangedOutsideArc(task.item.id.clone()));
            }
            let state = task.item.state.as_str().to_owned();
            let phase = task.item.phase.clone();
            snapshot.tasks.insert(
                task.item.id.clone(),
                TaskSnapshot {
                    plan: view.plan.item.id.clone(),
                    full: format!("{state}.{phase}"),
                    state,
                    phase,
                    arc_rework: arc_wrote_rework(&task.item.body),
                },
            );
        }
    }
    Ok(snapshot)
}

/// The newest `paused.json` record the automatic rework limit left for `plan` on one of its
/// tasks' current attempts: the record's identity and its `text` (decision 12). `radian arc
/// pause` records none. The record stays on its attempt after the owner resumes, so a caller
/// compares it with the record that belongs to the previous pause before it trusts the text
/// as this pause's reason; another task's older record is not this pause's reason either.
fn pause_record(
    store: &Store,
    plan: &str,
    tasks: &[store::Loaded<TaskFile>],
) -> Option<PauseRecord> {
    let mut newest: Option<(String, PauseRecord)> = None;
    for task in tasks {
        let path = crate::store::attempt_ops::run_dir(store, &task.item.id, task.item.attempt)
            .join(crate::store::attempt_ops::PAUSED_FILE);
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if value.get("plan").and_then(Value::as_str) != Some(plan) {
            continue;
        }
        let Some(text) = value.get("text").and_then(Value::as_str) else {
            continue;
        };
        let at = value
            .get("at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let record = PauseRecord {
            id: format!("{}@{}@{at}", task.item.id, task.item.attempt),
            text: text.to_owned(),
            // A record without the marker (one an older arc wrote) is one whose pause has
            // been applied: it must not become a pause's reason again.
            pending: value
                .get("pending")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        if newest
            .as_ref()
            .is_none_or(|(newest_at, _)| at >= *newest_at)
        {
            newest = Some((at, record));
        }
    }
    newest.map(|(_, record)| record)
}

/// Whether the newest `## Rework <n>` heading of a task body is one arc wrote itself. The
/// lead's `add_rework` and the owner's reject leave the heading without a suffix; a conflict
/// with main and a review that failed with blocking items carry one (decision 9).
fn arc_wrote_rework(body: &str) -> bool {
    body.lines()
        .rfind(|line| line.starts_with(REWORK_HEADING))
        .is_some_and(|line| {
            line.ends_with(CONFLICT_REWORK_SUFFIX) || line.ends_with(REVIEW_REWORK_SUFFIX)
        })
}

/// The task events to send, with the phase each task had before (`new` for a task the previous
/// read did not have). `previous` is a task map, not a phase map, so a task that moved to a
/// different plan is still one task.
fn task_transitions(
    previous: &BTreeMap<String, TaskSnapshot>,
    current: &BTreeMap<String, TaskSnapshot>,
) -> Vec<(String, TaskSnapshot, String)> {
    let mut events = Vec::new();
    for (task_id, task) in current {
        let before = previous
            .get(task_id)
            .map(|task| task.full.clone())
            .unwrap_or_else(|| "new".to_owned());
        if !sends_task(&before, task) {
            continue;
        }
        events.push((task_id.clone(), task.clone(), before));
    }
    events
}

/// Whether a task's move from `before` to its current phase sends an event.
fn sends_task(before: &str, task: &TaskSnapshot) -> bool {
    if before == task.full {
        return false;
    }
    if CHANNEL_PHASES.contains(&task.full.as_str()) {
        return true;
    }
    if !ready_from(before) || task.full != "todo.ready" {
        return false;
    }
    // The owner's reject leaves the task in `review.rework_pending` (decision 6), so the phase
    // list above sends it, note and all. A move into `todo.ready` from a `review.*` phase is the
    // lead's `add_rework`, and a rework arc wrote itself sends nothing.
    !task.arc_rework
}

/// Whether `before` is one of [`CHANNEL_READY_FROM`]'s phases. An entry ending in `.*` is a
/// prefix.
fn ready_from(before: &str) -> bool {
    CHANNEL_READY_FROM
        .iter()
        .any(|from| match from.strip_suffix(".*") {
            Some(prefix) => before.starts_with(prefix),
            None => before == *from,
        })
}

/// One plan event: the field, the two values `meta` carries, and the reason line to put under
/// them when the event is a pause the automatic rework limit caused.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanEvent {
    field: &'static str,
    previous: String,
    value: String,
    reason: Option<String>,
}

/// The plan events to send. A plan the previous read did not have sends nothing, and neither
/// does a cancelled one.
fn plan_transitions(
    previous: &BTreeMap<String, PlanSnapshot>,
    current: &BTreeMap<String, PlanSnapshot>,
) -> Vec<(String, PlanEvent)> {
    let mut events = Vec::new();
    for (plan_id, plan) in current {
        let Some(before) = previous.get(plan_id) else {
            continue;
        };
        // A cancelled plan is final (decision 3): it sends no plan event.
        if plan.cancelled {
            continue;
        }
        if before.status == "draft" && plan.status == "confirmed" {
            events.push((
                plan_id.clone(),
                PlanEvent {
                    field: "status",
                    previous: before.status.clone(),
                    value: plan.status.clone(),
                    reason: None,
                },
            ));
        }
        // A plan whose `status` changed is not paused or resumed by this read: `update_plan`'s
        // change record returns a confirmed plan to `draft` and sets `auto` to `off` in the
        // same write, and a confirm takes a draft plan to `confirmed`. Neither is a pause.
        if before.status == plan.status && before.paused != plan.paused {
            events.push((
                plan_id.clone(),
                PlanEvent {
                    field: "paused",
                    previous: bool_text(before.paused).to_owned(),
                    value: bool_text(plan.paused).to_owned(),
                    reason: pause_reason(before, plan),
                },
            ));
        }
    }
    events
}

/// Keeps each plan's pause record with the knowledge the poller already has about it.
///
/// A read that finds the plan paused keeps the record it found, and that record's pause has
/// now been reported: it is no longer pending, whatever the record says. A read that finds
/// the plan running keeps the record the previous read knew when it is the same record, and
/// the record is still pending only while both reads say so. A record the read finds for the
/// first time takes its `pending` marker from the store: the automatic rework limit writes the
/// record before it sets `auto` to `off` and marks it applied after, so a record a running
/// plan still shows as pending belongs to the pause that follows, while one it marks applied
/// was read after that pause and is not its reason. The poller's first read has no previous
/// read to take the marker from, so the marker is what tells a pause in progress from a
/// record an earlier pause left behind.
fn carry_pause_records(
    previous: Option<&BTreeMap<String, PlanSnapshot>>,
    current: &mut BTreeMap<String, PlanSnapshot>,
) {
    for (plan_id, plan) in current.iter_mut() {
        if plan.paused {
            if let Some(record) = &mut plan.pause_record {
                record.pending = false;
            }
            continue;
        }
        let Some(before) = previous.and_then(|previous| previous.get(plan_id)) else {
            continue;
        };
        if let (Some(found), Some(before)) = (&plan.pause_record, &before.pause_record)
            && found.id == before.id
        {
            // The marker says the pause is still to come only while both reads say so: a
            // read that already reported the pause does not resurrect the marker, and a
            // store that no longer marks the record pending says its pause has been applied
            // (or its plan write failed, leaving no pause to report).
            plan.pause_record = Some(PauseRecord {
                pending: before.pending && found.pending,
                ..before.clone()
            });
        }
    }
}

/// The reason line of a pause event: the text of the record that caused this pause. The
/// automatic rework limit writes the record just before it sets `auto` to `off`, so the record
/// the paused read finds is this pause's cause. A record from an earlier pause stays on its
/// attempt after the owner resumes; [`carry_pause_records`] keeps it as an applied record, so
/// a later `radian arc pause` sends no second line. A record a running read saw while it was
/// still pending is the one the pause that follows caused, so it is reported even though the
/// previous read already carried it.
fn pause_reason(before: &PlanSnapshot, plan: &PlanSnapshot) -> Option<String> {
    if !plan.paused {
        return None;
    }
    let record = plan.pause_record.as_ref()?;
    if let Some(before) = &before.pause_record
        && before.id == record.id
        && !before.pending
    {
        return None;
    }
    Some(record.text.clone())
}

fn bool_text(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

/// The event's second line: the same `rework_reason` `show --json` gives, when the task has
/// one. A read that fails leaves it off; the transition is still sent.
fn rework_reason(store: &Store, task_id: &str) -> Option<String> {
    let task: TaskFile = store.load_task(task_id).ok()?.item;
    crate::review::rework_reason(store, &task)
}

/// Sends one task's `notifications/claude/channel` event. A send that fails means the client
/// closed the connection; the poller keeps going until `serve` aborts it.
async fn send_event(
    store: &Store,
    peer: &Peer<RoleServer>,
    task_id: &str,
    task: &TaskSnapshot,
    previous: &str,
) {
    let mut content = format!("{task_id} {previous} -> {}", task.full);
    if let Some(reason) = rework_reason(store, task_id) {
        content.push('\n');
        content.push_str(&reason);
    }
    let params = json!({
        "content": content,
        "meta": {
            "task": task_id,
            "plan": task.plan,
            "state": task.state,
            "phase": task.phase,
            "previous": previous,
        },
    });
    send(peer, params).await;
}

/// Sends one plan's `notifications/claude/channel` event. Its `meta` has no `task` key, so a
/// reader tells a plan event from a task event.
async fn send_plan_event(peer: &Peer<RoleServer>, plan_id: &str, event: &PlanEvent) {
    debug_assert!(CHANNEL_PLAN_FIELDS.contains(&event.field));
    let mut content = format!(
        "{plan_id} {} {} -> {}",
        event.field, event.previous, event.value
    );
    if let Some(reason) = &event.reason {
        content.push('\n');
        content.push_str(reason);
    }
    let params = json!({
        "content": content,
        "meta": {
            "plan": plan_id,
            "field": event.field,
            "previous": event.previous,
            "value": event.value,
        },
    });
    send(peer, params).await;
}

async fn send(peer: &Peer<RoleServer>, params: Value) {
    let notification = CustomNotification::new(CHANNEL_METHOD, Some(params));
    let _ = peer
        .send_notification(ServerNotification::CustomNotification(notification))
        .await;
}

/// The channel's poller: read once as the baseline (nothing is sent for it), then every
/// [`CHANNEL_INTERVAL_SECS`] seconds. A failed read sends nothing that round and is retried
/// next round; it never ends the server and prints nothing.
async fn poll(store: Store, peer: Peer<RoleServer>) {
    let mut previous: Option<Snapshot> = None;
    loop {
        if let Ok(mut current) = snapshot(&store) {
            carry_pause_records(
                previous.as_ref().map(|before| &before.plans),
                &mut current.plans,
            );
            if let Some(before) = &previous {
                for (task_id, task, phase) in task_transitions(&before.tasks, &current.tasks) {
                    send_event(&store, &peer, &task_id, &task, &phase).await;
                }
                for (plan_id, event) in plan_transitions(&before.plans, &current.plans) {
                    send_plan_event(&peer, &plan_id, &event).await;
                }
            }
            previous = Some(current);
        }
        tokio::time::sleep(Duration::from_secs(CHANNEL_INTERVAL_SECS)).await;
    }
}

/// Serves `store` over standard input and output until the client closes the connection. A
/// tool error never stops the server: it is answered as that call's error result. With
/// `claude_experimental` the channel's poller runs beside the service and stops with it.
pub async fn serve(store: Store, claude_experimental: bool) -> Result<(), String> {
    let server = if claude_experimental {
        ArcServer::with_channel(store.clone())
    } else {
        ArcServer::new(store.clone())
    };
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| format!("the MCP handshake failed: {error}"))?;
    let poller = claude_experimental.then(|| {
        let peer = service.peer().clone();
        tokio::spawn(poll(store.clone(), peer))
    });
    let result = service.waiting().await;
    if let Some(poller) = poller {
        poller.abort();
    }
    result.map_err(|error| format!("the MCP server stopped unexpectedly: {error}"))?;
    Ok(())
}

/// [`serve`] on a runtime of its own, for the synchronous CLI entry.
pub fn run(store: Store, claude_experimental: bool) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("cannot start the MCP runtime: {error}"))?;
    runtime.block_on(serve(store, claude_experimental))
}

/// One call's arguments, typed. Every error names the argument and what is wrong with it,
/// and [`Args::finish`] refuses an argument the tool does not take.
struct Args {
    values: BTreeMap<String, Value>,
}

impl Args {
    fn new(arguments: Option<Map<String, Value>>) -> Self {
        Self {
            values: arguments.unwrap_or_default().into_iter().collect(),
        }
    }

    fn take(&mut self, key: &str) -> Option<Value> {
        self.values.remove(key).filter(|value| !value.is_null())
    }

    fn required_string(&mut self, key: &str) -> Result<String, String> {
        match self.take(key) {
            Some(Value::String(text)) => Ok(text),
            Some(_) => Err(format!("invalid arguments: `{key}` must be a string")),
            None => Err(format!("invalid arguments: `{key}` is required")),
        }
    }

    fn optional_string(&mut self, key: &str) -> Result<Option<String>, String> {
        match self.take(key) {
            Some(Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(format!("invalid arguments: `{key}` must be a string")),
            None => Ok(None),
        }
    }

    fn optional_bool(&mut self, key: &str) -> Result<bool, String> {
        Ok(self.optional_flag(key)?.unwrap_or(false))
    }

    /// [`Args::optional_bool`] that keeps `false` apart from an absent argument.
    fn optional_flag(&mut self, key: &str) -> Result<Option<bool>, String> {
        match self.take(key) {
            Some(Value::Bool(value)) => Ok(Some(value)),
            Some(_) => Err(format!("invalid arguments: `{key}` must be true or false")),
            None => Ok(None),
        }
    }

    fn optional_strings(&mut self, key: &str) -> Result<Vec<String>, String> {
        Ok(self.optional_strings_opt(key)?.unwrap_or_default())
    }

    /// [`Args::optional_strings`] that keeps an empty list apart from an absent argument.
    fn optional_strings_opt(&mut self, key: &str) -> Result<Option<Vec<String>>, String> {
        match self.take(key) {
            Some(Value::Array(values)) => values
                .into_iter()
                .map(|value| match value {
                    Value::String(text) => Ok(text),
                    _ => Err(format!(
                        "invalid arguments: `{key}` must be a list of strings"
                    )),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some),
            Some(_) => Err(format!(
                "invalid arguments: `{key}` must be a list of strings"
            )),
            None => Ok(None),
        }
    }

    fn finish(self) -> Result<(), String> {
        if self.values.is_empty() {
            return Ok(());
        }
        let names: Vec<&str> = self.values.keys().map(String::as_str).collect();
        Err(format!("invalid arguments: unknown {}", names.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(value: Value) -> Args {
        Args::new(value.as_object().cloned())
    }

    #[test]
    fn an_argument_of_the_wrong_type_is_named() {
        let mut args = args_of(json!({"slug": 1}));
        let error = args
            .required_string("slug")
            .expect_err("a number is not a string");
        assert!(error.contains("`slug`"), "{error}");
        let mut args = args_of(json!({"after": "TK0001-a"}));
        assert!(args.optional_strings("after").is_err());
        let mut args = args_of(json!({"allow_noop": "yes"}));
        assert!(args.optional_bool("allow_noop").is_err());
    }

    #[test]
    fn a_missing_required_argument_and_an_unknown_one_are_refused() {
        let mut args = args_of(json!({}));
        let error = args.required_string("plan").expect_err("plan is required");
        assert!(error.contains("required"), "{error}");
        let args = args_of(json!({"plan": "PL0001-a", "nope": true}));
        let error = args.finish().expect_err("nope is not an argument");
        assert!(error.contains("nope"), "{error}");
        assert_eq!(args_of(json!({})).finish(), Ok(()));
    }

    #[test]
    fn a_null_is_the_same_as_absent() {
        let mut args = args_of(json!({"after": null, "allow_noop": null}));
        assert_eq!(args.optional_strings("after"), Ok(Vec::new()));
        assert!(!args.optional_bool("allow_noop").expect("absent"));
        assert_eq!(args.finish(), Ok(()));
    }

    #[test]
    fn a_result_carries_structured_content_only_when_the_value_is_an_object() {
        let object = success_result(json!({"id": "PL0001-a"}));
        assert_eq!(object.is_error, Some(false));
        assert_eq!(
            object.structured_content,
            Some(json!({"id": "PL0001-a"})),
            "an object is structured"
        );
        assert_eq!(
            text_of(&object),
            object
                .structured_content
                .as_ref()
                .expect("the object")
                .to_string(),
            "the text is the same JSON"
        );

        // `list`'s document is an array; a string and a null are as far from an object. Each
        // is text only, so a client that requires an object in `structuredContent` still
        // reads the result.
        for value in [
            json!([]),
            json!([{"id": "PL0001-a"}]),
            json!("text"),
            Value::Null,
        ] {
            let result = success_result(value.clone());
            assert_eq!(result.structured_content, None, "{value}");
            assert_eq!(result.is_error, Some(false), "{value}");
            assert_eq!(text_of(&result), value.to_string(), "{value}");
        }
    }

    #[test]
    fn a_pause_record_seen_while_the_plan_still_ran_is_kept_for_its_pause() {
        let record = "the plan paused: task TK0001-a used 3 automatic reworks";
        let plan = |paused: bool, record: Option<(&str, bool)>| {
            BTreeMap::from([(
                "PL0001-a".to_owned(),
                PlanSnapshot {
                    status: "confirmed".to_owned(),
                    paused,
                    cancelled: false,
                    pause_record: record.map(|(text, pending)| PauseRecord {
                        id: "TK0001-a@0@2026-01-01T00:00:00Z".to_owned(),
                        text: text.to_owned(),
                        pending,
                    }),
                },
            )])
        };

        // The automatic rework limit writes the record first and sets `auto` to `off` after
        // marking it applied. The read in that window finds a pending record on a plan that is
        // not paused; the paused read after it must still report the record as the pause's
        // reason.
        let before = plan(false, None);
        let mut racing = plan(false, Some((record, true)));
        carry_pause_records(Some(&before), &mut racing);
        let paused = plan(true, Some((record, true)));
        let events = plan_transitions(&racing, &paused);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].1.field, "paused");
        assert_eq!(events[0].1.reason.as_deref(), Some(record));

        // The poller's first read can land in that window too: the client connects after
        // the record is written and before `auto` goes `off`. That read has no previous
        // read to take the marker from, so the record's own `pending` marker is what tells
        // it the pause is still to come; the pause that follows reports it.
        let mut baseline = plan(false, Some((record, true)));
        carry_pause_records(None, &mut baseline);
        let paused = plan(true, Some((record, true)));
        let events = plan_transitions(&baseline, &paused);
        assert_eq!(events[0].1.field, "paused");
        assert_eq!(events[0].1.reason.as_deref(), Some(record));

        // A pause the poller already reported is not repeated by a later manual pause: the
        // record carried out of the resume is an applied one.
        let resumed = plan(false, Some((record, false)));
        let manual = plan(true, Some((record, false)));
        assert_eq!(plan_transitions(&resumed, &manual)[0].1.reason, None);

        // A client whose first read lands after that pause was resumed finds the applied
        // record on a running plan. It belongs to the pause the client missed, not to the
        // owner's manual pause that follows.
        let mut stale = plan(false, Some((record, false)));
        carry_pause_records(None, &mut stale);
        let manual = plan(true, Some((record, false)));
        assert_eq!(
            plan_transitions(&stale, &manual)[0].1.reason,
            None,
            "a record an earlier pause left behind is not a manual pause's reason"
        );

        // A plan write that failed leaves the record marked applied on a plan that still
        // runs. A read that carried the record as pending must not keep it pending: no pause
        // is left to report, so the owner's next pause sends no second line.
        let carried = plan(false, Some((record, true)));
        let mut failed = plan(false, Some((record, false)));
        carry_pause_records(Some(&carried), &mut failed);
        let manual = plan(true, Some((record, false)));
        assert_eq!(plan_transitions(&failed, &manual)[0].1.reason, None);

        // The other way round: a pause the poller reported stays reported even when the
        // store still marks its record pending (the write that clears the marker failed).
        let reported = plan(false, Some((record, false)));
        let mut still_pending = plan(false, Some((record, true)));
        carry_pause_records(Some(&reported), &mut still_pending);
        let manual = plan(true, Some((record, false)));
        assert_eq!(plan_transitions(&still_pending, &manual)[0].1.reason, None);
    }

    /// The text blocks of a result, joined as a client reads them.
    fn text_of(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|block| block.as_text().map(|text| text.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}
