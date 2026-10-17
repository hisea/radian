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
//!   "cost_usd": 14.51,
//!   "tasks": { "running": 1, "total": 3 }
//! }
//! ```
//!
//! Every field is `null` when the fact is unknown (never a fabricated number). Only facts the
//! footer already displays are sent: no transcript, no tool arguments, no secrets, no headers.

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

/// The variables a command item inherits, on top of `PATH` (the MCP child baseline). There is no
/// shared MCP allow-list constant to reuse — an MCP server's inherited names come from its own
/// `--mcp-config` entry (`McpServerConfig::inherit_env`) — so this is the footer command's own
/// deliberate allow-list. `RADIAN_SESSION_ID` and `RADIAN_CWD` are the only Radian facts a
/// command gets. Everything else is cleared; the host environment never leaks in.
pub const INHERITED_ENV: &[&str] = &["RADIAN_SESSION_ID", "RADIAN_CWD"];

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
    pub tasks_running: Option<u64>,
    pub tasks_total: Option<u64>,
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
            "tasks": {
                "running": self.tasks_running,
                "total": self.tasks_total,
            },
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
/// revision 14 decision 4). Every `command` item from any current configuration source is
/// accepted: v1 has no project-level config layer (ADR 0031). When one is added, a `command`
/// item must be refused there, and that slice owns the test. The caller spawns one
/// [`CommandContributor`] per spec.
pub fn command_specs(footer: &TuiFooterConfig) -> (Vec<CommandSpec>, Vec<String>) {
    let mut specs = Vec::new();
    let mut warnings = Vec::new();
    for (id, item) in &footer.items {
        let Some(command) = param(item, "command") else {
            continue;
        };
        // A disabled item is hidden everywhere, so its command is never run.
        if item.disabled {
            continue;
        }
        let (program, args) = match parse_command(command) {
            Ok(parsed) => parsed,
            Err(error) => {
                warnings.push(format!("tui.footer.items.{id}.command: {error}"));
                continue;
            }
        };
        let period = match param(item, "period") {
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
        let mut child = command
            .spawn()
            .map_err(|_| "the footer command could not be started".to_owned())?;
        let run = async move {
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(stdin_json.as_bytes())
                    .await
                    .map_err(|_| "the footer command's stdin failed".to_owned())?;
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
            }])
        })
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
}
