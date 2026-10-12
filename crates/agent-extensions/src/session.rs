use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::{
    AgentEvent, ContentPart, OpaqueJson, ToolCall, ToolConcurrency, ToolContext, ToolError,
    ToolFailure, ToolOutput, ToolProvider, ToolSession, ToolSpec,
};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, ClientRequest, PingRequest, ProgressNotificationParam,
    ProtocolVersion, ServerResult,
};
use rmcp::service::{
    ClientLifecycleMode, ClientServiceExt, PeerRequestOptions, RequestHandle, RoleClient,
    RunningService, ServiceError,
};
use serde_json::{Value, json};

use crate::alias::tool_alias;
use crate::client::{Client, ProgressQueue, ProgressRouter};
use crate::config::McpServerConfig;
use crate::process::ServerProcess;

/// How long to wait for a killed server to be reaped.
const REAP_LIMIT: Duration = Duration::from_secs(2);

/// Opens one set of MCP server processes per `AgentSession`.
#[derive(Debug, Clone)]
pub struct McpToolProvider {
    servers: Arc<Vec<McpServerConfig>>,
}

impl McpToolProvider {
    pub fn new(servers: Vec<McpServerConfig>) -> Self {
        Self {
            servers: Arc::new(servers),
        }
    }
}

impl ToolProvider for McpToolProvider {
    type Session = McpSession;

    /// Starts every server and lists its tools. If any server fails, or two tools map to the
    /// same alias, the servers already started are stopped before the error is returned.
    async fn open_session(&self) -> Result<McpSession, ToolError> {
        let mut servers = Vec::with_capacity(self.servers.len());
        for config in self.servers.iter() {
            match Server::start(config.clone()).await {
                Ok(server) => servers.push(server),
                Err(e) => {
                    stop_all(&mut servers).await;
                    return Err(e);
                }
            }
        }
        match routes(&servers) {
            Ok((specs, routes)) => Ok(McpSession {
                servers,
                specs,
                routes,
            }),
            Err(e) => {
                stop_all(&mut servers).await;
                Err(e)
            }
        }
    }
}

struct Route {
    server: usize,
    /// The tool's MCP name.
    name: String,
}

/// The MCP servers of one `AgentSession`. They live across turns, so server-side state
/// persists for the conversation.
pub struct McpSession {
    servers: Vec<Server>,
    specs: Vec<ToolSpec>,
    routes: HashMap<String, Route>,
}

impl ToolSession for McpSession {
    fn tool_specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    async fn execute(
        &self,
        call: &ToolCall,
        cx: ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        let Some(route) = self.routes.get(&call.name) else {
            return Err(ToolFailure::new(
                "unknown_tool",
                format!("no tool named {}", call.name),
            ));
        };
        self.servers[route.server]
            .call(&route.name, call, &cx)
            .await
    }

    /// Closes each server's stdin and waits for it to exit, killing it after
    /// `shutdown_timeout`. A server that could not be reaped is retried on the next call.
    async fn close(&mut self) -> Result<(), ToolError> {
        let mut first_error = None;
        for server in &mut self.servers {
            if let Err(e) = server.close().await {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl McpSession {
    /// Not part of the public API: lets the integration tests observe that no call leaves
    /// its progress route behind. `#[cfg(test)]` cannot reach integration tests.
    #[doc(hidden)]
    pub fn progress_route_count(&self) -> usize {
        self.servers
            .iter()
            .map(|s| s.progress_router.route_count())
            .sum()
    }
}

struct Server {
    config: McpServerConfig,
    service: Option<RunningService<RoleClient, Client>>,
    process: ServerProcess,
    tools: Vec<Value>,
    /// Why the server can no longer be used in this session, if it cannot.
    unavailable: Mutex<Option<String>>,
    /// Shared with the `Client` handler, which routes progress notifications here by token.
    progress_router: Arc<ProgressRouter>,
    closed: bool,
}

impl Server {
    async fn start(config: McpServerConfig) -> Result<Self, ToolError> {
        let id = config.id.clone();
        let failed = |what: &str, e: &dyn std::fmt::Display| {
            ToolError(format!("MCP server {id}: {what}: {e}"))
        };
        let (process, stdout, stdin) =
            ServerProcess::spawn(&config).map_err(|e| failed("failed to start", &e))?;
        let progress_router = Arc::new(ProgressRouter::default());
        // rmcp's plain `serve()` would use only the legacy `initialize` handshake. `Auto` tries
        // `server/discover` (2026-07-28) and falls back to `initialize` (2025-11-25).
        let lifecycle = ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        };
        let startup = async {
            let service = Client::new(Arc::clone(&progress_router))
                .serve_with_lifecycle((stdout, stdin), lifecycle)
                .await
                .map_err(|e| failed("handshake failed", &e))?;
            let tools = service
                .list_all_tools()
                .await
                .map_err(|e| failed("tools/list failed", &e))?;
            let tools = tools
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| failed("unreadable tool list", &e))?;
            Ok::<_, ToolError>((service, tools))
        };
        let started = tokio::time::timeout(config.startup_timeout, startup)
            .await
            .unwrap_or_else(|_| {
                Err(failed(
                    "startup timed out after",
                    &format!("{:?}", config.startup_timeout),
                ))
            });
        match started {
            Ok((service, tools)) => Ok(Self {
                config,
                service: Some(service),
                process,
                tools,
                unavailable: Mutex::new(None),
                progress_router,
                closed: false,
            }),
            Err(e) => {
                process.kill_and_reap(REAP_LIMIT).await;
                Err(e)
            }
        }
    }

    fn unavailable(&self) -> Option<String> {
        self.unavailable
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn mark_unavailable(&self, reason: impl Into<String>) {
        self.unavailable
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(|| reason.into());
    }

    fn unavailable_failure(&self, reason: &str) -> ToolFailure {
        ToolFailure::new(
            "extension_unavailable",
            format!(
                "The MCP server `{}` is no longer available in this conversation ({reason}). \
                 Do not call its tools again.",
                self.config.id
            ),
        )
    }

    async fn call(
        &self,
        name: &str,
        call: &ToolCall,
        cx: &ToolContext<'_>,
    ) -> Result<ToolOutput, ToolFailure> {
        if let Some(reason) = self.unavailable() {
            return Err(self.unavailable_failure(&reason));
        }
        let Some(service) = &self.service else {
            return Err(self.unavailable_failure("the session is closed"));
        };
        let mut params = CallToolRequestParams::new(name.to_owned());
        params.arguments = call.arguments.as_object().cloned();
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params));
        let mut handle = match service
            .peer()
            .send_cancellable_request(request, PeerRequestOptions::no_options())
            .await
        {
            Ok(handle) => handle,
            Err(e) => {
                let reason = format!("sending the request failed: {e}");
                self.mark_unavailable(&reason);
                self.process.kill_tree();
                return Err(self.unavailable_failure(&reason));
            }
        };

        // The request is already on the wire when `send_cancellable_request` returns, so the
        // server may have sent progress before this point; whatever the router buffered for
        // this token is replayed into the queue by `register`, under the same lock
        // `on_progress` delivers through, so nothing is missed. The queue is bounded, so a
        // server that floods progress cannot grow host memory; see `PROGRESS_QUEUE_LIMIT`.
        let progress = Arc::new(ProgressQueue::default());
        self.progress_router
            .register(handle.progress_token.clone(), Arc::clone(&progress));
        // If this future is dropped mid-call (the turn was poisoned), kill the server now.
        let mut guard = AbortGuard {
            server: self,
            armed: true,
        };
        // Removes the registration when the call ends, whenever that happens.
        let _progress = ProgressGuard {
            router: &self.progress_router,
            token: handle.progress_token.clone(),
        };
        enum Waited<T> {
            Response(T),
            Cancelled,
            TimedOut,
        }
        // rmcp routes each notification on its own spawned task and hands the response straight
        // to the waiter, so a notification can be delivered out of order, or after the reply
        // was taken. Order within a call is restored here; tail loss is accepted.
        let mut monotone = ProgressMonotone::default();
        let timeout = tokio::time::sleep(self.config.call_timeout);
        tokio::pin!(timeout);
        let waited = loop {
            tokio::select! {
                biased;
                () = cx.cancellation.cancelled() => break Waited::Cancelled,
                // Pinned outside the loop: recreating it per notification would reset the
                // deadline whenever progress flows.
                () = &mut timeout => break Waited::TimedOut,
                // Polled before the response, which trims (but cannot eliminate) tail loss.
                notification = progress.recv() => {
                    if monotone.admit(&notification) {
                        cx.events.emit(AgentEvent::ToolProgress {
                            index: cx.index,
                            progress: progress_json(&notification),
                        });
                    }
                }
                response = &mut handle.rx => break Waited::Response(response),
            }
        };
        let response = match waited {
            Waited::Response(response) => response,
            Waited::Cancelled | Waited::TimedOut => {
                let timed_out = matches!(waited, Waited::TimedOut);
                let what = if timed_out {
                    "timed out"
                } else {
                    "was cancelled"
                };
                self.cancel_and_stop(service, handle, what).await;
                guard.armed = false;
                const STOPPED: &str = "The server was stopped and its tools are unavailable \
                                       for the rest of the conversation.";
                return Err(if timed_out {
                    ToolFailure::new(
                        "extension_timeout",
                        format!(
                            "The tool did not finish within {:?}. {STOPPED}",
                            self.config.call_timeout
                        ),
                    )
                } else {
                    ToolFailure::new("cancelled", format!("The call was cancelled. {STOPPED}"))
                });
            }
        };
        guard.armed = false;
        match response {
            Ok(Ok(result)) => map_result(result),
            // The server answered with a JSON-RPC error; it is still healthy.
            Ok(Err(ServiceError::McpError(error))) => Err(ToolFailure::new(
                "extension_error",
                error.message.to_string(),
            )),
            Ok(Err(e)) => Err(self.fail(format!("protocol error: {e}")).await),
            Err(_closed) => Err(self.fail("the server closed the connection".into()).await),
        }
    }

    /// Cancels a call that timed out or was cancelled, then stops the whole server.
    ///
    /// A cancelled request gets no reply, and a server that still answers `ping` may well be
    /// running the tool in the background. Reusing it could interleave the abandoned work with
    /// the next call, which is unsafe for stateful servers (browsers, databases, REPLs). So the
    /// server is always stopped; only a short grace period is spent first so that the
    /// notification is written and read: the `ping` reply shows the server got past it.
    async fn cancel_and_stop(
        &self,
        service: &RunningService<RoleClient, Client>,
        handle: RequestHandle<RoleClient>,
        what: &str,
    ) {
        let _ = tokio::time::timeout(self.config.cancel_grace, async {
            let _ = handle.cancel(Some(format!("the call {what}"))).await;
            let _ = service
                .peer()
                .send_request(ClientRequest::PingRequest(PingRequest::default()))
                .await;
        })
        .await;
        self.fail(format!("a call {what}, so the server was stopped"))
            .await;
    }

    /// Marks the server unavailable, kills its process tree and waits for it to be reaped.
    async fn fail(&self, reason: String) -> ToolFailure {
        self.mark_unavailable(&reason);
        self.process.kill_and_reap(REAP_LIMIT).await;
        self.unavailable_failure(&reason)
    }

    async fn close(&mut self) -> Result<(), ToolError> {
        if self.closed {
            return Ok(());
        }
        self.mark_unavailable("the session is closed");
        if let Some(service) = self.service.take() {
            // Stops the client loop and drops the transport, which closes the server's stdin.
            let _ = service.cancel().await;
        }
        if !self.process.wait_exit(self.config.shutdown_timeout).await {
            self.process.kill_tree();
        }
        // Also reach anything the server started and left behind.
        self.process.kill_tree();
        if !self.process.wait_exit(REAP_LIMIT).await {
            return Err(ToolError(format!(
                "MCP server {} (pid {}) could not be reaped",
                self.config.id,
                self.process.pid()
            )));
        }
        self.closed = true;
        Ok(())
    }
}

/// Kills the server if a call's future is dropped before it settles.
struct AbortGuard<'a> {
    server: &'a Server,
    armed: bool,
}

impl Drop for AbortGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.server.mark_unavailable("a call was interrupted");
            self.server.process.kill_tree();
        }
    }
}

/// Unregisters a call's progress route when it settles or is dropped, so a late notification
/// finds no route and a dropped call leaks no entry.
struct ProgressGuard<'a> {
    router: &'a Arc<ProgressRouter>,
    token: rmcp::model::ProgressToken,
}

impl Drop for ProgressGuard<'_> {
    fn drop(&mut self) {
        self.router.unregister(&self.token);
    }
}

/// The model-visible form of a progress notification: always the current value, `total` and
/// `message` only when the server sent them.
fn progress_json(notification: &ProgressNotificationParam) -> Value {
    let mut progress = json!({ "progress": notification.progress });
    if let Some(total) = notification.total {
        progress["total"] = json!(total);
    }
    if let Some(message) = &notification.message {
        progress["message"] = json!(message);
    }
    progress
}

/// The spec requires a token's `progress` values to rise monotonically. rmcp routes every
/// notification on its own spawned task, so they can arrive out of order; a value that does
/// not beat what was already emitted is stale and dropped.
#[derive(Debug, Default)]
struct ProgressMonotone {
    last: Option<f64>,
}

impl ProgressMonotone {
    fn admit(&mut self, notification: &ProgressNotificationParam) -> bool {
        if self.last.is_some_and(|last| notification.progress <= last) {
            return false;
        }
        self.last = Some(notification.progress);
        true
    }
}

async fn stop_all(servers: &mut [Server]) {
    for server in servers {
        let _ = server.close().await;
    }
}

fn routes(servers: &[Server]) -> Result<(Vec<ToolSpec>, HashMap<String, Route>), ToolError> {
    let mut specs = Vec::new();
    let mut routes: HashMap<String, Route> = HashMap::new();
    for (index, server) in servers.iter().enumerate() {
        let config = &server.config;
        for tool in &server.tools {
            let name = tool["name"].as_str().unwrap_or_default().to_owned();
            let alias = tool_alias(&config.id, &name);
            if let Some(existing) = routes.get(&alias) {
                return Err(ToolError(format!(
                    "tools `{}/{}` and `{}/{}` both map to `{alias}`; rename one server id",
                    servers[existing.server].config.id, existing.name, config.id, name
                )));
            }
            let read_only = tool["annotations"]["readOnlyHint"].as_bool() == Some(true);
            let shared =
                config.shared_tools.contains(&name) || (config.trust_read_only_hint && read_only);
            specs.push(ToolSpec {
                name: alias.clone(),
                description: tool["description"].as_str().unwrap_or_default().to_owned(),
                input_schema: tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object"})),
                concurrency: if shared {
                    ToolConcurrency::Shared
                } else {
                    ToolConcurrency::Exclusive
                },
            });
            routes.insert(
                alias,
                Route {
                    server: index,
                    name,
                },
            );
        }
    }
    Ok((specs, routes))
}

fn map_result(result: ServerResult) -> Result<ToolOutput, ToolFailure> {
    let result = match result {
        ServerResult::CallToolResult(result) => result,
        ServerResult::InputRequiredResult(_) => {
            return Err(ToolFailure::new(
                "extension_unsupported_interaction",
                "The tool asked for user input, which this host does not support.",
            ));
        }
        ServerResult::CreateTaskResult(_) => {
            return Err(ToolFailure::new(
                "extension_unsupported_interaction",
                "The tool started a long-running task, which this host does not support.",
            ));
        }
        _ => {
            return Err(ToolFailure::new(
                "extension_protocol_error",
                "The server answered tools/call with an unexpected result.",
            ));
        }
    };
    // Read the wire form, so new content kinds survive as opaque blocks instead of being
    // dropped by a typed match.
    let value = serde_json::to_value(&result)
        .map_err(|e| ToolFailure::new("extension_protocol_error", e.to_string()))?;
    let content: Vec<ContentPart> = value["content"]
        .as_array()
        .map(|blocks| blocks.iter().map(content_part).collect())
        .unwrap_or_default();
    if value["isError"].as_bool() == Some(true) {
        let message = content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        return Err(ToolFailure::new("tool_error", message));
    }
    Ok(ToolOutput {
        content,
        structured: value.get("structuredContent").cloned(),
    })
}

fn content_part(block: &Value) -> ContentPart {
    let text = |key: &str| block[key].as_str().map(str::to_owned);
    let annotations = block.get("annotations").cloned();
    let meta = block.get("_meta").cloned();
    let known = match block["type"].as_str() {
        Some("text") => text("text").map(|text| ContentPart::Text {
            text,
            annotations,
            meta,
        }),
        Some("image") => {
            text("mimeType")
                .zip(text("data"))
                .map(|(mime_type, data)| ContentPart::Image {
                    mime_type,
                    data,
                    annotations,
                    meta,
                })
        }
        Some("audio") => {
            text("mimeType")
                .zip(text("data"))
                .map(|(mime_type, data)| ContentPart::Audio {
                    mime_type,
                    data,
                    annotations,
                    meta,
                })
        }
        Some("resource_link") => text("uri").map(|uri| ContentPart::ResourceLink {
            uri,
            name: text("name"),
            mime_type: text("mimeType"),
            annotations,
            meta,
        }),
        Some("resource") => {
            block
                .get("resource")
                .cloned()
                .map(|resource| ContentPart::EmbeddedResource {
                    resource,
                    annotations,
                    meta,
                })
        }
        _ => None,
    };
    known.unwrap_or_else(|| ContentPart::Opaque {
        raw: OpaqueJson::parse(block.to_string()).expect("serde_json output is valid JSON"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::NumberOrString;

    fn token(n: i64) -> rmcp::model::ProgressToken {
        rmcp::model::ProgressToken(NumberOrString::Number(n))
    }

    fn notification(progress: f64) -> ProgressNotificationParam {
        ProgressNotificationParam::new(token(1), progress)
    }

    #[test]
    fn out_of_order_progress_is_dropped_to_a_monotone_sequence() {
        let mut monotone = ProgressMonotone::default();
        let admitted: Vec<f64> = [1.0, 3.0, 2.0, 3.0, 4.0]
            .iter()
            .map(|&p| notification(p))
            .filter(|n| monotone.admit(n))
            .map(|n| n.progress)
            .collect();
        assert_eq!(admitted, [1.0, 3.0, 4.0]);
    }

    #[test]
    fn routed_progress_reaches_emission_in_registration_order() {
        // What `call` does, minus the server: the router queue plus the monotone filter.
        let router = crate::client::ProgressRouter::default();
        let queue = Arc::new(ProgressQueue::default());
        // Notifications that beat the registration, in order.
        router.deliver(notification(1.0));
        router.register(token(1), Arc::clone(&queue));
        // ...and ones that find the live route.
        router.deliver(notification(3.0));
        router.deliver(notification(2.0));

        let mut monotone = ProgressMonotone::default();
        let mut emitted = Vec::new();
        while let Some(notification) = queue.try_pop() {
            if monotone.admit(&notification) {
                emitted.push(notification.progress);
            }
        }
        assert_eq!(emitted, [1.0, 3.0]);
    }
}
