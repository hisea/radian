//! The ACP version 1 wire shapes this host uses (ADR 0049).
//!
//! The protocol schema is the authority: field names are camelCase, the update and tool enums
//! are snake_case, and `_meta` is spelled with the leading underscore. The host implements a
//! closed subset — initialize, session/new, session/load, session/prompt, session/cancel,
//! session/update and session/request_permission — and answers any other method with a
//! JSON-RPC method-not-found error.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The one protocol version this host speaks.
pub(crate) const PROTOCOL_VERSION: u64 = 1;

/// JSON-RPC error codes.
pub(crate) const PARSE_ERROR: i64 = -32700;
pub(crate) const METHOD_NOT_FOUND: i64 = -32601;
pub(crate) const INVALID_PARAMS: i64 = -32602;
pub(crate) const INTERNAL_ERROR: i64 = -32603;

/// The permission option ids the host offers for a tool approval. They are also the wire
/// values of ADR 0012's three outcomes.
pub(crate) const ALLOW_ONCE: &str = "allow_once";
pub(crate) const ALLOW_ALWAYS: &str = "allow_always";
pub(crate) const REJECT_ONCE: &str = "reject_once";

/// The three approval options, in the order the protocol lists them.
pub(crate) fn approval_options() -> Value {
    json!([
        { "optionId": ALLOW_ONCE, "name": "Allow once", "kind": "allow_once" },
        { "optionId": ALLOW_ALWAYS, "name": "Allow for this session", "kind": "allow_always" },
        { "optionId": REJECT_ONCE, "name": "Reject", "kind": "reject_once" },
    ])
}

/// Parses the `mcpServers` of `session/new`. Only stdio servers are accepted; an HTTP, SSE or
/// ACP server is an error that names the transport. The returned load is merged at the
/// `--mcp-config` level by the shared opener.
pub(crate) fn parse_mcp_servers(servers: &[Value]) -> Result<radian_host::McpLoad, String> {
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    let mut load = radian_host::McpLoad::default();
    let mut seen = BTreeSet::new();
    for (index, server) in servers.iter().enumerate() {
        let object = server
            .as_object()
            .ok_or_else(|| format!("mcpServers[{index}] is not an object"))?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("mcpServers[{index}] has no name"))?
            .to_owned();
        if !seen.insert(name.clone()) {
            return Err(format!(
                "MCP server `{name}` is named more than once; names prefix tool ids and must be \
                 unique"
            ));
        }
        let kind = object.get("type").and_then(Value::as_str);
        if let Some(kind) = kind
            && kind != "stdio"
        {
            return Err(format!(
                "MCP server `{name}` uses the `{kind}` transport, which is not supported; only \
                 stdio MCP servers run"
            ));
        }
        if object.get("url").is_some_and(|url| !url.is_null()) {
            return Err(format!(
                "MCP server `{name}` is remote (it has a `url`), which is not supported; only \
                 stdio MCP servers run"
            ));
        }
        let command = object
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("MCP server `{name}` has no command"))?;
        let mut config = agent_extensions::McpServerConfig::new(&name, command);
        if let Some(args) = object.get("args").filter(|value| !value.is_null()) {
            let args = args
                .as_array()
                .ok_or_else(|| format!("MCP server `{name}`: args is not an array"))?;
            for arg in args {
                let arg = arg
                    .as_str()
                    .ok_or_else(|| format!("MCP server `{name}`: an arg is not a string"))?;
                config.args.push(OsString::from(arg));
            }
        }
        if let Some(env) = object.get("env").filter(|value| !value.is_null()) {
            let env = env
                .as_array()
                .ok_or_else(|| format!("MCP server `{name}`: env is not an array"))?;
            for variable in env {
                let variable_name = variable
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("MCP server `{name}`: an env entry has no name"))?;
                let value = variable
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        format!("MCP server `{name}`: env `{variable_name}` has no value")
                    })?;
                config
                    .env
                    .push((OsString::from(variable_name), OsString::from(value)));
            }
        }
        // The shared `mcpServers` shape gives a foreign server HOME; the ACP client supplies
        // the variables it wants, and PATH is always copied by the MCP process layer.
        config.inherit_env = vec![OsString::from("HOME")];
        load.servers.push(config);
    }
    Ok(load)
}

/// An `agent_message_chunk` or `agent_thought_chunk` update carrying one text block.
pub(crate) fn text_chunk(kind: &str, text: &str) -> Value {
    json!({
        "sessionUpdate": kind,
        "content": { "type": "text", "text": text },
    })
}

/// The tool kind for a built-in tool name.
pub(crate) fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "skill" => "read",
        "write" | "patch" => "edit",
        "glob" | "grep" | "memory_search" => "search",
        "shell" | "job" => "execute",
        _ => "other",
    }
}

/// A short human title for a tool call: the tool's main argument when it has one.
pub(crate) fn tool_title(name: &str, arguments: &Value) -> String {
    if let Some(command) = arguments.get("command").and_then(Value::as_str) {
        return format!("{name}: {command}");
    }
    if let Some(path) = argument_path(arguments) {
        return format!("{name}: {}", path.display());
    }
    name.to_owned()
}

/// The path a tool call names, if it names one.
fn argument_path(arguments: &Value) -> Option<PathBuf> {
    ["path", "file_path", "file"]
        .iter()
        .find_map(|key| arguments.get(*key).and_then(Value::as_str))
        .map(PathBuf::from)
}

/// The file locations a tool call touches, resolved against the workspace. A relative path is
/// joined to the workspace; no path means no locations.
pub(crate) fn tool_locations(arguments: &Value, workspace: &Path) -> Value {
    let Some(path) = argument_path(arguments) else {
        return json!([]);
    };
    let path = if path.is_absolute() {
        path
    } else {
        workspace.join(path)
    };
    let mut location = json!({ "path": path });
    if let Some(line) = arguments
        .get("line")
        .or_else(|| arguments.get("offset"))
        .and_then(Value::as_u64)
        .and_then(|line| u32::try_from(line).ok())
    {
        location["line"] = line.into();
    }
    json!([location])
}

/// A `tool_call` update for a started call.
pub(crate) fn tool_call(call_id: &str, name: &str, arguments: &Value, workspace: &Path) -> Value {
    json!({
        "sessionUpdate": "tool_call",
        "toolCallId": call_id,
        "title": tool_title(name, arguments),
        "kind": tool_kind(name),
        "status": "in_progress",
        "locations": tool_locations(arguments, workspace),
        "rawInput": arguments,
    })
}

/// The `rawOutput` of a `tool_call_update`: the result's text, plus its structured output and
/// its machine-readable error code when the result has them. The live sink and the
/// `session/load` replay both use it, so a replayed call reports what a live client saw.
pub(crate) fn tool_output(result: &radian_core::ToolResult) -> Value {
    let mut raw = serde_json::Map::new();
    raw.insert("text".to_owned(), Value::String(result.text()));
    if let Some(structured) = &result.structured {
        raw.insert("structured".to_owned(), structured.clone());
    }
    if let Some(code) = &result.error_code {
        raw.insert("code".to_owned(), Value::String(code.clone()));
    }
    Value::Object(raw)
}

/// A `tool_call_update` for a finished call. `raw_output` is present when the tool result is
/// known; a status-only update omits it.
pub(crate) fn tool_call_update(
    call_id: &str,
    name: &str,
    is_error: bool,
    raw_output: Option<Value>,
) -> Value {
    let mut update = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": call_id,
        "title": name,
        "status": if is_error { "failed" } else { "completed" },
    });
    if let Some(output) = raw_output {
        update["rawOutput"] = output;
    }
    update
}

/// The `toolCall` object of a `session/request_permission`.
pub(crate) fn permission_tool_call(
    call_id: &str,
    title: &str,
    kind: &str,
    raw_input: Value,
    locations: Value,
) -> Value {
    json!({
        "toolCallId": call_id,
        "title": title,
        "kind": kind,
        "status": "pending",
        "locations": locations,
        "rawInput": raw_input,
    })
}

/// The result of `initialize`: version 1, `session/load` supported, no client filesystem or
/// terminal dependence.
pub(crate) fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": {
                "image": false,
                "audio": false,
                "embeddedContext": false,
            },
        },
        "authMethods": [],
        "agentInfo": {
            "name": "radian",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// One incoming prompt content block as plain text. Text is required; a resource link becomes
/// a line naming its label and URI. Any other block is refused because this host advertises no
/// capability for it.
pub(crate) fn prompt_text(blocks: &[Value]) -> Result<String, String> {
    let mut text = String::new();
    for block in blocks {
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "text" => {
                if let Some(chunk) = block.get("text").and_then(Value::as_str) {
                    text.push_str(chunk);
                }
            }
            "resource_link" => {
                let uri = block.get("uri").and_then(Value::as_str).unwrap_or_default();
                let name = block.get("name").and_then(Value::as_str).unwrap_or(uri);
                text.push_str(&format!("[{name}]({uri})"));
            }
            other => {
                return Err(format!(
                    "content block `{other}` is not supported; this agent accepts text and \
                     resource links"
                ));
            }
        }
    }
    if text.trim().is_empty() {
        return Err("the prompt carries no text".to_owned());
    }
    Ok(text)
}
