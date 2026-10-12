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

use serde_json::{Value, json};

use crate::policy::{SandboxMode, SandboxPolicy};
use crate::{Sandbox, SandboxError};

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
    pub note: String,
}

impl Case {
    /// A case passes only when the sandbox behaved as expected **and** the control confirms
    /// the command itself is sound: an escape must succeed without the sandbox; ordinary use
    /// must succeed both ways.
    pub fn passed(&self) -> bool {
        match self.expect {
            Expect::MustFail => !self.sandbox.succeeded && self.control.succeeded,
            Expect::MustSucceed => self.sandbox.succeeded && self.control.succeeded,
        }
    }
}

pub struct Matrix {
    pub backend: String,
    pub probe: String,
    pub cases: Vec<Case>,
    pub orphans: String,
    pub root: PathBuf,
}

impl Matrix {
    pub fn passed(&self) -> bool {
        self.cases.iter().all(Case::passed)
    }

    pub fn failures(&self) -> Vec<&Case> {
        self.cases.iter().filter(|c| !c.passed()).collect()
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
                if c.passed() { "PASS" } else { "FAIL" },
            ));
        }
        out.push_str(&format!(
            "\n{} cases, {} failed\n\norphan check:\n{}\n",
            self.cases.len(),
            self.failures().len(),
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
    worktree_name: String,
    cases: Vec<Case>,
}

/// Runs the whole matrix under the platform backend. Fails if the backend is unavailable or
/// its probe fails — the enforced half never runs unsandboxed.
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
    // A symlink from inside the workspace to a sibling outside it.
    symlink(&outside, &ws.join("leak"))?;
    // A symlink from inside the worktree to the shared git directory.
    symlink(&main.join(".git"), &wt.join("leakgit"))?;

    let policy = SandboxPolicy::workspace_write(&ws)?.deny_read(home.join(".ssh"));
    let sandbox = Sandbox::from_policy(policy.clone())?;
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
        worktree_name,
        cases: Vec::new(),
    };
    rig.escape_writes(&sandbox, &control);
    rig.symlink_escapes(&sandbox, &control);
    rig.network(&sandbox, &control);
    rig.environment(&sandbox, &control, &secret_name, &secret_value);
    rig.nested_shell(&sandbox, &control);
    rig.normal_use(&sandbox, &control);
    rig.git_metadata(&sandbox, &control);
    rig.sensitive_read(&sandbox, &control);
    rig.mcp_stdio(&sandbox, &control);

    let orphans = orphan_report();
    let matrix = Matrix {
        backend: sandbox.backend_name().to_string(),
        probe: sandbox.probe().detail.clone(),
        cases: rig.cases,
        orphans,
        root,
    };
    if matrix.passed() {
        let _ = std::fs::remove_dir_all(&matrix.root);
    }
    Ok(matrix)
}

impl Rig {
    fn record(
        &mut self,
        group: &'static str,
        name: &str,
        expect: Expect,
        sandbox: Outcome,
        control: Outcome,
        note: &str,
    ) {
        self.cases.push(Case {
            group,
            name: name.to_string(),
            expect,
            sandbox,
            control,
            note: note.to_string(),
        });
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

    fn network(&mut self, sandbox: &Sandbox, control: &Sandbox) {
        let g = "network_off";
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local listener");
        let port = listener.local_addr().unwrap().port();
        let script = format!(
            "python3 -c 'import socket,sys; s=socket.create_connection((\"127.0.0.1\",{port}),2); s.close(); sys.exit(0)'"
        );
        let sb = run_shell(sandbox, &script);
        // Only the control reaches the listener; accept exactly that one connection.
        let accept = std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let ct = run_shell(control, &script);
        let _ = accept.join();
        self.record(
            g,
            "connect_local_port",
            Expect::MustFail,
            sb,
            ct,
            "sandbox has no network; control connects to a listener this test owns",
        );
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

fn run_outcome(mut command: Command) -> Outcome {
    match command.output() {
        Ok(output) => {
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
        Err(e) => Outcome {
            succeeded: false,
            detail: format!("spawn failed: {e}"),
        },
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
