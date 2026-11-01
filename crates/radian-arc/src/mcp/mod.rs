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
use crate::store::{self, Error, Store};
use crate::transitions::{PlanEvent, Poller, Repo, TaskSnapshot, Transition};

/// The experiment capability Claude Code reads a channel declaration from.
pub const CHANNEL_CAPABILITY: &str = "claude/channel";
/// The capability the server does **not** declare: it never relays permission prompts.
pub const CHANNEL_PERMISSION_CAPABILITY: &str = "claude/channel/permission";
/// The notification method a channel event is sent with.
pub const CHANNEL_METHOD: &str = "notifications/claude/channel";
/// The newest protocol version the flagged server supports. A revision after this one (such
/// as `2026-07-28`) cannot carry channel messages, so Claude Code does not register a server
/// that negotiates it as a channel.
pub const CHANNEL_MAX_VERSION: ProtocolVersion = ProtocolVersion::V_2025_11_25;

/// The closed transition lists and the poll cadence, shared with the hub's event stream (ADR
/// 0048 decisions 11 and 16). They stay re-exported here because the manual's drift test and
/// the channel's own tests read them from the MCP module; the poller itself lives in
/// [`crate::transitions`].
pub use crate::transitions::{
    CHANNEL_INTERVAL_SECS, CHANNEL_PHASES, CHANNEL_PLAN_FIELDS, CHANNEL_READY_FROM,
};

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
                let allow_noop = args.optional_flag("allow_noop")?;
                let extra_stages = args.optional_strings("extra_stages")?;
                let flow = args.optional_string("flow")?;
                let review_fix_level = args.optional_string("review_fix_level")?;
                args.finish()?;
                let id = self
                    .store
                    .add_task_proposed_with_fix_level(
                        &plan,
                        &slug,
                        &title,
                        after,
                        allow_noop,
                        extra_stages,
                        &brief,
                        flow.as_deref(),
                        review_fix_level.as_deref(),
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
                let flow = args.optional_string("flow")?;
                let review_fix_level = args.optional_nullable_string("review_fix_level")?;
                args.finish()?;
                store::plan_ops::update_task_with_flow(
                    &self.store,
                    &task,
                    title.as_deref(),
                    brief.as_deref(),
                    after,
                    allow_noop,
                    extra_stages,
                    flow.as_deref(),
                    review_fix_level.as_ref().map(|level| level.as_deref()),
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

/// The channel's poller: one store, read once as the baseline (nothing is sent for it), then
/// every [`CHANNEL_INTERVAL_SECS`] seconds. The transitions and the baseline rule are the
/// shared poller's ([`crate::transitions::Poller`]), so the channel and the hub's event
/// stream can never disagree about which change is sent.
async fn poll(store: Store, peer: Peer<RoleServer>) {
    let repo = Repo {
        name: String::new(),
        store: store.clone(),
    };
    let mut poller = Poller::new();
    loop {
        for (_, transition) in poller.poll(std::slice::from_ref(&repo)) {
            match transition {
                Transition::Task { id, task, previous } => {
                    send_event(&store, &peer, &id, &task, &previous).await
                }
                Transition::Plan { id, event } => send_plan_event(&peer, &id, &event).await,
            }
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

    fn optional_flag(&mut self, key: &str) -> Result<Option<bool>, String> {
        match self.take(key) {
            Some(Value::Bool(value)) => Ok(Some(value)),
            Some(_) => Err(format!("invalid arguments: `{key}` must be true or false")),
            None => Ok(None),
        }
    }

    /// [`Args::optional_string`] that tells an absent argument from an explicit `null`: `None`
    /// is absent, `Some(None)` is a `null`, and `Some(Some(text))` is a string. `update_task`
    /// reads `review_fix_level` this way, because a `null` clears the task's own level.
    fn optional_nullable_string(&mut self, key: &str) -> Result<Option<Option<String>>, String> {
        match self.values.remove(key) {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(None)),
            Some(Value::String(text)) => Ok(Some(Some(text))),
            Some(_) => Err(format!(
                "invalid arguments: `{key}` must be a string or null"
            )),
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
        assert!(args.optional_flag("allow_noop").is_err());
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
        assert_eq!(args.optional_flag("allow_noop"), Ok(None));
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
