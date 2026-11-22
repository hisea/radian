//! Unit tests for the command hooks, one module per area.

mod async_hooks;
mod layers;
mod observe;
mod permission_request;
mod stop_gate;
mod tool_events;
mod validator;

use super::*;
use radian_core::{CancelReason, CancellationToken};
use std::sync::Mutex;

use crate::wake_gate::{MAX_WAKES_PER_WINDOW, WAKE_WINDOW};

static N: AtomicU32 = AtomicU32::new(0);

/// A unique temp root, removed on drop. `ws` is the canonicalized workspace.
struct TempRoot {
    dir: PathBuf,
    ws: PathBuf,
}

impl TempRoot {
    fn new(label: &str) -> Self {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("radian-hooks-{}-{label}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ws = dir.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let ws = std::fs::canonicalize(&ws).unwrap();
        Self { dir, ws }
    }

    fn outside(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Fixture {
    _root: TempRoot,
    hooks: CommandHooks,
    warnings: Arc<Mutex<Vec<String>>>,
    wake_notices: Arc<Mutex<Vec<String>>>,
    intent_notices: Arc<Mutex<Vec<String>>>,
    hook_stop_notices: Arc<Mutex<Vec<String>>>,
    inbox: Arc<Inbox>,
}

fn warnings_collector() -> (HooksWarning, Arc<Mutex<Vec<String>>>) {
    let warnings = Arc::new(Mutex::new(Vec::new()));
    let warn: HooksWarning = {
        let warnings = Arc::clone(&warnings);
        Arc::new(move |message: &str| {
            warnings.lock().unwrap().push(message.to_owned());
        })
    };
    (warn, warnings)
}

fn hook(command: &str) -> HookCommand {
    HookCommand {
        command: command.to_owned(),
        args: None,
        timeout_secs: None,
        async_hook: false,
        async_rewake: false,
    }
}

fn entry(matcher: &str, commands: Vec<HookCommand>) -> HookMatcher {
    HookMatcher {
        matcher: Some(matcher.to_owned()),
        hooks: commands,
    }
}

/// An executable `/bin/sh` script for the exec-form tests. The shebang is what makes the
/// kernel run it directly, with exactly the arguments it was given.
#[cfg(unix)]
fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A `PreToolUse` config with one hook for the `shell` tool.
fn pre_hook(hook: HookCommand) -> HooksConfig {
    HooksConfig {
        pre_tool_use: vec![entry("shell", vec![hook])],
        ..HooksConfig::default()
    }
}

fn fixture_with(config: &HooksConfig, sandbox: SandboxSlot) -> Fixture {
    layered_with_root(
        TempRoot::new("case"),
        config,
        &HooksConfig::default(),
        sandbox,
    )
}

fn layered(user: &HooksConfig, project: &HooksConfig) -> Fixture {
    layered_with_root(
        TempRoot::new("layered"),
        user,
        project,
        SandboxSlot::empty(),
    )
}

fn layered_with_root(
    root: TempRoot,
    user: &HooksConfig,
    project: &HooksConfig,
    sandbox: SandboxSlot,
) -> Fixture {
    let (warn, warnings) = warnings_collector();
    let (wake, wake_notices) = warnings_collector();
    let (intent, intent_notices) = warnings_collector();
    let (hook_stop, hook_stop_notices) = warnings_collector();
    let inbox = Inbox::new(64);
    let hooks = CommandHooks::new(
        user,
        project,
        &root.ws,
        "test-session",
        sandbox,
        &inbox,
        &tokio::runtime::Handle::current(),
        warn,
        wake,
        intent,
        hook_stop,
    );
    Fixture {
        _root: root,
        hooks,
        warnings,
        wake_notices,
        intent_notices,
        hook_stop_notices,
        inbox,
    }
}

fn plain(config: &HooksConfig) -> Fixture {
    fixture_with(config, SandboxSlot::empty())
}

fn pre_config(matcher: &str, command: &str) -> HooksConfig {
    HooksConfig {
        pre_tool_use: vec![entry(matcher, vec![hook(command)])],
        ..HooksConfig::default()
    }
}

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: format!("call-{name}"),
        name: name.to_owned(),
        arguments,
    }
}

fn cx() -> HookContext {
    HookContext {
        cancellation: CancellationToken::new(),
        model_calls: 0,
    }
}

/// A completed turn whose last assistant message is `text` (or none).
fn completed_outcome(status: RunStatus, text: Option<&str>) -> RunOutcome {
    RunOutcome {
        status,
        final_message: text.map(Message::assistant),
        unapplied_steering: Vec::new(),
        model_calls: 0,
    }
}

/// A completed turn with no last assistant message: the validator stays off, so
/// hook-only tests keep their old shape.
fn completed_quiet() -> RunOutcome {
    completed_outcome(RunStatus::Completed, None)
}

/// A completed turn ending with `text`.
fn completed_text(text: &str) -> RunOutcome {
    completed_outcome(RunStatus::Completed, Some(text))
}

/// A completed turn that made `model_calls` model requests: `1` means the model only
/// spoke (no tool round trip), and anything above means at least one tool call.
fn completed_calls(text: Option<&str>, model_calls: u32) -> RunOutcome {
    let mut outcome = completed_outcome(RunStatus::Completed, text);
    outcome.model_calls = model_calls;
    outcome
}

/// One hook continuation cycle at a turn end and the turn it opened, claimed so the
/// next `run_stop_once` sees it as a hook-opened turn. Returns whether a turn opened.
async fn hook_round(fixture: &Fixture, outcome: &RunOutcome) -> bool {
    fixture.hooks.run_stop_once(outcome).await;
    fixture.inbox.claim_next_turn().await.is_some()
}

fn warned(warnings: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    warnings.lock().unwrap().clone()
}

/// Waits for one observe-only hook's stdin file to hold a complete JSON object. A
/// `Notification` run is not waited for by `wait_observe` (by design), so its file is
/// the only completion signal.
async fn wait_notification(path: &Path) -> String {
    for _ in 0..600 {
        if let Ok(text) = std::fs::read_to_string(path)
            && serde_json::from_str::<serde_json::Value>(&text).is_ok()
        {
            return text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{} never held valid JSON", path.display());
}

fn stop_config(command: &str) -> HooksConfig {
    HooksConfig {
        stop: vec![HookMatcher {
            matcher: None,
            hooks: vec![hook(command)],
        }],
        ..HooksConfig::default()
    }
}
