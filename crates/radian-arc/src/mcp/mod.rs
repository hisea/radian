//! `radian arc mcp`: the MCP stdio server the lead writes the backlog through (ADR 0048
//! decisions 2 and 11).
//!
//! The server holds one [`Store`] and answers `tools/list` and `tools/call`. Every call is
//! one of the store's transactions, so it takes `state.lock`, never `runner.lock`, and
//! refuses a plan or task that changed outside arc with that reason in the tool error. That
//! is why the lead can add tasks while a scheduling loop runs (decision 5).
//!
//! The six tools are in [`tools`]; there is no tool to confirm, start, merge, reject,
//! change the auto level, or approve (decision 11).

pub mod tools;

use std::borrow::Cow;
use std::collections::BTreeMap;

pub use tools::{TOOLS, ToolDef, names};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt};
use serde_json::{Map, Value, json};

use crate::store::{self, Error, Store};

/// The MCP server for one repository's arc store.
#[derive(Debug, Clone)]
pub struct ArcServer {
    store: Store,
}

impl ArcServer {
    /// A server over `store`.
    pub fn new(store: Store) -> Self {
        Self { store }
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
                store::cli::list_json(&self.store, plan.as_deref()).map_err(reason)
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
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("radian-arc", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "arc is the plan and task backlog of this repository. Write it with these \
                 tools. Confirming a plan, starting a task, merging, and the auto level are \
                 the owner's; you have no tool for them.",
            )
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
        let result = match self.dispatch(request.name.as_ref(), request.arguments) {
            Ok(value) => CallToolResult::structured(value),
            Err(message) => CallToolResult::error(vec![ContentBlock::text(message)]),
        };
        std::future::ready(Ok(result.into()))
    }
}

/// Serves `store` over standard input and output until the client closes the connection. A
/// tool error never stops the server: it is answered as that call's error result.
pub async fn serve(store: Store) -> Result<(), String> {
    let service = ArcServer::new(store)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| format!("the MCP handshake failed: {error}"))?;
    service
        .waiting()
        .await
        .map_err(|error| format!("the MCP server stopped unexpectedly: {error}"))?;
    Ok(())
}

/// [`serve`] on a runtime of its own, for the synchronous CLI entry.
pub fn run(store: Store) -> Result<(), String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("cannot start the MCP runtime: {error}"))?;
    runtime.block_on(serve(store))
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
        match self.take(key) {
            Some(Value::Bool(value)) => Ok(value),
            Some(_) => Err(format!("invalid arguments: `{key}` must be true or false")),
            None => Ok(false),
        }
    }

    fn optional_strings(&mut self, key: &str) -> Result<Vec<String>, String> {
        match self.take(key) {
            Some(Value::Array(values)) => values
                .into_iter()
                .map(|value| match value {
                    Value::String(text) => Ok(text),
                    _ => Err(format!(
                        "invalid arguments: `{key}` must be a list of strings"
                    )),
                })
                .collect(),
            Some(_) => Err(format!(
                "invalid arguments: `{key}` must be a list of strings"
            )),
            None => Ok(Vec::new()),
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
}
