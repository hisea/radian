//! The seccomp fd-hygiene test (Linux only), in its own binary: it counts the process's
//! open fds, so it must not share a process with other spawning tests. With exactly one
//! test in this binary there is no intra-process concurrency and the count is exact.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use radian_sandbox::{Sandbox, SandboxPolicy};

/// Building and spawning commands does not accumulate fds in the parent: each command
/// owns a freshly written pipe (closed on drop), so a loop cannot leak. A shared pipe
/// would break the second spawn with `EINVAL` (the escape matrix would catch it); a
/// per-command leak would grow the count by the loop length and fail this.
#[test]
fn commands_do_not_leak_filter_fds() {
    if radian_sandbox::confined_by_outer_sandbox() {
        println!("radian-test: skipped inside a sandbox: commands_do_not_leak_filter_fds");
        return;
    }
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let ws = std::env::temp_dir().join(format!(
        "radian-seccomp-fd-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&ws).unwrap();
    let ws: PathBuf = std::fs::canonicalize(&ws).unwrap();
    let sandbox = Sandbox::from_policy(SandboxPolicy::workspace_write(&ws).unwrap()).unwrap();
    fn open_fds() -> usize {
        std::fs::read_dir("/proc/self/fd")
            .expect("fd listing")
            .count()
    }
    let before = open_fds();
    for _ in 0..20 {
        let _ = sandbox
            .command("/bin/true", std::iter::empty::<&str>())
            .unwrap();
    }
    for _ in 0..5 {
        let out = sandbox
            .command("/bin/true", std::iter::empty::<&str>())
            .unwrap()
            .output()
            .expect("true runs");
        assert!(out.status.success());
    }
    assert_eq!(
        open_fds(),
        before,
        "building and spawning commands must not leak fds"
    );
    let _ = std::fs::remove_dir_all(&ws);
}
