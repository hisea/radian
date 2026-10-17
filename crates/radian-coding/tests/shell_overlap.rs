//! ADR 0043 decision 4: the heavy-command gate is gone, so `shell` commands run without
//! queueing.
//!
//! Two commands that *look* heavy to the deleted matcher (`cargo build`/`cargo test`/`cargo
//! clippy`) must overlap. `cargo` is a **stub** on `PATH` (a real `cargo test` is not wanted
//! here), and the two stubs cross a barrier made of marker files: each writes its own marker and
//! then waits for the other's, so only real concurrency lets both finish. No test-side sleep and
//! no polling; the stub's wait is bounded, so a serialized run fails instead of hanging.
//!
//! The mutation this catches is exactly the deleted gate: put a one-slot gate back in
//! `shell::run` and the first command times out on the barrier while the second is not started.

mod common;

use std::path::Path;

use agent_core::{NoopSink, ToolCall, ToolContext, ToolOutput, ToolProvider, ToolSession};
use common::TempDir;
use radian_coding::CodingTools;
use radian_sandbox::{Sandbox, SandboxMode, SandboxPolicy};
use serde_json::json;

/// The stub's bounded barrier wait. A serialized run reaches the bound and exits 3.
const BARRIER_ITERATIONS: u32 = 400_000;

/// A `cargo` stub: `cargo test <my-marker> <other-marker>`. It writes its marker, then waits
/// (bounded, no sleep) for the other stub's marker, then prints the proof line.
fn stub() -> String {
    format!(
        "#!/bin/sh\n\
         # Drop the `test` subcommand, keeping the two marker paths.\n\
         shift\n\
         mine=$1\n\
         other=$2\n\
         : > \"$mine\"\n\
         i=0\n\
         while [ ! -e \"$other\" ]; do\n\
         \x20 i=$((i+1))\n\
         \x20 if [ \"$i\" -ge {BARRIER_ITERATIONS} ]; then\n\
         \x20   echo \"barrier timed out waiting for $other\" >&2\n\
         \x20   exit 3\n\
         \x20 fi\n\
         done\n\
         echo \"both ran\"\n"
    )
}

/// `FullAccess` needs no backend, so this runs everywhere.
fn transparent_tools(ws: &Path) -> CodingTools {
    let policy = SandboxPolicy::new(SandboxMode::FullAccess, ws).expect("full-access policy");
    let sandbox = Sandbox::from_policy(policy).expect("full access needs no backend");
    CodingTools::with_sandbox(ws, sandbox).expect("matching workspaces")
}

async fn call(tools: &CodingTools, command: String) -> Result<ToolOutput, String> {
    let session = tools.open_session().await.expect("open session");
    let call = ToolCall {
        id: "t".into(),
        name: "shell".into(),
        arguments: json!({"command": command}),
    };
    let sink = NoopSink;
    let cx = ToolContext {
        cancellation: agent_core::CancellationToken::new(),
        events: &sink,
        index: 0,
    };
    session
        .execute(&call, cx)
        .await
        .map_err(|failure| format!("{}: {}", failure.code, failure.message))
}

fn text_of(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|part| match part {
            agent_core::ContentPart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Puts the stub's directory first on `PATH` for the whole process, restoring it on drop. This
/// is a single-test binary, so nothing else reads `PATH` while it is changed.
struct PathFront {
    previous: Option<std::ffi::OsString>,
}

impl PathFront {
    fn install(dir: &Path) -> Self {
        let previous = std::env::var_os("PATH");
        let mut paths = vec![dir.to_path_buf()];
        paths.extend(std::env::split_paths(
            previous.as_deref().unwrap_or_default(),
        ));
        let joined = std::env::join_paths(paths).expect("a valid PATH");
        // SAFETY: this binary has one test; the runtime's worker threads do no environment I/O
        // of their own, and the value is restored before the test returns.
        unsafe { std::env::set_var("PATH", &joined) };
        Self { previous }
    }
}

impl Drop for PathFront {
    fn drop(&mut self) {
        // SAFETY: same argument as `install`.
        unsafe {
            match &self.previous {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

#[tokio::test]
async fn two_heavy_looking_shell_commands_run_at_the_same_time() {
    let dir = TempDir::new("shell-overlap");
    let ws = dir.path().to_path_buf();
    let bin = ws.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let cargo = bin.join("cargo");
    std::fs::write(&cargo, stub()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let _path = PathFront::install(&bin);

    let tools = transparent_tools(&ws);
    let first = ws.join("first.started");
    let second = ws.join("second.started");
    // Both commands are heavy-looking: the deleted gate matched `cargo test` as a prefix.
    let a = call(
        &tools,
        format!("cargo test {} {}", first.display(), second.display()),
    );
    let b = call(
        &tools,
        format!("cargo test {} {}", second.display(), first.display()),
    );
    let (a, b) = tokio::join!(a, b);

    let a = a.unwrap_or_else(|e| panic!("the first command failed: {e}"));
    let b = b.unwrap_or_else(|e| panic!("the second command failed: {e}"));
    for (name, output) in [("first", &a), ("second", &b)] {
        let text = text_of(output);
        assert!(
            text.contains("both ran"),
            "the {name} command must finish only after the other's marker exists: {text}"
        );
        assert!(
            text.contains("exit code: 0"),
            "the {name} command exited nonzero: {text}"
        );
    }
    assert!(first.exists() && second.exists(), "both stubs started");
    // Nothing was queued: the shell result carries no queue marker (the field the deleted gate
    // wrote is gone).
    assert!(
        a.structured.is_none()
            || a.structured
                .as_ref()
                .unwrap()
                .get("heavy_queued_ms")
                .is_none()
    );
}
