//! Slice-0 integration tests (Linux only): the seccomp filter denies `socket(AF_UNIX)`
//! and `io_uring_setup` with `EPERM` in the `workspace` tier, a TCP socket and
//! `socketpair` keep working, and `full` is untouched. Each test carries its `FullAccess`
//! control, like the escape matrix.
#![cfg(target_os = "linux")]

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use radian_sandbox::{Sandbox, SandboxMode, SandboxPolicy};

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "radian-seccomp-test-{}-{label}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(std::fs::canonicalize(&dir).unwrap())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `workspace` sandbox plus its `FullAccess` control over a fresh workspace. Inside
/// another sandbox the resources (and the control) cannot be trusted, so the test prints
/// the fixed marker and returns, exactly like the escape matrix.
fn rig(label: &str) -> Option<(TempDir, Sandbox, Sandbox)> {
    if radian_sandbox::confined_by_outer_sandbox() {
        println!("radian-test: skipped inside a sandbox: {label}");
        return None;
    }
    let dir = TempDir::new(label);
    let sandbox =
        Sandbox::from_policy(SandboxPolicy::workspace_write(&dir.0).unwrap()).unwrap();
    let control = Sandbox::new(SandboxMode::FullAccess, &dir.0).unwrap();
    Some((dir, sandbox, control))
}

/// Runs `python3 -c <script>` under `sandbox`; returns (success, combined output).
fn python(sandbox: &Sandbox, script: &str) -> (bool, String) {
    let out = sandbox
        .command("python3", ["-c", script])
        .unwrap()
        .output()
        .expect("python3 runs under the sandbox");
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// `socket(AF_UNIX)` fails with `EPERM` in `workspace` (pathname and abstract sockets both
/// need `socket` first) and succeeds in `full`. Removing the `AF_UNIX` rule (or the whole
/// `--seccomp` install) lets the sandboxed creation succeed and fails this test.
#[test]
fn unix_socket_creation_fails_with_eperm_in_workspace() {
    let Some((_dir, sandbox, control)) = rig("unix-socket-creation") else {
        return;
    };
    // The script exits 0 only when the creation failed with exactly EPERM.
    let eperm_only = "import socket, errno, sys\n\
        try:\n    socket.socket(socket.AF_UNIX)\n    print('CREATED')\n    sys.exit(10)\n\
        except OSError as e:\n    print(f'ERRNO={e.errno}')\n    sys.exit(0 if e.errno == errno.EPERM else 11)";
    let (ok, text) = python(&sandbox, eperm_only);
    assert!(ok, "workspace socket(AF_UNIX) must fail with EPERM: {text}");
    // The control proves the script is sound: creation succeeds with no sandbox.
    let (ok, text) = python(&control, "import socket; socket.socket(socket.AF_UNIX)");
    assert!(ok, "control socket(AF_UNIX) must succeed: {text}");
}

/// A TCP socket still works in `workspace`: the tier keeps its network (ADR 0016), and
/// the filter only denies `AF_UNIX`. Narrowing the match to all of `socket` fails this.
#[test]
fn tcp_socket_still_works_in_workspace() {
    let Some((_dir, sandbox, control)) = rig("tcp-socket") else {
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let port = listener.local_addr().unwrap().port();
    let script = format!(
        "import socket; s = socket.create_connection(('127.0.0.1', {port}), 5); s.close()"
    );
    let (ok, text) = python(&sandbox, &script);
    assert!(ok, "workspace TCP connect must succeed: {text}");
    let (ok, text) = python(&control, &script);
    assert!(ok, "control TCP connect must succeed: {text}");
}

/// `socketpair` still works in `workspace`: an unnamed connected pair cannot reach
/// anything outside, so the filter leaves it alone. Blocking it fails this.
#[test]
fn socketpair_still_works_in_workspace() {
    let Some((_dir, sandbox, control)) = rig("socketpair") else {
        return;
    };
    let script = "import socket; a, b = socket.socketpair(); a.send(b'x'); assert b.recv(1) == b'x'";
    let (ok, text) = python(&sandbox, script);
    assert!(ok, "workspace socketpair must succeed: {text}");
    let (ok, text) = python(&control, script);
    assert!(ok, "control socketpair must succeed: {text}");
}

/// `io_uring_setup` fails with `EPERM` in `workspace` (`IORING_OP_SOCKET` would create a
/// socket without the `socket` syscall) and does something else without the sandbox (on
/// this box: `EFAULT` for the null params). Removing the rule fails this test.
#[test]
fn io_uring_setup_fails_with_eperm_in_workspace() {
    let Some((_dir, sandbox, control)) = rig("io-uring") else {
        return;
    };
    // Exits 0 only when the syscall failed with exactly EPERM (prints rc and errno).
    let eperm_only = "import ctypes, sys\n\
        libc = ctypes.CDLL(None, use_errno=True)\n\
        r = libc.syscall(425, 8, 0)\n\
        e = ctypes.get_errno()\n\
        print(f'rc={r} errno={e}')\n\
        sys.exit(0 if (r == -1 and e == 1) else 1)";
    let (ok, text) = python(&sandbox, eperm_only);
    assert!(ok, "workspace io_uring_setup must fail with EPERM: {text}");
    let (ok, text) = python(&control, eperm_only);
    assert!(
        !ok,
        "control io_uring_setup must not fail with EPERM (the filter is what adds it): {text}"
    );
}

/// `full` runs no sandbox, so nothing about sockets changes there: creation succeeds and
/// no `--seccomp` is installed (the bwrap unit test pins the argv; this pins the effect).
#[test]
fn full_leaves_unix_sockets_alone() {
    let Some((_dir, _sandbox, control)) = rig("full-sockets") else {
        return;
    };
    assert_eq!(control.backend_name(), "none");
    let (ok, text) = python(&control, "import socket; socket.socket(socket.AF_UNIX)");
    assert!(ok, "full socket(AF_UNIX) must succeed: {text}");
}

/// A 64-bit binary that reaches 32-bit and x32 syscalls (x86_64 only): `int 0x80` arrives
/// with `seccomp_data.arch == AUDIT_ARCH_I386`, and an `0x40000000`-tagged number is the
/// x32 table. Both must fail with `EPERM` in `workspace` and succeed in `full`. Removing
/// the arch check lets the `int 0x80` socket through; removing the x32 check lets the
/// x32 socket through. No 32-bit libc is needed: only the syscall entry is 32-bit, the
/// binary itself is an ordinary 64-bit one built with the host `cc`.
#[cfg(target_arch = "x86_64")]
#[test]
fn foreign_arch_syscalls_fail_with_eperm_in_workspace() {
    const HELPER: &str = r#"
#include <errno.h>
#include <stdio.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>
static int fails_eperm(long rc) { return rc == -1 && errno == EPERM; }
int main(void) {
    // 1. socketcall SYS_SOCKET via int 0x80 (arch AUDIT_ARCH_I386).
    long args[3] = { AF_UNIX, SOCK_STREAM, 0 };
    long fd;
    __asm__ volatile("int $0x80" : "=a"(fd) : "a"(102), "b"(1), "c"(args) : "memory");
    if (!fails_eperm(fd)) { printf("socketcall: rc=%ld errno=%d\n", fd, errno); return 11; }
    // 2. the x32 socket number through the syscall instruction.
    errno = 0;
    long xfd = syscall(0x40000000L + 41, AF_UNIX, SOCK_STREAM, 0);
    if (!fails_eperm(xfd)) { printf("x32: rc=%ld errno=%d\n", xfd, errno); return 12; }
    // 3. the ordinary number, for the shape of the failure.
    errno = 0;
    long nfd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (!fails_eperm(nfd)) { printf("socket: rc=%ld errno=%d\n", nfd, errno); return 13; }
    printf("all denied with EPERM\n");
    return 0;
}
"#;
    let Some((_dir, sandbox, control)) = rig("foreign-arch") else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("radian-seccomp-cc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("arch_probe.c");
    let bin = dir.join("arch_probe");
    std::fs::write(&src, HELPER).unwrap();
    let built = std::process::Command::new("cc")
        .args(["-O2", "-o"])
        .arg(&bin)
        .arg(&src)
        .output();
    let _ = std::fs::remove_dir_all(&dir);
    let built = match built {
        Ok(out) if out.status.success() && bin.exists() => bin,
        _ => {
            println!("radian-test: skipped foreign-arch: no working host cc");
            return;
        }
    };
    // The helper must be reachable: copy it into the workspace (only the workspace is
    // writable under the sandbox; the binary itself lives outside).
    let ws_bin = sandbox.policy().workspace.join("arch_probe");
    std::fs::copy(&built, &ws_bin).unwrap();
    let run = |sandbox: &Sandbox| {
        sandbox
            .command(ws_bin.to_str().unwrap(), std::iter::empty::<&str>())
            .unwrap()
            .output()
            .expect("the probe runs")
    };
    let out = run(&sandbox);
    let text = String::from_utf8_lossy(&out.stdout).into_owned()
        + &String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "workspace foreign-arch syscalls must fail with EPERM: {text}"
    );
    let out = run(&control);
    assert!(
        !out.status.success(),
        "control foreign-arch syscalls must succeed (no sandbox): {}",
        String::from_utf8_lossy(&out.stdout)
    );
}
