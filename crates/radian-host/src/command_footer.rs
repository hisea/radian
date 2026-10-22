//! The out-of-process footer `command` component (ADR 0027).
//!
//! `[tui.footer.items.<id>] command = "<executable or command line>"` runs a user command as the
//! user, unsandboxed, on a background period. Its stdout's **first line** becomes the item's
//! value; a line that is JSON `{"text","style","symbol"}` also sets the text's theme role and an
//! optional symbol. Its stdin is one JSON object of the footer's live, display-safe facts.
//!
//! The value travels **only** through the shared [`ContributionStore`]: a command item is an
//! ordinary [`StatusContributor`], so the renderer needs no special case and the draw path does no
//! I/O. Nothing else leaves the child — the value never enters the journal, the session
//! transcript, the model context or a herdr report, and a failure notice is generic, never the
//! command's output.
//!
//! A JSON `symbol` is drawn **verbatim** before the text (like a built-in glyph), so include your
//! own trailing space; a configured `symbol` on the item wins over it, and a configured
//! `symbol_style` styles whichever symbol is drawn.
//!
//! v1 has **one** configuration layer, chosen by the user (`--config` > `RADIAN_CONFIG` >
//! `~/.radian/config/config.toml`; there is no project-level config, ADR 0031), so every source
//! this reads is the user's own and every `command` item is accepted. When a project-level
//! config layer is added, a `command` item must be refused there and that slice owns the test.
//!
//! # stdin JSON schema
//!
//! Field names are stable and documented:
//!
//! ```json
//! {
//!   "model": "claude-new",
//!   "cwd": "/work/project",
//!   "branch": "main",
//!   "context_percent": 41.3,
//!   "tokens": { "input": 15000000, "output": 2600000, "cache_read": 1962000000 },
//!   "cost_usd": 14.51
//! }
//! ```
//!
//! Every field is `null` when the fact is unknown (never a fabricated number). Only facts the
//! footer already displays are sent: no transcript, no tool arguments, no secrets, no headers.
//! (ADR 0043 removed the task registry, so the earlier `tasks` key is gone.)

use std::ffi::OsString;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::config::TuiFooterConfig;
use crate::contrib::{Contribution, Segment, SegmentRole, StatusContributor};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long a command item may take for its whole run (spawn, write stdin, read stdout and exit)
/// before it is killed and its last value kept and marked stale (ADR 0027).
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// The hard maximum size of the displayed line, in bytes.
pub const MAX_OUTPUT_BYTES: usize = 256;

/// The shortest configurable period, and the longest (ADR 0027).
pub const MIN_PERIOD_SECS: u64 = 15;
pub const MAX_PERIOD_SECS: u64 = 3600;

/// The default period when `period` is not written.
pub const DEFAULT_PERIOD_SECS: u64 = 60;

/// A hard cap on how much of a command's stdout is read, so a flooding command cannot grow the
/// process's memory without bound. Only the first line (at most [`MAX_OUTPUT_BYTES`]) is kept.
const STDOUT_READ_CAP: u64 = 64 * 1024;

/// How long a spawn that fails with `ETXTBSY` ("Text file busy", [`std::io::ErrorKind::ExecutableFileBusy`])
/// is retried: 20 attempts 10 ms apart. On Linux, executing a script that any process still
/// holds open for writing fails, and in a multi-threaded runner another thread's `fork` can
/// inherit the script's write fd for the instant between its create and close. The holder
/// always closes within milliseconds (a fixture write, an editor save), so the bound is never
/// reached in practice; it only stops a permanently-held file from stalling the tick past
/// [`COMMAND_TIMEOUT`].
const SPAWN_BUSY_RETRIES: u32 = 20;
const SPAWN_BUSY_PAUSE: Duration = Duration::from_millis(10);

/// The variables a command item inherits, on top of `PATH` (the MCP child baseline). There is no
/// shared MCP allow-list constant to reuse — an MCP server's inherited names come from its own
/// `--mcp-config` entry (`McpServerConfig::inherit_env`) — so this is the footer command's own
/// deliberate allow-list. `RADIAN_SESSION_ID` and `RADIAN_CWD` are the only Radian facts a
/// command gets. Everything else is cleared; the host environment never leaks in.
pub const INHERITED_ENV: &[&str] = &["RADIAN_SESSION_ID", "RADIAN_CWD"];

/// The item parameters a footer `command` item reads (`crates/radian-tui/tests/docs_sync.rs`
/// asserts the manual names each one): `command` is the command line, `period` the refresh
/// period in seconds.
pub const COMMAND_ITEM_PARAMS: &[&str] = &["command", "period"];

/// Every built-in footer item id (ADR 0027): the ids the TUI's component registry claims.
/// A `command` under one of these never spawns: the built-in always wins the draw, so the
/// process would be invisible. `command_specs` skips such an entry with one warning.
///
/// The single source for the registry itself is `radian-tui`'s component list (which cannot
/// be shared here without a dependency cycle); a drift test there pins the two equal, so a
/// new built-in fails there until it is added here too.
pub const BUILTIN_FOOTER_ITEMS: &[&str] = &[
    "brand",
    "host",
    "model",
    "reasoning",
    "tier",
    "cwd",
    "git_branch",
    "worktree",
    "status",
    "session",
    "context",
    "usage",
    "tokens",
    "speed",
    "elapsed",
    "cost",
    "pending",
    "queued",
    "loop",
    "jobs",
    "mcp",
    "scroll",
];

/// Whether `id` is a built-in footer item, whose draw the built-in component always wins.
pub fn is_builtin_footer_item(id: &str) -> bool {
    BUILTIN_FOOTER_ITEMS.contains(&id)
}

/// The display-safe facts a command item receives on stdin (ADR 0027).
/// `None` means "unknown": a missing fact is `null`, never a zero that looks measured.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FooterSnapshot {
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    /// The used fraction of the context window, as a percentage (0–100).
    pub context_percent: Option<f64>,
    pub tokens_input: Option<u64>,
    pub tokens_output: Option<u64>,
    pub tokens_cache_read: Option<u64>,
    /// The accumulated fee in US dollars, or `None` when no priced call has arrived.
    pub cost_usd: Option<f64>,
}

impl FooterSnapshot {
    /// The one JSON document written to a command's stdin. Field order is fixed so the document
    /// is stable for a reader; unknown facts are `null`.
    pub fn to_json(&self) -> String {
        let value = serde_json::json!({
            "model": self.model,
            "cwd": self.cwd,
            "branch": self.branch,
            "context_percent": self.context_percent,
            "tokens": {
                "input": self.tokens_input,
                "output": self.tokens_output,
                "cache_read": self.tokens_cache_read,
            },
            "cost_usd": self.cost_usd,
        });
        value.to_string()
    }
}

/// The live handoff between the TUI (writer) and the command contributors (readers). The TUI
/// publishes a fresh [`FooterSnapshot`] as the footer's facts change; a contributor reads the
/// latest when it runs. A plain lock, never the terminal or the draw path, so the UI never waits
/// for a command.
#[derive(Debug, Default)]
pub struct FooterContext {
    snapshot: RwLock<FooterSnapshot>,
}

impl FooterContext {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Publishes the latest facts.
    pub fn set(&self, snapshot: FooterSnapshot) {
        *self.snapshot.write().unwrap_or_else(|e| e.into_inner()) = snapshot;
    }

    /// The latest facts, cloned for one command run.
    pub fn snapshot(&self) -> FooterSnapshot {
        self.snapshot
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// One enabled command footer item (ADR 0027): the parsed argv and the
/// refresh period. `period == None` means "run once, at startup".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub id: String,
    pub program: OsString,
    pub args: Vec<OsString>,
    pub period: Option<Duration>,
}

/// The one-line output of a command item, parsed from the first stdout line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandOutput {
    text: String,
    role: SegmentRole,
    symbol: Option<String>,
}

/// The command items a footer config enables, plus a warning per malformed one (ADR 0027
/// ). Every `command` item from any current configuration source is
/// accepted: v1 has no project-level config layer (ADR 0031). When one is added, a `command`
/// item must be refused there, and that slice owns the test. The caller spawns one
/// [`CommandContributor`] per spec.
pub fn command_specs(footer: &TuiFooterConfig) -> (Vec<CommandSpec>, Vec<String>) {
    let mut specs = Vec::new();
    let mut warnings = Vec::new();
    for (id, item) in &footer.items {
        let Some(command) = param(item, COMMAND_ITEM_PARAMS[0]) else {
            continue;
        };
        // A disabled item is hidden everywhere, so its command is never run.
        if item.disabled {
            continue;
        }
        // A built-in always wins the draw, so a `command` under its id would run unseen:
        // spawn nothing and warn once.
        if is_builtin_footer_item(id) {
            warnings.push(format!(
                "tui.footer.items.{id}.command: {id:?} is a built-in item; its `command` is ignored"
            ));
            continue;
        }
        let (program, args) = match parse_command(command) {
            Ok(parsed) => parsed,
            Err(error) => {
                warnings.push(format!("tui.footer.items.{id}.command: {error}"));
                continue;
            }
        };
        let period = match param(item, COMMAND_ITEM_PARAMS[1]) {
            None => Some(Duration::from_secs(DEFAULT_PERIOD_SECS)),
            Some(text) => match text.parse::<u64>() {
                Ok(secs) => period_from_secs(secs),
                Err(_) => {
                    warnings.push(format!(
                        "tui.footer.items.{id}.period = {text:?} is not a number of seconds; \
                         using the default"
                    ));
                    Some(Duration::from_secs(DEFAULT_PERIOD_SECS))
                }
            },
        };
        specs.push(CommandSpec {
            id: id.clone(),
            program,
            args,
            period,
        });
    }
    (specs, warnings)
}

/// One item's own parameter, as text (the config reader keeps unknown item options in `params`).
fn param<'a>(item: &'a crate::config::TuiFooterItemConfig, key: &str) -> Option<&'a str> {
    item.params
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// The period a configured number of seconds means (ADR 0027): `0` means
/// "startup only" (`None`); every other value is clamped into 15–3600 seconds.
fn period_from_secs(secs: u64) -> Option<Duration> {
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(
            secs.clamp(MIN_PERIOD_SECS, MAX_PERIOD_SECS),
        ))
    }
}

/// Splits a configured command line into argv (ADR 0027). **No shell runs**:
/// whitespace separates words, single and double quotes group them, and a backslash escapes the
/// next character (inside double quotes too). An empty or unterminated command is refused, so a
/// malformed line never reaches `exec`.
pub fn parse_command(line: &str) -> Result<(OsString, Vec<OsString>), String> {
    let mut argv: Vec<String> = Vec::new();
    let mut token = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else if ch == '\\' && q == '"' {
                    match chars.next() {
                        Some(escaped) => token.push(escaped),
                        None => return Err("a trailing backslash escapes nothing".to_owned()),
                    }
                } else {
                    token.push(ch);
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    started = true;
                }
                '\\' => match chars.next() {
                    Some(escaped) => {
                        token.push(escaped);
                        started = true;
                    }
                    None => return Err("a trailing backslash escapes nothing".to_owned()),
                },
                ch if ch.is_whitespace() => {
                    if started {
                        argv.push(std::mem::take(&mut token));
                        started = false;
                    }
                }
                ch => {
                    token.push(ch);
                    started = true;
                }
            },
        }
    }
    if quote.is_some() {
        return Err("an unterminated quote".to_owned());
    }
    if started {
        argv.push(token);
    }
    let Some(program) = argv.first() else {
        return Err("the command is empty".to_owned());
    };
    if program.is_empty() {
        return Err("the command's executable name is empty".to_owned());
    }
    Ok((
        OsString::from(program.clone()),
        argv[1..].iter().map(OsString::from).collect(),
    ))
}

/// One command item's background producer (ADR 0027). It is an ordinary
/// [`StatusContributor`]: the runner writes its value into the shared store or, on a failure,
/// keeps the last value and marks it stale.
pub struct CommandContributor {
    id: String,
    program: OsString,
    args: Vec<OsString>,
    period: Option<Duration>,
    context: Arc<FooterContext>,
    session_id: String,
    /// The workspace the session runs in, exported as `RADIAN_CWD`. `None` when unknown.
    radian_cwd: Option<String>,
}

impl CommandContributor {
    pub fn new(
        spec: CommandSpec,
        context: Arc<FooterContext>,
        session_id: impl Into<String>,
        radian_cwd: Option<String>,
    ) -> Self {
        Self {
            id: spec.id,
            program: spec.program,
            args: spec.args,
            period: spec.period,
            context,
            session_id: session_id.into(),
            radian_cwd,
        }
    }

    /// Runs the command once and returns its parsed first line. Every failure is a generic,
    /// redacted message: the command's output, stderr and stdin never travel in it.
    ///
    /// Two Linux races are handled, not reported: a spawn that fails with `ETXTBSY` is retried
    /// (another thread's `fork` inherited the script's write fd for the instant between its
    /// create and close), and an `EPIPE` on the stdin write is ignored (a command that exits
    /// without reading stdin closed the pipe first; its output is still collected below). Both
    /// hit a real user too: a fast command that ignores stdin, or a save of the script at the
    /// tick moment, must not mark the value stale.
    async fn run(&self, stdin_json: String) -> Result<CommandOutput, String> {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&self.args)
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        command.env("RADIAN_SESSION_ID", &self.session_id);
        if let Some(cwd) = &self.radian_cwd {
            command.env("RADIAN_CWD", cwd);
        }
        let mut child = spawn_retrying_txtbsy(&mut command).await?;
        let run = async move {
            if let Some(mut stdin) = child.stdin.take() {
                write_stdin(&mut stdin, stdin_json.as_bytes()).await?;
                // Closing stdin signals end-of-input; the child may read it before writing.
                drop(stdin);
            }
            let mut stdout = child
                .stdout
                .take()
                .ok_or_else(|| "the footer command's stdout was not piped".to_owned())?;
            let mut buffer = Vec::new();
            let mut limited = (&mut stdout).take(STDOUT_READ_CAP);
            let read = limited.read_to_end(&mut buffer);
            let (read_result, status) = tokio::join!(read, child.wait());
            read_result.map_err(|_| "the footer command's stdout failed".to_owned())?;
            let status = status.map_err(|_| "the footer command's wait failed".to_owned())?;
            Ok::<_, String>((buffer, status))
        };
        let (buffer, status) = match tokio::time::timeout(COMMAND_TIMEOUT, run).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(error)) => return Err(error),
            // A timeout: the future is dropped and `kill_on_drop` kills the child.
            Err(_) => return Err("the footer command timed out".to_owned()),
        };
        if !status.success() {
            return Err("the footer command exited non-zero".to_owned());
        }
        let line = first_output_line(&buffer);
        if line.is_empty() {
            return Err("the footer command produced no output".to_owned());
        }
        parse_output(&line)
    }
}

impl StatusContributor for CommandContributor {
    fn id(&self) -> &'static str {
        "footer command"
    }

    fn items(&self) -> Vec<String> {
        vec![self.id.clone()]
    }

    fn period(&self) -> Duration {
        self.period
            .unwrap_or_else(|| Duration::from_secs(DEFAULT_PERIOD_SECS))
    }

    fn refresh(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Vec<Contribution>, String>> + Send + '_>,
    > {
        Box::pin(async move {
            let stdin_json = self.context.snapshot().to_json();
            let output = self.run(stdin_json).await?;
            Ok(vec![Contribution {
                item: self.id.clone(),
                segments: vec![Segment::new(output.text, output.role)],
                stale: false,
                symbol: output.symbol,
                usage: None,
            }])
        })
    }
}

/// Spawns the command, retrying an `ETXTBSY` spawn (see [`SPAWN_BUSY_RETRIES`]). Every other
/// spawn error is the generic redacted failure.
async fn spawn_retrying_txtbsy(
    command: &mut tokio::process::Command,
) -> Result<tokio::process::Child, String> {
    let mut attempts = 0;
    loop {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                    && attempts < SPAWN_BUSY_RETRIES =>
            {
                attempts += 1;
                tokio::time::sleep(SPAWN_BUSY_PAUSE).await;
            }
            Err(_) => return Err("the footer command could not be started".to_owned()),
        }
    }
}

/// Writes the stdin document, tolerating a child that already exited: a command that never
/// reads stdin (the common `printf` fixture) closes the pipe on exit and the write then fails
/// with `EPIPE`, but the child's output is still in flight and is collected by the caller.
/// Any other write error is the generic redacted failure.
async fn write_stdin(
    stdin: &mut (impl tokio::io::AsyncWrite + Unpin),
    stdin_json: &[u8],
) -> Result<(), String> {
    match stdin.write_all(stdin_json).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(_) => Err("the footer command's stdin failed".to_owned()),
    }
}

/// The first line of some command output, trimmed and truncated to [`MAX_OUTPUT_BYTES`] on a
/// UTF-8 boundary.
fn first_output_line(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(bytes.len());
    let line = &bytes[..end];
    let end = if line.len() > MAX_OUTPUT_BYTES {
        utf8_boundary(line, MAX_OUTPUT_BYTES)
    } else {
        line.len()
    };
    String::from_utf8_lossy(&line[..end])
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .to_owned()
}

/// The largest UTF-8 boundary at or below `max`.
fn utf8_boundary(bytes: &[u8], max: usize) -> usize {
    let mut i = max.min(bytes.len());
    while i > 0 && (bytes[i] & 0b1100_0000) == 0b1000_0000 {
        i -= 1;
    }
    i
}

/// Parses one output line: plain text, or a JSON `{"text","style","symbol"}` object. An invalid
/// JSON object or an unknown style is an error, so the last good value is kept and marked stale
/// (ADR 0027). A line that does not start with `{` is plain text.
fn parse_output(line: &str) -> Result<CommandOutput, String> {
    if !line.starts_with('{') {
        return Ok(CommandOutput {
            text: line.to_owned(),
            role: SegmentRole::Normal,
            symbol: None,
        });
    }
    let value: serde_json::Value = serde_json::from_str(line)
        .map_err(|_| "the footer command output was not valid JSON".to_owned())?;
    let object = value
        .as_object()
        .ok_or_else(|| "the footer command output was not a JSON object".to_owned())?;
    let text = object
        .get("text")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "the footer command JSON has no string `text`".to_owned())?;
    let role = match object.get("style") {
        None | Some(serde_json::Value::Null) => SegmentRole::Normal,
        // The style name is the command's output: the error never repeats it.
        Some(serde_json::Value::String(name)) => SegmentRole::parse(name)
            .ok_or_else(|| "the footer command style is not a known role".to_owned())?,
        Some(_) => return Err("the footer command style was not a string".to_owned()),
    };
    let symbol = match object.get("symbol") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(symbol)) => Some(symbol.clone()),
        Some(_) => return Err("the footer command symbol was not a string".to_owned()),
    };
    if text.is_empty() && symbol.is_none() {
        return Err("the footer command produced no output".to_owned());
    }
    Ok(CommandOutput {
        text: text.to_owned(),
        role,
        symbol,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_lines_split_without_a_shell() {
        let (program, args) = parse_command("printf hi").unwrap();
        assert_eq!(program, OsString::from("printf"));
        assert_eq!(args, vec![OsString::from("hi")]);

        let (program, args) =
            parse_command("  /bin/echo   'two words'  \"a \\\"b\\\"\"  ").unwrap();
        assert_eq!(program, OsString::from("/bin/echo"));
        assert_eq!(
            args,
            vec![OsString::from("two words"), OsString::from("a \"b\"")]
        );
    }

    #[test]
    fn empty_and_malformed_commands_are_refused() {
        assert!(parse_command("").is_err());
        assert!(parse_command("   ").is_err());
        assert!(parse_command("'unterminated").is_err());
        assert!(parse_command("trailing\\").is_err());
        assert!(parse_command("''").is_err(), "an empty program is refused");
    }

    #[test]
    fn the_period_is_clamped_and_zero_means_once() {
        assert_eq!(period_from_secs(0), None);
        assert_eq!(period_from_secs(1), Some(Duration::from_secs(15)));
        assert_eq!(period_from_secs(900), Some(Duration::from_secs(900)));
        assert_eq!(period_from_secs(9999), Some(Duration::from_secs(3600)));
    }

    #[test]
    fn the_first_line_only_is_taken_and_truncated_to_256_bytes() {
        assert_eq!(first_output_line(b"hi\nsecond\n"), "hi");
        assert_eq!(first_output_line(b"no newline"), "no newline");
        assert_eq!(first_output_line(b"  padded  \n"), "padded");
        let long = "é".repeat(200); // 400 bytes
        let line = first_output_line(long.as_bytes());
        assert!(line.len() <= MAX_OUTPUT_BYTES, "{}", line.len());
        assert_eq!(line.len(), 256);
    }

    #[test]
    fn plain_text_is_the_normal_role_and_json_sets_style_and_symbol() {
        let plain = parse_output("hello").unwrap();
        assert_eq!(plain.text, "hello");
        assert_eq!(plain.role, SegmentRole::Normal);
        assert_eq!(plain.symbol, None);

        let json = parse_output(r#"{"text":"hi","style":"warn","symbol":"!"}"#).unwrap();
        assert_eq!(json.text, "hi");
        assert_eq!(json.role, SegmentRole::Warn);
        assert_eq!(json.symbol.as_deref(), Some("!"));
    }

    #[test]
    fn invalid_json_and_unknown_styles_are_refused() {
        assert!(parse_output("{\"text\": }").is_err());
        assert!(parse_output(r#"{"style":"warn"}"#).is_err(), "no text");
        assert!(parse_output(r#"{"text":"x","style":"neon"}"#).is_err());
    }

    fn footer_with(id: &str, params: &[(&str, &str)], disabled: bool) -> TuiFooterConfig {
        let mut footer = TuiFooterConfig::default();
        footer.items.insert(
            id.to_owned(),
            crate::config::TuiFooterItemConfig {
                id: id.to_owned(),
                disabled,
                params: params
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
                ..Default::default()
            },
        );
        footer
    }

    #[test]
    fn a_disabled_command_is_not_a_spec_and_a_malformed_one_warns() {
        let footer = footer_with("x", &[("command", "printf hi")], true);
        let (specs, warnings) = command_specs(&footer);
        assert!(specs.is_empty(), "a disabled item's command never runs");
        assert!(warnings.is_empty(), "{warnings:?}");

        let footer = footer_with("x", &[("command", "printf 'unterminated")], false);
        let (specs, warnings) = command_specs(&footer);
        assert!(specs.is_empty());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("command"), "{warnings:?}");
    }

    /// A `command` under a built-in id spawns nothing: the built-in always wins the draw,
    /// so the process would be invisible. One warning per id names it.
    #[test]
    fn a_command_under_a_builtin_id_spawns_nothing_and_warns() {
        let mut footer = TuiFooterConfig::default();
        for id in ["model", "tokens"] {
            footer.items.insert(
                id.to_owned(),
                crate::config::TuiFooterItemConfig {
                    id: id.to_owned(),
                    params: vec![("command".to_owned(), "printf hi".to_owned())],
                    ..Default::default()
                },
            );
        }
        let (specs, warnings) = command_specs(&footer);
        assert!(
            specs.is_empty(),
            "a built-in `command` never runs: {specs:?}"
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        for id in ["model", "tokens"] {
            assert!(
                warnings.iter().any(
                    |w| w.contains(id) && w.contains("built-in item; its `command` is ignored")
                ),
                "the warning names {id:?}: {warnings:?}"
            );
        }
    }

    /// A custom id next to a built-in one still runs: the built-in skip drops only the
    /// built-in entry.
    #[test]
    fn a_custom_id_next_to_a_builtin_one_still_runs() {
        let mut footer = TuiFooterConfig::default();
        for id in ["model", "weather"] {
            footer.items.insert(
                id.to_owned(),
                crate::config::TuiFooterItemConfig {
                    id: id.to_owned(),
                    params: vec![("command".to_owned(), "printf hi".to_owned())],
                    ..Default::default()
                },
            );
        }
        let (specs, warnings) = command_specs(&footer);
        assert_eq!(
            specs
                .iter()
                .map(|spec| spec.id.as_str())
                .collect::<Vec<_>>(),
            vec!["weather"],
            "{specs:?}"
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("model"), "{warnings:?}");
    }

    #[cfg(target_os = "linux")]
    fn retry_scratch(label: &str) -> std::path::PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-command-footer-retry-{}-{label}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(target_os = "linux")]
    fn retry_script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[cfg(target_os = "linux")]
    fn retry_contributor(program: &std::path::Path, dir: &std::path::Path) -> CommandContributor {
        CommandContributor::new(
            CommandSpec {
                id: "thing".to_owned(),
                program: program.as_os_str().to_owned(),
                args: Vec::new(),
                period: Some(Duration::from_secs(60)),
            },
            FooterContext::new(),
            "sess-1",
            Some(dir.display().to_string()),
        )
    }

    /// An `ETXTBSY` spawn is retried until the writer closes (a Linux-only failure mode: macOS
    /// has no "text file busy" on exec). The script is held open for writing before `run`
    /// starts, so the first attempt always fails; a closer thread drops the hold after 30 ms
    /// while the runner retries for up to 200 ms. The sleeps only set the overlap's wide
    /// margins, never an ordering: the hold predates the first attempt, and the budget
    /// outlives any scheduling jitter of the close.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_txtbsy_spawn_is_retried_until_the_writer_closes() {
        let dir = retry_scratch("busy");
        let script = retry_script(&dir, "busy.sh", "printf hi");
        let held = std::fs::OpenOptions::new()
            .write(true)
            .open(&script)
            .unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            drop(held);
        });
        let output = retry_contributor(&script, &dir)
            .run("{}".to_owned())
            .await
            .expect("the retry outlives the writer's hold");
        assert_eq!(output.text, "hi");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An `EPIPE` on the stdin write is not a run failure: the read end is dropped first, then
    /// the send buffer is saturated so the kernel must report the closed pipe. No timing is
    /// involved: once the buffer is full the next write cannot succeed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_closed_stdin_pipe_is_not_a_run_failure() {
        let (mut writer, reader) = tokio::net::UnixStream::pair().unwrap();
        drop(reader);
        let chunk = vec![0u8; 65536];
        loop {
            match writer.write_all(&chunk).await {
                Ok(()) => {}
                Err(error) => {
                    assert_eq!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe,
                        "a write past a dropped reader is EPIPE"
                    );
                    break;
                }
            }
        }
        write_stdin(&mut writer, b"{}")
            .await
            .expect("EPIPE on stdin is tolerated");
    }
}
