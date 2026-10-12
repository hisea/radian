use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

/// How to start one MCP stdio server and how to treat its tools.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Stable id. It prefixes every tool alias, so changing it renames the tools.
    pub id: String,
    pub command: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    /// Variables set for the server. The host environment is not inherited.
    pub env: Vec<(OsString, OsString)>,
    /// Host variables copied through by name, e.g. `HOME`. `PATH` is always copied.
    pub inherit_env: Vec<OsString>,
    /// MCP tool names (not aliases) that may run in parallel with other calls.
    pub shared_tools: Vec<String>,
    /// Also treat tools annotated `readOnlyHint: true` as shareable. Off by default: the
    /// annotation is the server's own claim.
    pub trust_read_only_hint: bool,
    /// Limit for one `tools/call`. On expiry the call is cancelled and, if the server does not
    /// stop within `cancel_grace`, the server is killed.
    pub call_timeout: Duration,
    /// How long a cancelled call may take to stop before the server is killed.
    pub cancel_grace: Duration,
    /// Limit for spawning the server and listing its tools.
    pub startup_timeout: Duration,
    /// How long `close()` waits for the server to exit after its stdin is closed.
    pub shutdown_timeout: Duration,
}

impl McpServerConfig {
    pub fn new(id: impl Into<String>, command: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            inherit_env: Vec::new(),
            shared_tools: Vec::new(),
            trust_read_only_hint: false,
            call_timeout: Duration::from_secs(60),
            cancel_grace: Duration::from_secs(2),
            startup_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(5),
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}
