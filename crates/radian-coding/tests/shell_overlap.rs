//! ADR 0043 decision 4: the heavy-command gate is gone, so `shell` commands run without
//! queueing.
//!
//! Three commands that *look* heavy to the deleted matcher (`cargo build`/`cargo test`/`cargo
//! clippy`) must all run at the same time. `cargo` is a **stub** on `PATH` (a real `cargo test`
//! is not wanted here), and the three stubs cross a barrier made of marker files: each writes its
//! own marker and then waits for both others', so only real three-way concurrency lets any of
//! them finish. No test-side sleep and no polling; the stub's wait is bounded, so a serialized
//! run fails instead of hanging.
//!
//! Three, not two, because the deleted gate's default was `DEFAULT_HEAVY_SLOTS = 2`: two
//! commands overlapped even under the old gate, so a two-command test would pass on the old code
//! and prove nothing. With three, the old code starts two and queues the third, the two wait for
//! the third's marker, and the test fails exactly there.

mod common;

use std::path::Path;

use common::TempDir;
use radian_coding::CodingTools;
use radian_core::{NoopSink, ToolCall, ToolContext, ToolOutput, ToolProvider, ToolSession};
use radian_sandbox::{Sandbox, SandboxMode, SandboxPolicy};
use serde_json::json;

/// The stub's bounded barrier wait. A serialized run reaches the bound and exits 3.
const BARRIER_ITERATIONS: u32 = 400_000;

/// A `cargo` stub: `cargo test <my-marker> <other-marker> <other-marker>`. It writes its marker,
/// then waits (bounded, no sleep) for the two other stubs' markers, then prints the proof line.
fn stub() -> String {
    format!(
        "#!/bin/sh\n\
         # Drop the `test` subcommand, keeping the three marker paths.\n\
         shift\n\
         mine=$1\n\
         other1=$2\n\
         other2=$3\n\
         : > \"$mine\"\n\
         i=0\n\
         while [ ! -e \"$other1\" ] || [ ! -e \"$other2\" ]; do\n\
         \x20 i=$((i+1))\n\
         \x20 if [ \"$i\" -ge {BARRIER_ITERATIONS} ]; then\n\
         \x20   echo \"barrier timed out waiting for $other1 and $other2\" >&2\n\
         \x20   exit 3\n\
         \x20 fi\n\
         done\n\
         echo \"all three ran\"\n"
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
        cancellation: radian_core::CancellationToken::new(),
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
            radian_core::ContentPart::Text { text, .. } => Some(text.as_str()),
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
async fn three_heavy_looking_shell_commands_run_at_the_same_time() {
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
    let third = ws.join("third.started");
    // Every command is heavy-looking: the deleted gate matched `cargo test` as a prefix, and its
    // default was two slots.
    let a = call(
        &tools,
        format!(
            "cargo test {} {} {}",
            first.display(),
            second.display(),
            third.display()
        ),
    );
    let b = call(
        &tools,
        format!(
            "cargo test {} {} {}",
            second.display(),
            third.display(),
            first.display()
        ),
    );
    let c = call(
        &tools,
        format!(
            "cargo test {} {} {}",
            third.display(),
            first.display(),
            second.display()
        ),
    );
    let (a, b, c) = tokio::join!(a, b, c);

    let a = a.unwrap_or_else(|e| panic!("the first command failed: {e}"));
    let b = b.unwrap_or_else(|e| panic!("the second command failed: {e}"));
    let c = c.unwrap_or_else(|e| panic!("the third command failed: {e}"));
    for (name, output) in [("first", &a), ("second", &b), ("third", &c)] {
        let text = text_of(output);
        assert!(
            text.contains("all three ran"),
            "the {name} command must finish only after both other markers exist: {text}"
        );
        assert!(
            text.contains("exit code: 0"),
            "the {name} command exited nonzero: {text}"
        );
    }
    assert!(
        first.exists() && second.exists() && third.exists(),
        "all three stubs started"
    );
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
