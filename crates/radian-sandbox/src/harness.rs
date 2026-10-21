//! The escape matrix (feature `testing`): every "must fail" case runs the same command
//! under an enforced sandbox and, as a control, under `FullAccess`. A sandboxed failure
//! with no successful control proves nothing — it could be a missing file or a typo — so
//! the control is mandatory and [`Case::passed`] requires it.
//!
//! The same matrix runs on macOS (locally) and Linux (via the `sandbox-escape` driver built
//! for the VM). It touches only its own scratch directory and never reads a real credential:
//! the sensitive-read case redirects to `/dev/null` and only inspects the exit status.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::policy::{SandboxMode, SandboxPolicy};
use crate::{Sandbox, SandboxCommand, SandboxError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    MustFail,
    MustSucceed,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub succeeded: bool,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct Case {
    pub group: &'static str,
    pub name: String,
    pub expect: Expect,
    pub sandbox: Outcome,
    pub control: Outcome,
    /// `Some(reason)` when this machine cannot exercise the case at all, so a sandbox
    /// "failure" would prove nothing: the case renders as `SKIP` and never counts as a
    /// failure. The `TIOCSTI` case is the one user today (the kernel can refuse the ioctl for
    /// the control too).
    pub skip: Option<String>,
    pub note: String,
}

impl Case {
    /// A case passes only when the sandbox behaved as expected **and** the control confirms
    /// the command itself is sound: an escape must succeed without the sandbox; ordinary use
    /// must succeed both ways. A skipped case ([`Self::skip`]) never fails the matrix: there
    /// is no working control, so it is reported as `SKIP`, not as a pass and not as a failure.
    pub fn passed(&self) -> bool {
        if self.skip.is_some() {
            return true;
        }
        match self.expect {
            Expect::MustFail => !self.sandbox.succeeded && self.control.succeeded,
            Expect::MustSucceed => self.sandbox.succeeded && self.control.succeeded,
        }
    }

    /// Whether this case was skipped for this machine (see [`Self::skip`]).
    pub fn skipped(&self) -> bool {
        self.skip.is_some()
    }

    /// The verdict: a skipped case is [`Verdict::Skip`] — without a working control it
    /// proves nothing, so it never counts as a failure. Anything else is [`Verdict::Pass`]
    /// or [`Verdict::Fail`]. (Slice 0 closed the old Linux Unix-socket known limit: the
    /// seccomp filter denies every Unix socket, so the denial cases must pass like any
    /// other case. There is no per-backend exception left.)
    pub fn verdict(&self) -> Verdict {
        if self.skipped() {
            Verdict::Skip
        } else if self.passed() {
            Verdict::Pass
        } else {
            Verdict::Fail
        }
    }
}

/// What one matrix case counts as: a pass, a failure, or a skip (no working
/// control, so it proves nothing and never counts as a failure either).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
    Skip,
}

pub struct Matrix {
    pub backend: String,
    pub probe: String,
    pub cases: Vec<Case>,
    pub orphans: String,
    pub root: PathBuf,
}

impl Matrix {
    /// The matrix passes when no case fails: a [`Verdict::Skip`] has no working control,
    /// so it is never a failure and never a pass.
    pub fn passed(&self) -> bool {
        self.cases.iter().all(|c| c.verdict() != Verdict::Fail)
    }

    pub fn failures(&self) -> Vec<&Case> {
        self.cases
            .iter()
            .filter(|c| c.verdict() == Verdict::Fail)
            .collect()
    }

    /// Cases skipped on this machine (no working control). Never a failure and never a pass.
    pub fn skipped(&self) -> Vec<&Case> {
        self.cases
            .iter()
            .filter(|c| c.verdict() == Verdict::Skip)
            .collect()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "backend": self.backend,
            "probe": self.probe,
            "passed": self.passed(),
            "cases": self.cases.iter().map(|c| json!({
                "group": c.group,
                "name": c.name,
                "expect": match c.expect { Expect::MustFail => "must_fail", Expect::MustSucceed => "must_succeed" },
                "sandbox_succeeded": c.sandbox.succeeded,
                "sandbox_detail": c.sandbox.detail,
                "control_succeeded": c.control.succeeded,
                "control_detail": c.control.detail,
                "passed": c.passed(),
                "verdict": match c.verdict() { Verdict::Pass => "PASS", Verdict::Fail => "FAIL", Verdict::Skip => "SKIP" },
                "skipped": c.skipped(),
                "skip": c.skip,
                "note": c.note,
            })).collect::<Vec<_>>(),
            "orphans": self.orphans,
        })
    }

    pub fn render_text(&self) -> String {
        let mut out = format!("backend: {}\nprobe: {}\n\n", self.backend, self.probe);
        out.push_str(&format!(
            "{:<16} {:<26} {:>8} {:>8} {:>8}  {}\n",
            "group", "case", "expect", "sandbox", "control", "verdict"
        ));
        for c in &self.cases {
            let verdict = match c.verdict() {
                Verdict::Pass => "PASS".to_string(),
                Verdict::Fail => "FAIL".to_string(),
                Verdict::Skip => "SKIP".to_string(),
            };
            out.push_str(&format!(
                "{:<16} {:<26} {:>8} {:>8} {:>8}  {}\n",
                c.group,
                c.name,
                match c.expect {
                    Expect::MustFail => "fail",
                    Expect::MustSucceed => "ok",
                },
                if c.sandbox.succeeded { "ok" } else { "denied" },
                if c.control.succeeded { "ok" } else { "denied" },
                verdict,
            ));
        }
        let skipped = self.skipped();
        for c in &skipped {
            out.push_str(&format!(
                "skipped: {} / {}: {}\n",
                c.group,
                c.name,
                c.skip.as_deref().unwrap_or("")
            ));
        }
        out.push_str(&format!(
            "\n{} cases, {} failed, {} skipped\n\norphan check:\n{}\n",
            self.cases.len(),
            self.failures().len(),
            skipped.len(),
            self.orphans
        ));
        out
    }
}

struct Rig {
    ws: PathBuf,
    outside: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    main: PathBuf,
    wt: PathBuf,
    /// A sessions directory moved elsewhere (`--session-dir`), which must also be denied.
    custom_sessions: PathBuf,
    /// A session directory **inside** the workspace (the reverse of the usual layout): it is
    /// allowed, but must be masked while the rest of the workspace stays usable (ADR 0020).
    inside_sessions: PathBuf,
    worktree_name: String,
    cases: Vec<Case>,
}

/// Runs the whole matrix under the platform backend. Fails if the backend is unavailable or
/// its probe fails — the enforced half never runs unsandboxed.
/// A unique suffix for the short Unix socket path (one per matrix run in this process).
fn unix_socket_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

pub fn run_matrix() -> Result<Matrix, SandboxError> {
    let root = new_root()?;
    let ws = std::fs::canonicalize(root.join("ws"))?;
    let outside = std::fs::canonicalize(root.join("outside"))?;
    let home = std::fs::canonicalize(root.join("home"))?;
    let repo = std::fs::canonicalize(root.join("ws/proj"))?;
    let main = std::fs::canonicalize(root.join("main"))?;
    let wt = std::fs::canonicalize(root.join("ws/wt"))?;

    // A fake credential that must stay unreadable. The test never reads it either.
    std::fs::create_dir_all(home.join(".ssh"))?;
    std::fs::write(home.join(".ssh/id_ed25519"), "FAKE-KEY-NOT-A-REAL-SECRET\n")?;
    std::fs::write(home.join("readable.txt"), "plain\n")?;
    // ADR 0020: a fake `~/.radian/` with the four subdirectories, so the matrix can assert
    // sessions/memory are unreadable and config/cache are readable. The test only ever reads
    // these through the sandbox (redirected to /dev/null) or not at all.
    let radian_home = home.join(".radian");
    for (dir, file) in [
        ("sessions", "s.jsonl"),
        ("memory", "m.txt"),
        ("config", "c.toml"),
        ("cache", "cache.txt"),
        ("credentials", "auth.json"),
    ] {
        std::fs::create_dir_all(radian_home.join(dir))?;
        std::fs::write(radian_home.join(dir).join(file), "radian-data\n")?;
    }
    // ADR 0025 ruling 1: `skills/` is deliberately **not** on the deny list — a skill's body is
    // read by the model and its scripts run inside the sandbox.
    std::fs::create_dir_all(radian_home.join("skills/greet"))?;
    std::fs::write(radian_home.join("skills/greet/SKILL.md"), "radian-data\n")?;
    // ADR 0031 T2: `trust/` is on the deny list — the user's map of trusted directories.
    std::fs::create_dir_all(radian_home.join("trust"))?;
    std::fs::write(radian_home.join("trust/trust.json"), "{}\n")?;
    // A sessions directory moved elsewhere (`--session-dir`): it must be denied too.
    std::fs::create_dir_all(root.join("custom-sessions"))?;
    let custom_sessions = std::fs::canonicalize(root.join("custom-sessions"))?;
    std::fs::write(custom_sessions.join("x.jsonl"), "{}\n")?;
    // The reverse layout: a session directory *inside* the workspace. Allowed, but masked.
    let inside_sessions = ws.join("inside-sessions");
    std::fs::create_dir_all(&inside_sessions)?;
    std::fs::write(inside_sessions.join("s.jsonl"), "{}\n")?;
    // A symlink from inside the workspace to a sibling outside it.
    symlink(&outside, &ws.join("leak"))?;
    // A symlink from inside the worktree to the shared git directory.
    symlink(&main.join(".git"), &wt.join("leakgit"))?;

    let radian_deny = |policy: SandboxPolicy| {
        policy
            .deny_read(home.join(".ssh"))
            .deny_read(radian_home.join("sessions"))
            .deny_read(radian_home.join("memory"))
            // ADR 0026 D4: the credential store is denied in every enforced tier.
            .deny_read(radian_home.join("credentials"))
            // ADR 0031 T2: the trust store is denied too.
            .deny_read(radian_home.join("trust"))
            .deny_read(&custom_sessions)
    };
    let policy = radian_deny(SandboxPolicy::workspace_write(&ws)?);
    let sandbox = Sandbox::from_policy(policy.clone())?;
    // The reverse layout builds: the deny path is a child of the workspace, not a parent.
    let inside_policy =
        radian_deny(SandboxPolicy::workspace_write(&ws)?).deny_read(&inside_sessions);
    let denied_inside = Sandbox::from_policy(inside_policy)?;
    // ADR 0016: the network cases are asserted against `workspace` (on),
    // so the rig needs the enforced sandbox as well as the transparent control.
    // ADR 0016: a listener outside the workspace, and a `workspace` sandbox that
    // whitelists exactly that socket. The listener stays bound (connect fills its backlog).
    // The AF_UNIX path limit (~104 bytes) is why this lives under `/tmp` and not the test
    // root: the sandbox canonicalizes it (`/tmp` -> `/private/tmp`), so the connect and the
    // whitelist both use the canonical path.
    let short = std::path::PathBuf::from(format!(
        "/tmp/radian-esc-{}-{}.sock",
        std::process::id(),
        unix_socket_seq()
    ));
    let _unix_listener = std::os::unix::net::UnixListener::bind(&short)?;
    let unix_listener_path = std::fs::canonicalize(&short)?;
    // Slice 0: an abstract-namespace listener too (Linux only: macOS has no abstract
    // namespace, so the case below is recorded only where the control can connect).
    // The seccomp filter denies `socket(AF_UNIX, …)` before any connect, pathname and
    // abstract alike.
    #[cfg(target_os = "linux")]
    let (abstract_name, _abstract_listener) = {
        use std::os::linux::net::SocketAddrExt;
        let name = format!("radian-esc-{}-{}", std::process::id(), unix_socket_seq());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
        let listener = std::os::unix::net::UnixListener::bind_addr(&addr)?;
        (name, listener)
    };
    let whitelisted = Sandbox::from_policy(
        radian_deny(SandboxPolicy::workspace_write(&ws)?).allow_unix_socket(&unix_listener_path),
    )?;
    let control = Sandbox::new(SandboxMode::FullAccess, &ws)?;
    let worktree_name = wt
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "wt".into());
    let (secret_name, secret_value) = pick_unwhitelisted_env(&policy);

    let mut rig = Rig {
        ws,
        outside,
        home,
        repo,
        main,
        wt,
        custom_sessions,
        inside_sessions,
        worktree_name,
        cases: Vec::new(),
    };
    rig.escape_writes(&sandbox, &control);
    rig.symlink_escapes(&sandbox, &control);
    rig.network(&sandbox, &control);
    // The abstract listener binds only on Linux; elsewhere there is no case for it.
    #[cfg(target_os = "linux")]
    let abstract_name: Option<&str> = Some(abstract_name.as_str());
    #[cfg(not(target_os = "linux"))]
    let abstract_name: Option<&str> = None;
    rig.unix_socket(
        &sandbox,
        &whitelisted,
        &control,
        &unix_listener_path,
        abstract_name,
    );
    rig.pty_ok(&sandbox, &control);
    rig.tiocsti_outside_terminal(&sandbox, &control);
    rig.environment(&sandbox, &control, &secret_name, &secret_value);
    rig.nested_shell(&sandbox, &control);
    rig.normal_use(&sandbox, &control);
    rig.git_metadata(&sandbox, &control);
    rig.sensitive_read(&sandbox, &control);
    rig.radian_home(&sandbox, &control);
    rig.deny_inside_workspace(&denied_inside, &control);
    rig.mcp_stdio(&sandbox, &control);
    rig.signals(&sandbox, &control);

    let orphans = orphan_report();
    let matrix = Matrix {
        backend: sandbox.backend_name().to_string(),
        probe: sandbox.probe().detail.clone(),
        cases: rig.cases,
        orphans,
        root,
    };
    let _ = std::fs::remove_file(&unix_listener_path);
    if matrix.passed() {
        let _ = std::fs::remove_dir_all(&matrix.root);
    }
    Ok(matrix)
}

impl Rig {
    /// Records a case and returns it, so a caller can mark it skipped (see [`Case::skip`]).
    fn record(
        &mut self,
        group: &'static str,
        name: &str,
        expect: Expect,
        sandbox: Outcome,
        control: Outcome,
        note: &str,
    ) -> &mut Case {
        self.cases.push(Case {
            group,
            name: name.to_string(),
            expect,
            sandbox,
            control,
            skip: None,
            note: note.to_string(),
        });
        self.cases.last_mut().expect("just recorded")
    }

    fn sh_case(
        &mut self,
        sandbox: &Sandbox,
        control: &Sandbox,
        group: &'static str,
        name: &str,
        expect: Expect,
        script: &str,
    ) {
        let sb = run_shell(sandbox, script);
        let ct = run_shell(control, script);
        self.record(group, name, expect, sb, ct, "");
    }

    fn escape_writes(&mut self, sandbox: &Sandbox, control: &Sandbox) {
        let g = "escape_write";
        let abs = self.outside.join("absolute.txt");
        self.sh_case(
            sandbox,
            control,
            g,
            "outside_absolute",
            Expect::MustFail,
            &format!("printf x > {}", q(&abs)),
        );
        self.sh_case(
            sandbox,
            control,
            g,
            "outside_dotdot",
            Expect::MustFail,
            &format!("cd {} && printf x > ../outside/dotdot.txt", q(&self.ws)),
        );
        self.sh_case(
            sandbox,
            control,
            g,
            "outside_via_symlink",
            Expect::MustFail,
            &format!("printf x > {}", q(&self.ws.join("leak/via_symlink.txt"))),
        );
        self.sh_case(
            sandbox,
            control,
            g,
            "outside_tmp",
            Expect::MustFail,
            &format!("printf x > {}", q(&self.outside.join("tmp.txt"))),
        );
    }

    fn symlink_escapes(&mut self, sandbox: &Sandbox, control: &Sandbox) {
        let g = "symlink";
        self.sh_case(
            sandbox,
            control,
            g,
            "symlinked_parent_dir",
            Expect::MustFail,
            &format!("printf x > {}", q(&self.ws.join("leak/new.txt"))),
        );
        self.sh_case(
            sandbox,
            control,
            g,
            "write_through_workspace_symlink",
            Expect::MustFail,
            &format!("printf x > {}", q(&self.ws.join("leak/again.txt"))),
        );
    }

    fn network(&mut self, workspace: &Sandbox, control: &Sandbox) {
        let connect_script = |port: u16| {
            format!(
                "python3 -c 'import socket,sys; s=socket.create_connection((\"127.0.0.1\",{port}),2); s.close(); sys.exit(0)'"
            )
        };
        // `workspace` (ADR 0016) can connect to loopback, bind/listen, and reach out.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local listener");
        let port = listener.local_addr().unwrap().port();
        let script = connect_script(port);
        self.sh_case(
            workspace,
            control,
            "network_workspace",
            "connect_local_port",
            Expect::MustSucceed,
            &script,
        );
        drop(listener);
        self.sh_case(
            workspace,
            control,
            "network_workspace",
            "bind_local_port",
            Expect::MustSucceed,
            "python3 -c 'import socket; s=socket.socket(); s.bind((\"127.0.0.1\",0)); s.listen(1); s.close()'",
        );
        // An actual outbound connection (the ADR measured 1.1.1.1:443). Both the sandbox and
        // the control need it to succeed, so a machine without internet fails the case.
        self.sh_case(
            workspace,
            control,
            "network_workspace",
            "connect_external",
            Expect::MustSucceed,
            "python3 -c 'import socket; s=socket.create_connection((\"1.1.1.1\",443),4); s.close()'",
        );
    }

    /// Slice 0: the `workspace` tier denies **every** Unix socket on Linux — the seccomp
    /// filter rejects `socket(AF_UNIX, …)` before any connect, pathname and abstract alike,
    /// and seccomp cannot see paths so the whitelist is ignored there (it warns once at
    /// startup). macOS seatbelt keeps its path whitelist unchanged. DNS still resolves
    /// (mDNSResponder stays allowed on macOS; on Linux glibc falls back off the denied
    /// socket).
    fn unix_socket(
        &mut self,
        workspace: &Sandbox,
        whitelisted: &Sandbox,
        control: &Sandbox,
        sock: &Path,
        abstract_name: Option<&str>,
    ) {
        let connect = format!(
            "python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect({:?}); s.close(); sys.exit(0)'",
            sock.to_string_lossy()
        );
        // `workspace`: the socket is denied (on Linux by the seccomp filter).
        self.sh_case(
            workspace,
            control,
            "unix_socket",
            "workspace_denied",
            Expect::MustFail,
            &connect,
        );
        // The abstract namespace: the same deny, through an abstract listener (Linux only —
        // macOS has no abstract namespace, so no control could connect there).
        if let Some(name) = abstract_name {
            let connect_abstract = format!(
                "python3 -c 'import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(\"\\0{name}\"); s.close(); sys.exit(0)'"
            );
            self.sh_case(
                workspace,
                control,
                "unix_socket",
                "abstract_denied",
                Expect::MustFail,
                &connect_abstract,
            );
        }
        // The same socket in `allow_unix_sockets`: honored on macOS (the control for the
        // fix), ignored on Linux where the filter denies it anyway.
        let (name, expect, note) = if workspace.backend_name() == "linux-bwrap" {
            (
                "whitelisted_denied",
                Expect::MustFail,
                "Linux ignores allow_unix_sockets: seccomp cannot whitelist by path",
            )
        } else {
            (
                "whitelisted_allowed",
                Expect::MustSucceed,
                "the control for the fix",
            )
        };
        let sb = run_shell(whitelisted, &connect);
        let ct = run_shell(control, &connect);
        self.record("unix_socket", name, expect, sb, ct, note);
        // DNS resolution uses mDNSResponder, which stays allowed when the network is on.
        self.sh_case(
            workspace,
            control,
            "unix_socket",
            "dns_resolves",
            Expect::MustSucceed,
            "python3 -c 'import socket; socket.getaddrinfo(\"example.com\", 443)'",
        );
    }

    /// ADR 0016: the `workspace` tier can `openpty()` (the pty device rules).
    fn pty_ok(&mut self, sandbox: &Sandbox, control: &Sandbox) {
        self.sh_case(
            sandbox,
            control,
            "devices",
            "openpty",
            Expect::MustSucceed,
            "python3 -c 'import pty; m, s = pty.openpty()'",
        );
    }

    /// ADR 0016: a sandboxed command must not be able to `TIOCSTI` into a terminal held
    /// **outside** the sandbox. The judge is the master side: it must receive no byte from the
    /// sandbox and must receive the injected byte from the control, which makes the same
    /// outside slave its controlling terminal (proving the test can inject at all).
    #[cfg(unix)]
    fn tiocsti_outside_terminal(&mut self, sandbox: &Sandbox, control: &Sandbox) {
        let Some(pty) = OutsidePty::open() else {
            self.record(
                "devices",
                "tiocsti_outside_terminal",
                Expect::MustFail,
                Outcome {
                    succeeded: true,
                    detail: "could not create an outside pty".into(),
                },
                Outcome {
                    succeeded: false,
                    detail: "could not create an outside pty".into(),
                },
                "setup failed: no outside pty",
            );
            return;
        };
        let slave = pty.slave_path.to_string_lossy().into_owned();
        // The sandboxed command only does TIOCSTI: without a controlling terminal (or with a
        // different one) the kernel refuses it.
        let no_ctty = format!(
            "python3 -c 'import os,fcntl,termios,sys; fd=os.open(sys.argv[1], os.O_RDWR|os.O_NOCTTY); fcntl.ioctl(fd, termios.TIOCSTI, b\"X\")' {slave}"
        );
        let (sb_exit, sb_bytes) = run_shell_with_pty(sandbox, &no_ctty, &pty);
        // The control makes the outside slave its controlling terminal first, so the ioctl is
        // allowed and the byte is echoed back to the master.
        let with_ctty = format!(
            "python3 -c 'import os,fcntl,termios,sys; os.setsid(); fd=os.open(sys.argv[1], os.O_RDWR|os.O_NOCTTY); fcntl.ioctl(fd, termios.TIOCSCTTY, 0); fcntl.ioctl(fd, termios.TIOCSTI, b\"X\")' {slave}"
        );
        let (ct_exit, ct_bytes) = run_shell_with_pty(control, &with_ctty, &pty);
        let sandbox_succeeded = !sb_bytes.is_empty();
        let control_succeeded = !ct_bytes.is_empty();
        let sandbox_outcome = Outcome {
            succeeded: sandbox_succeeded,
            detail: format!(
                "master_bytes={} injector={}",
                sb_bytes.len(),
                sb_exit.detail
            ),
        };
        let control_outcome = Outcome {
            succeeded: control_succeeded,
            detail: format!(
                "master_bytes={} injector={}",
                ct_bytes.len(),
                ct_exit.detail
            ),
        };
        let note = "the sandbox cannot inject into an outside terminal; the control (same slave \
                    as its controlling terminal) can";
        let case = self.record(
            "devices",
            "tiocsti_outside_terminal",
            Expect::MustFail,
            sandbox_outcome,
            control_outcome,
            note,
        );
        // No working control means the case proves nothing on this machine (the kernel refuses
        // `TIOCSTI` for the control too); it is reported as skipped, never failed.
        case.skip = tiocsti_skip_reason(control_succeeded);
    }

    fn environment(&mut self, sandbox: &Sandbox, control: &Sandbox, name: &str, value: &str) {
        let g = "environment";
        let script = format!("printf %s \"${{{name}-<unset>}}\"");
        let sb = run_shell(sandbox, &script);
        let leaked = sb.detail.contains(value);
        let ct = run_shell(control, &script);
        self.record(
            g,
            "unlisted_var_hidden",
            Expect::MustFail,
            Outcome {
                succeeded: leaked,
                detail: sb.detail,
            },
            Outcome {
                succeeded: ct.detail.contains(value),
                detail: ct.detail,
            },
            "the sandbox clears the environment; the control inherits it",
        );
        // A whitelisted variable is visible in both.
        let script = "printf %s \"${PATH-<unset>}\"";
        self.sh_case(
            sandbox,
            control,
            g,
            "whitelisted_path_visible",
            Expect::MustSucceed,
            script,
        );
    }

    fn nested_shell(&mut self, sandbox: &Sandbox, control: &Sandbox) {
        let g = "inheritance";
        let target = self.outside.join("nested.txt");
        self.sh_case(
            sandbox,
            control,
            g,
            "nested_sh_confined",
            Expect::MustFail,
            &format!("sh -c 'sh -c \"printf x > {}\"'", target.display()),
        );
        let inside = self.ws.join("nested-inside.txt");
        self.sh_case(
            sandbox,
            control,
            g,
            "nested_sh_inside_ok",
            Expect::MustSucceed,
            &format!("sh -c 'sh -c \"printf x > {}\"'", inside.display()),
        );
    }

    /// A sandboxed command may signal what it started (children and grandchildren: test
    /// suites and build tools kill their own helpers), never a process outside the sandbox.
    fn signals(&mut self, sandbox: &Sandbox, control: &Sandbox) {
        let g = "signals";
        self.sh_case(
            sandbox,
            control,
            g,
            "signal_own_child_ok",
            Expect::MustSucceed,
            "sleep 5 & c=$!; kill -TERM $c",
        );
        // The grandchild's pid comes back through a FIFO in the workspace: `read` blocks until
        // the middle shell has written it, so there is no polling. The scratch path has no
        // quotes or spaces (it is under the test's own temp root), so it is used bare inside
        // the inner `sh -c '...'`.
        let fifo = self.ws.join("grandchild.fifo");
        let f = fifo.display();
        self.sh_case(
            sandbox,
            control,
            g,
            "signal_own_grandchild_ok",
            Expect::MustSucceed,
            &format!(
                "rm -f {f}; mkfifo {f} || exit 2; sh -c 'sleep 5 & echo $! > {f}; wait' & m=$!; \
                 read g < {f}; kill -TERM $g; r=$?; wait $m; rm -f {f}; exit $r"
            ),
        );
        // A same-user process started by this test, outside any sandbox. One per run: the
        // control is expected to kill its own.
        let mut outside_runs = Vec::new();
        for sb in [sandbox, control] {
            let mut target = Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn an outside process");
            let outcome = run_shell(sb, &format!("kill -TERM {}", target.id()));
            let _ = target.kill();
            let _ = target.wait();
            outside_runs.push(outcome);
        }
        let ct = outside_runs.pop().unwrap();
        let sb = outside_runs.pop().unwrap();
        self.record(
            g,
            "signal_outside_process",
            Expect::MustFail,
            sb,
            ct,
            "a same-user process outside the sandbox, started by this test",
        );
    }

    fn normal_use(&mut self, sandbox: &Sandbox, control: &Sandbox) {
        let g = "normal_use";
        let inside = self.ws.join("in.txt");
        self.sh_case(
            sandbox,
            control,
            g,
            "write_read_inside",
            Expect::MustSucceed,
            &format!(
                "printf hello > {p} && test \"$(cat {p})\" = hello",
                p = q(&inside)
            ),
        );
        // Reads outside the workspace are allowed (read-mostly boundary, ADR 0012 P4).
        self.sh_case(
            sandbox,
            control,
            g,
            "read_outside_allowed",
            Expect::MustSucceed,
            &format!("cat {} > /dev/null", q(&self.home.join("readable.txt"))),
        );
    }

    fn git_metadata(&mut self, sandbox: &Sandbox, control: &Sandbox) {
        let g = "git_readonly";
        let p = q(&self.repo);
        for (name, script) in [
            (
                "write_hook",
                format!("printf x > {p}/.git/hooks/pre-commit"),
            ),
            ("write_config", format!("printf x > {p}/.git/config")),
            ("write_info", format!("printf x > {p}/.git/info/exclude")),
            (
                "git_config_hookspath",
                format!("cd {p} && git config core.hooksPath /tmp/elsewhere"),
            ),
            ("git_add", format!("cd {p} && git add new.txt")),
        ] {
            self.sh_case(sandbox, control, g, name, Expect::MustFail, &script);
        }
        // The worktree's pointer file and the shared common directory.
        self.sh_case(
            sandbox,
            control,
            g,
            "worktree_pointer",
            Expect::MustFail,
            &format!("printf x > {}", q(&self.wt.join(".git"))),
        );
        self.sh_case(
            sandbox,
            control,
            g,
            "shared_gitdir",
            Expect::MustFail,
            &format!(
                "printf x > {}",
                q(&self
                    .main
                    .join(".git/worktrees")
                    .join(&self.worktree_name)
                    .join("gitdir"))
            ),
        );
        self.sh_case(
            sandbox,
            control,
            g,
            "shared_git_via_symlink",
            Expect::MustFail,
            &format!("printf x > {}", q(&self.wt.join("leakgit/hooks/x"))),
        );
        // Read-only git still works through the carve-outs.
        self.sh_case(
            sandbox,
            control,
            g,
            "git_status_ok",
            Expect::MustSucceed,
            &format!("cd {p} && git --no-optional-locks status --porcelain"),
        );
        self.sh_case(
            sandbox,
            control,
            g,
            "git_log_ok",
            Expect::MustSucceed,
            &format!("cd {p} && git log --oneline -1"),
        );
    }

    fn sensitive_read(&mut self, sandbox: &Sandbox, control: &Sandbox) {
        let g = "sensitive_read";
        // Redirect to /dev/null: we only ever see the exit status, never the bytes.
        self.sh_case(
            sandbox,
            control,
            g,
            "fake_ssh_key_unreadable",
            Expect::MustFail,
            &format!("cat {} > /dev/null", q(&self.home.join(".ssh/id_ed25519"))),
        );
        self.sh_case(
            sandbox,
            control,
            g,
            "normal_file_still_readable",
            Expect::MustSucceed,
            &format!("cat {} > /dev/null", q(&self.home.join("readable.txt"))),
        );
    }

    /// ADR 0020: `~/.radian/sessions` and `~/.radian/memory` are not readable; `config` and
    /// `cache` are. A sessions directory moved by `--session-dir` is denied as well. Each case
    /// carries the transparent control (which can read everything).
    fn radian_home(&mut self, sandbox: &Sandbox, control: &Sandbox) {
        let g = "radian_home";
        let radian = self.home.join(".radian");
        self.sh_case(
            sandbox,
            control,
            g,
            "sessions_unreadable",
            Expect::MustFail,
            &format!("cat {} > /dev/null", q(&radian.join("sessions/s.jsonl"))),
        );
        self.sh_case(
            sandbox,
            control,
            g,
            "memory_unreadable",
            Expect::MustFail,
            &format!("cat {} > /dev/null", q(&radian.join("memory/m.txt"))),
        );
        // ADR 0026 D4: `credentials/auth.json` is denied; the control (FullAccess) reads it,
        // so the denial is the sandbox, not a missing file.
        self.sh_case(
            sandbox,
            control,
            g,
            "credentials_unreadable",
            Expect::MustFail,
            &format!(
                "cat {} > /dev/null",
                q(&radian.join("credentials/auth.json"))
            ),
        );
        // ADR 0031 T2 / ADR 0020 revision: `trust/` is denied like sessions, memory and
        // credentials; the control (FullAccess) reads it, so the denial is the sandbox.
        self.sh_case(
            sandbox,
            control,
            g,
            "trust_unreadable",
            Expect::MustFail,
            &format!("cat {} > /dev/null", q(&radian.join("trust/trust.json"))),
        );
        // ADR 0025 ruling 1 / ADR 0020 revision: `skills/` joins config and cache on the
        // readable side; only sessions, memory, credentials and trust are denied.
        self.sh_case(
            sandbox,
            control,
            g,
            "skills_readable",
            Expect::MustSucceed,
            &format!(
                "cat {} > /dev/null",
                q(&radian.join("skills/greet/SKILL.md"))
            ),
        );
        self.sh_case(
            sandbox,
            control,
            g,
            "config_readable",
            Expect::MustSucceed,
            &format!("cat {} > /dev/null", q(&radian.join("config/c.toml"))),
        );
        self.sh_case(
            sandbox,
            control,
            g,
            "cache_readable",
            Expect::MustSucceed,
            &format!("cat {} > /dev/null", q(&radian.join("cache/cache.txt"))),
        );
        self.sh_case(
            sandbox,
            control,
            g,
            "moved_sessions_unreadable",
            Expect::MustFail,
            &format!(
                "cat {} > /dev/null",
                q(&self.custom_sessions.join("x.jsonl"))
            ),
        );
    }

    /// ADR 0020, the reverse layout: a session directory nested in the workspace is allowed
    /// to build, is unreadable inside the sandbox, and the rest of the workspace still works.
    fn deny_inside_workspace(&mut self, sandbox: &Sandbox, control: &Sandbox) {
        let g = "deny_inside_workspace";
        self.sh_case(
            sandbox,
            control,
            g,
            "nested_sessions_unreadable",
            Expect::MustFail,
            &format!(
                "cat {} > /dev/null",
                q(&self.inside_sessions.join("s.jsonl"))
            ),
        );
        self.sh_case(
            sandbox,
            control,
            g,
            "workspace_elsewhere_readable",
            Expect::MustSucceed,
            &format!("cat {} > /dev/null", q(&self.ws.join("proj/readme.md"))),
        );
        let allowed = self.ws.join("allowed.txt");
        self.sh_case(
            sandbox,
            control,
            g,
            "workspace_elsewhere_writable",
            Expect::MustSucceed,
            &format!(
                "printf x > {} && cat {} > /dev/null",
                q(&allowed),
                q(&allowed)
            ),
        );
    }

    fn mcp_stdio(&mut self, sandbox: &Sandbox, control: &Sandbox) {
        let g = "mcp_stdio";
        let script_path = self.ws.join("mcp_echo.py");
        std::fs::write(&script_path, MCP_ECHO_PY).unwrap();
        let sb = run_mcp(sandbox, &script_path);
        let ct = run_mcp(control, &script_path);
        self.record(
            g,
            "initialize_list_echo",
            Expect::MustSucceed,
            sb,
            ct,
            "minimal stdio MCP server under the sandbox",
        );
    }
}

// ---------- running ----------

/// Whether the `TIOCSTI` case can prove anything on this machine. The judge is the pty master:
/// with a working control, a sandbox that manages to inject is a real failure. When the
/// **control** is denied too, no byte ever reaches the master, so a sandbox "failure" would be
/// meaningless — the kernel itself refuses the ioctl. Since Linux 6.2 `TIOCSTI` is off unless
/// `dev.tty.legacy_tiocsti` is 1 (the new-kernel default is 0), and it refuses the ioctl for
/// every process, the control included. `Some(reason)` marks the case skipped so
/// [`Matrix::failures`] does not count it; `None` keeps it a real PASS/FAIL.
#[cfg(unix)]
fn tiocsti_skip_reason(control_succeeded: bool) -> Option<String> {
    (!control_succeeded).then(|| {
        format!(
            "the control could not TIOCSTI either, so the case proves nothing: the kernel \
             refuses TIOCSTI for every process (dev.tty.legacy_tiocsti = {})",
            legacy_tiocsti()
        )
    })
}

/// The kernel's `dev.tty.legacy_tiocsti` knob. The file exists only on Linux; elsewhere (macOS)
/// the reason string says so.
#[cfg(unix)]
fn legacy_tiocsti() -> String {
    std::fs::read_to_string("/proc/sys/dev/tty/legacy_tiocsti")
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|_| "unavailable (not Linux)".to_owned())
}

fn run_shell(sandbox: &Sandbox, script: &str) -> Outcome {
    let command = match sandbox.command("/bin/sh", ["-c", script]) {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                succeeded: false,
                detail: format!("sandbox error: {e}"),
            };
        }
    };
    run_outcome(command)
}

/// Runs a shell script while draining an outside pty's master, so a control process that
/// makes that pty its controlling terminal can exit (macOS blocks the exit until the master
/// is read) and so the injected byte is observed on the master. Returns the child's outcome
/// and every byte read from the master.
#[cfg(unix)]
fn run_shell_with_pty(sandbox: &Sandbox, script: &str, pty: &OutsidePty) -> (Outcome, Vec<u8>) {
    let mut command = match sandbox.command("/bin/sh", ["-c", script]) {
        Ok(c) => c,
        Err(e) => {
            return (
                Outcome {
                    succeeded: false,
                    detail: format!("sandbox error: {e}"),
                },
                Vec::new(),
            );
        }
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            return (
                Outcome {
                    succeeded: false,
                    detail: format!("spawn error: {e}"),
                },
                Vec::new(),
            );
        }
    };
    let master = pty.master;
    let drain = std::thread::spawn(move || drain_master(master, Duration::from_secs(2)));
    let output = child.wait_with_output();
    let bytes = drain.join().unwrap_or_default();
    let outcome = match output {
        Ok(output) => outcome_from(output),
        Err(e) => Outcome {
            succeeded: false,
            detail: format!("wait error: {e}"),
        },
    };
    (outcome, bytes)
}

/// Reads an outside pty's master until a byte arrives, the slave side goes away, or the
/// deadline passes.
#[cfg(unix)]
fn drain_master(master: std::os::fd::RawFd, deadline: Duration) -> Vec<u8> {
    let start = std::time::Instant::now();
    let mut out = Vec::new();
    let mut buf = [0u8; 256];
    while start.elapsed() < deadline {
        let mut pfd = libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is one valid pollfd; poll only reads/writes it.
        let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
        if ready < 0 {
            break;
        }
        if ready == 0 || pfd.revents & libc::POLLIN == 0 {
            continue;
        }
        // SAFETY: the buffer is valid for `buf.len()` bytes.
        let n = unsafe { libc::read(master, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
        if n <= 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
        // One injected byte is enough for the judge.
        break;
    }
    out
}

/// A pty held **outside** any sandbox, used by the `TIOCSTI` case. The master is read by the
/// test; the slave path is handed to the sandboxed command.
#[cfg(unix)]
struct OutsidePty {
    master: std::os::fd::RawFd,
    /// Kept open for the case's lifetime: a master with no slave holder reads EIO, and the
    /// injected byte is buffered here until the drain reads it.
    slave: std::os::fd::RawFd,
    slave_path: std::ffi::CString,
}

#[cfg(unix)]
impl OutsidePty {
    fn open() -> Option<Self> {
        // SAFETY: plain libc calls with no memory-safety preconditions; every failure path
        // closes what it opened.
        unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            if master < 0 {
                return None;
            }
            if libc::grantpt(master) != 0 || libc::unlockpt(master) != 0 {
                libc::close(master);
                return None;
            }
            let name = libc::ptsname(master);
            if name.is_null() {
                libc::close(master);
                return None;
            }
            let slave_path = std::ffi::CStr::from_ptr(name).to_owned();
            let slave = libc::open(name, libc::O_RDWR | libc::O_NOCTTY);
            if slave < 0 {
                libc::close(master);
                return None;
            }
            // ECHO on, so a `TIOCSTI` byte is echoed back to the master (the judge).
            let mut term: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(slave, &mut term) == 0 {
                term.c_lflag |= libc::ECHO;
                let _ = libc::tcsetattr(slave, libc::TCSANOW, &term);
            }
            Some(Self {
                master,
                slave,
                slave_path,
            })
        }
    }
}

#[cfg(unix)]
impl Drop for OutsidePty {
    fn drop(&mut self) {
        // SAFETY: both fds were returned by libc open/posix_openpt and are closed once.
        unsafe {
            libc::close(self.slave);
            libc::close(self.master);
        }
    }
}

fn run_outcome(mut command: SandboxCommand) -> Outcome {
    match command.output() {
        Ok(output) => outcome_from(output),
        Err(e) => Outcome {
            succeeded: false,
            detail: format!("spawn failed: {e}"),
        },
    }
}

fn outcome_from(output: std::process::Output) -> Outcome {
    let mut detail = String::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.trim().is_empty() {
        detail.push_str(stdout.trim());
    }
    if !stderr.trim().is_empty() {
        if !detail.is_empty() {
            detail.push_str(" | ");
        }
        detail.push_str(stderr.trim());
    }
    Outcome {
        succeeded: output.status.success(),
        detail: truncate(&detail, 200),
    }
}

/// Drives the minimal MCP server over stdio: initialize, tools/list, tools/call echo.
fn run_mcp(sandbox: &Sandbox, script: &Path) -> Outcome {
    let mut command = match sandbox.command("python3", [OsStr::new("-B"), script.as_os_str()]) {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                succeeded: false,
                detail: format!("sandbox error: {e}"),
            };
        }
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Outcome {
                succeeded: false,
                detail: format!("spawn failed: {e}"),
            };
        }
    };
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("piped stdout"));
    let mut problems = Vec::new();

    let mut call = |id: u64, method: &str, params: Value| -> Option<Value> {
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if writeln!(stdin, "{message}").is_err() {
            return None;
        }
        let _ = stdin.flush();
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
            if let Ok(value) = serde_json::from_str::<Value>(&line)
                && value.get("id").is_some()
            {
                return Some(value);
            }
        }
    };

    let init = call(
        1,
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "sandbox-harness", "version": "0"}}),
    );
    match init
        .as_ref()
        .and_then(|v| v.pointer("/result/serverInfo/name")?.as_str())
    {
        Some(name) => problems.push(format!("server={name}")),
        None => problems.push("initialize had no serverInfo".into()),
    }
    match call(2, "tools/list", json!({}))
        .and_then(|v| v.pointer("/result/tools")?.as_array().map(|a| a.len()))
    {
        Some(n) if n >= 1 => problems.push(format!("tools={n}")),
        _ => problems.push("tools/list empty".into()),
    }
    match call(3, "tools/call", json!({"name": "echo", "arguments": {}})).and_then(|v| {
        v.pointer("/result/content/0/text")?
            .as_str()
            .map(str::to_owned)
    }) {
        Some(text) if text == "hello-mcp" => problems.push("echo=hello-mcp".into()),
        other => problems.push(format!("echo mismatch: {other:?}")),
    }

    drop(stdin);
    let status = child.wait();
    let ok = status.map(|s| s.success()).unwrap_or(false)
        && !problems
            .iter()
            .any(|p| p.contains("no serverInfo") || p.contains("empty") || p.contains("mismatch"));
    Outcome {
        succeeded: ok,
        detail: truncate(&problems.join(" "), 200),
    }
}

// ---------- scratch setup ----------

fn new_root() -> Result<PathBuf, SandboxError> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let root =
        std::env::temp_dir().join(format!("radian-sandbox-escape-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for sub in ["ws", "outside", "home"] {
        std::fs::create_dir_all(root.join(sub))?;
    }
    // A git repository inside the workspace...
    let repo = root.join("ws/proj");
    std::fs::create_dir_all(&repo)?;
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "sandbox@example.com"]);
    git(&repo, &["config", "user.name", "sandbox"]);
    std::fs::write(repo.join("readme.md"), "hello\n")?;
    std::fs::write(repo.join("new.txt"), "new\n")?;
    git(&repo, &["add", "readme.md"]);
    git(&repo, &["commit", "-qm", "init"]);
    // ...and a worktree layout: the shared git dir lives outside the workspace.
    let main = root.join("main");
    std::fs::create_dir_all(&main)?;
    git(&main, &["init", "-q"]);
    git(&main, &["config", "user.email", "sandbox@example.com"]);
    git(&main, &["config", "user.name", "sandbox"]);
    std::fs::write(main.join("a.txt"), "a\n")?;
    git(&main, &["add", "a.txt"]);
    git(&main, &["commit", "-qm", "init"]);
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            root.join("ws/wt").to_str().unwrap(),
            "-b",
            "feature",
        ],
    );
    Ok(root)
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git is required for the sandbox harness");
    assert!(status.success(), "git {args:?} failed in {dir:?}");
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Picks an environment variable that exists in this process but is not on the policy's
/// whitelist, for the "unlisted variable is hidden" case. Returns `("<unset>", "")` when
/// there is none (the case then cannot distinguish, and its control fails loudly).
fn pick_unwhitelisted_env(policy: &SandboxPolicy) -> (String, String) {
    let mut vars: Vec<(String, String)> = std::env::vars()
        .filter(|(k, v)| {
            !policy.allow_env.contains(k)
                && !k.starts_with('=')
                && !v.is_empty()
                && !v.contains(['\'', '"', '$', '`', '\n'])
        })
        .collect();
    vars.sort();
    vars.into_iter()
        .next()
        .unwrap_or_else(|| ("RADIAN_UNSET_VAR".into(), String::new()))
}

fn orphan_report() -> String {
    let mut command = Command::new("ps");
    #[cfg(target_os = "macos")]
    command.args(["-axo", "pid,ppid,comm,args"]);
    #[cfg(not(target_os = "macos"))]
    command.args(["-eo", "pid,ppid,comm,args"]);
    match command.output() {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            let hits: Vec<&str> = text
                .lines()
                // The driver's own argv contains its name and the launching shell; only
                // children that could outlive the run matter.
                .filter(|l| {
                    l.contains("mcp_echo.py")
                        || l.contains("sandbox-exec -p")
                        || l.contains(" bwrap ")
                })
                .collect();
            if hits.is_empty() {
                "(no orphan processes)".into()
            } else {
                hits.join("\n")
            }
        }
        Err(e) => format!("(ps failed: {e})"),
    }
}

fn q(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn truncate(text: &str, n: usize) -> String {
    if text.chars().count() <= n {
        text.to_string()
    } else {
        let head: String = text.chars().take(n).collect();
        format!("{head}…")
    }
}

/// A minimal stdio MCP server (initialize / tools/list / tools/call echo). Python is used so
/// the same server runs on macOS and in the Linux VM without a compiled fixture.
const MCP_ECHO_PY: &str = r#"
import json, sys
def send(o):
    sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method"); mid = msg.get("id")
    if mid is None:
        continue
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2025-11-25","capabilities":{},"serverInfo":{"name":"sandbox-echo","version":"0"}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"echo","description":"echo","inputSchema":{"type":"object"}}]}})
    elif method == "tools/call" and msg.get("params", {}).get("name") == "echo":
        send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"hello-mcp"}]}})
    else:
        send({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"not found"}})
"#;

// ---------- startup overhead ----------

/// Timing samples for one process spawn, in milliseconds.
#[derive(Debug, Clone)]
pub struct Overhead {
    pub bare: Vec<f64>,
    pub sandboxed: Vec<f64>,
}

impl Overhead {
    pub fn median(v: &[f64]) -> f64 {
        if v.is_empty() {
            return 0.0;
        }
        let mut s = v.to_vec();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        s[s.len() / 2]
    }

    pub fn report(&self) -> String {
        format!(
            "bare median {:.2} ms; sandboxed median {:.2} ms; overhead {:.2} ms ({} samples)",
            Self::median(&self.bare),
            Self::median(&self.sandboxed),
            Self::median(&self.sandboxed) - Self::median(&self.bare),
            self.bare.len()
        )
    }
}

/// Times `true` with and without the sandbox (warmup 2, then `n` samples).
pub fn startup_overhead(n: usize) -> Result<Overhead, SandboxError> {
    let root = std::env::temp_dir().join(format!("radian-sandbox-bench-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&root);
    let ws = std::fs::canonicalize(&root)?;
    let sandbox = Sandbox::from_policy(SandboxPolicy::workspace_write(&ws)?)?;
    let true_bin = if cfg!(target_os = "macos") {
        "/usr/bin/true"
    } else {
        "/bin/true"
    };
    let mut bare = Vec::new();
    let mut sandboxed = Vec::new();
    for i in 0..(n + 2) {
        let start = std::time::Instant::now();
        let _ = Command::new(true_bin).output();
        if i >= 2 {
            bare.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        let start = std::time::Instant::now();
        if let Ok(mut command) = sandbox.command(true_bin, std::iter::empty::<&str>()) {
            let _ = command.output();
        }
        if i >= 2 {
            sandboxed.push(start.elapsed().as_secs_f64() * 1000.0);
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(Overhead { bare, sandboxed })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Unix-socket case whose sandbox connected (the old Linux shape: the socket was
    /// reachable, the control confirms the listener is sound). After slice 0 this is an
    /// escape on every backend: the seccomp filter must have denied the `socket` call.
    fn connected_unix_case(name: &str) -> Case {
        Case {
            group: "unix_socket",
            name: name.to_string(),
            expect: Expect::MustFail,
            sandbox: Outcome {
                succeeded: true,
                detail: "connected".into(),
            },
            control: Outcome {
                succeeded: true,
                detail: "connected".into(),
            },
            skip: None,
            note: String::new(),
        }
    }

    /// A Unix-socket denial case that behaved (the slice-0 shape: the sandbox was denied,
    /// the control connected).
    fn denied_unix_case(name: &str) -> Case {
        let mut case = connected_unix_case(name);
        case.sandbox = Outcome {
            succeeded: false,
            detail: "denied".into(),
        };
        case
    }

    /// Slice 0 closed the known limit: a denial case that behaved passes on every
    /// backend, and one whose sandbox connected fails on every backend (a FAIL is still
    /// possible, so an escape cannot hide behind a limit).
    #[test]
    fn the_unix_socket_denial_passes_and_an_escape_fails_on_every_backend() {
        let denied = denied_unix_case("workspace_denied");
        assert!(denied.passed());
        assert_eq!(denied.verdict(), Verdict::Pass);

        let escaped = connected_unix_case("workspace_denied");
        assert!(!escaped.passed());
        assert_eq!(escaped.verdict(), Verdict::Fail);
    }

    /// Sibling cases keep their verdicts: a passing sibling passes, and any other group
    /// still fails exactly as before.
    #[test]
    fn sibling_cases_keep_their_verdicts() {
        let mut other = connected_unix_case("whitelisted_allowed");
        other.expect = Expect::MustSucceed;
        assert!(other.passed());
        assert_eq!(other.verdict(), Verdict::Pass);

        let denied_write = Case {
            group: "escape_write",
            ..connected_unix_case("outside_absolute")
        };
        assert!(!denied_write.passed());
        assert_eq!(denied_write.verdict(), Verdict::Fail);
    }

    /// A matrix with only passing cases passes; one escape fails it. There is no
    /// backend-dependent exception left.
    #[test]
    fn the_matrix_has_no_backend_exception_left() {
        let matrix = Matrix {
            backend: "linux-bwrap".to_string(),
            probe: "test".to_string(),
            cases: vec![denied_unix_case("workspace_denied")],
            orphans: String::new(),
            root: std::env::temp_dir(),
        };
        assert!(matrix.passed());
        assert!(matrix.failures().is_empty());
        assert!(!matrix.render_text().contains("KNOWN-LIMIT"));

        let escaping = Matrix {
            backend: "linux-bwrap".to_string(),
            probe: "test".to_string(),
            cases: vec![connected_unix_case("workspace_denied")],
            orphans: String::new(),
            root: std::env::temp_dir(),
        };
        assert!(!escaping.passed());
        assert_eq!(escaping.failures().len(), 1);
    }
}

/// The verdict logic behind the `TIOCSTI` case's skip, checked without a pty or a kernel:
/// `tiocsti_skip_reason` is only ever fed the control's result, so a faked control exercises
/// exactly the branch the real case takes on this machine.
#[cfg(all(test, unix))]
mod tiocsti_verdict_tests {
    use super::*;

    fn outcome(succeeded: bool) -> Outcome {
        Outcome {
            succeeded,
            detail: String::new(),
        }
    }

    /// A `TIOCSTI` case is only skipped when the control could not inject; the skip reason
    /// always comes from the control's result.
    fn case(sandbox_injected: bool, control_injected: bool) -> Case {
        Case {
            group: "devices",
            name: "tiocsti_outside_terminal".to_owned(),
            expect: Expect::MustFail,
            sandbox: outcome(sandbox_injected),
            control: outcome(control_injected),
            skip: tiocsti_skip_reason(control_injected),
            note: String::new(),
        }
    }

    /// A skip never softens a case that could prove something: as long as the control injects,
    /// a sandbox that injects too is a real FAIL.
    #[test]
    fn a_case_with_a_working_control_still_fails_when_the_sandbox_injects() {
        let case = case(true, true);
        assert!(
            case.skip.is_none(),
            "a working control must not skip the case"
        );
        assert!(
            !case.passed(),
            "an escape with a working control must fail the matrix"
        );
    }

    /// The ordinary PASS: the control injects, the sandbox is denied.
    #[test]
    fn a_case_with_a_working_control_passes_when_the_sandbox_is_denied() {
        assert!(case(false, true).passed());
    }

    /// When the kernel refuses TIOCSTI for the control too, the case proves nothing: it is
    /// skipped, the reason names the kernel knob, and it is never counted as a failure.
    #[test]
    fn a_case_whose_control_is_denied_is_skipped_not_failed() {
        let case = case(false, false);
        assert!(
            case.skipped(),
            "a control that cannot inject must skip the case"
        );
        assert!(case.passed(), "a skipped case must not fail the matrix");
        assert!(
            case.skip
                .as_deref()
                .unwrap_or("")
                .contains("legacy_tiocsti"),
            "the skip reason names the kernel knob: {:?}",
            case.skip
        );
    }

    /// A skipped case reports SKIP (with its own reason): it never counts as a failure,
    /// and a case that is neither skipped nor passing still reports FAIL.
    #[test]
    fn a_skipped_case_reports_skip() {
        let skipped = case(false, false);
        assert_eq!(skipped.verdict(), Verdict::Skip);
        let matrix = Matrix {
            backend: "linux-bwrap".to_string(),
            probe: "test".to_string(),
            cases: vec![skipped],
            orphans: String::new(),
            root: std::env::temp_dir(),
        };
        assert!(matrix.passed());
        assert!(matrix.failures().is_empty());
        assert_eq!(matrix.skipped().len(), 1);
        let text = matrix.render_text();
        assert!(text.contains("SKIP"), "{text}");
        assert!(text.contains("legacy_tiocsti"), "{text}");
        let json = matrix.to_json();
        assert_eq!(json["cases"][0]["verdict"], serde_json::json!("SKIP"));
        assert!(
            json["cases"][0]["skip"]
                .as_str()
                .unwrap_or("")
                .contains("legacy_tiocsti"),
            "{json}"
        );
    }
}
