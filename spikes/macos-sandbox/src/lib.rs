//! macOS 沙箱可行性验证的驱动逻辑（Phase 0b spike 1）。
//!
//! 本 crate 只做验证，不是产品代码。所有"工作区"都是 `run`/测试在临时目录里新建的
//! scratch 目录，结束后删除；对工作区外的写尝试只指向同一 scratch 里的哨兵目录。
//! 网络尝试只连本进程自己在 127.0.0.1 上绑定的监听端口，不访问任何外部主机。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// 一次用例的期望与结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// 白名单内的正常用法，必须成功。
    MustSucceed,
    /// 逃逸尝试，必须被沙箱拒绝（非零退出）。
    MustBeDenied,
}

#[derive(Debug, Clone)]
pub struct CaseReport {
    pub group: &'static str,
    pub name: String,
    pub expected: Expect,
    pub passed: bool,
    pub detail: String,
}

impl CaseReport {
    fn ok(group: &'static str, name: &str, detail: String) -> Self {
        Self {
            group,
            name: name.into(),
            expected: Expect::MustSucceed,
            passed: true,
            detail,
        }
    }

    fn denied(group: &'static str, name: &str, detail: String) -> Self {
        Self {
            group,
            name: name.into(),
            expected: Expect::MustBeDenied,
            passed: true,
            detail,
        }
    }

    fn mismatch(group: &'static str, name: &str, expected: Expect, detail: String) -> Self {
        Self {
            group,
            name: name.into(),
            expected,
            passed: false,
            detail,
        }
    }
}

/// 一次验证运行的全部现场：scratch 目录、两个 git 布局、fixture 路径、数据输出目录。
pub struct Rig {
    pub scratch: PathBuf,
    /// 沙箱子进程的"工作区"（已 realpath，SBPL subpath 需要解析后的路径）。
    pub ws: PathBuf,
    /// 哨兵目录：所有"工作区外"的写尝试只落在这里。
    pub outside: PathBuf,
    /// ws/proj：普通 git 仓库（.git 在工作区内，被单独 deny）。
    pub repo: PathBuf,
    /// scratch/mainrepo + scratch/wt：git worktree 布局，共享 .git 在工作区外。
    pub mainrepo: PathBuf,
    pub wt: PathBuf,
    pub fixture: PathBuf,
    /// 探针二进制（bin 本体）。cargo test 里 current_exe 是测试 harness，不能当探针。
    pub probe_bin: PathBuf,
    pub cargo_bin: PathBuf,
    pub data_dir: PathBuf,
    profiles: Vec<PathBuf>,
}

fn real(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|e| panic!("canonicalize {}: {e}", p.display()))
}

fn run_out(cmd: &mut Command) -> (bool, String) {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", format_args!("{cmd:?}")));
    let mut text = String::new();
    text.push_str(&String::from_utf8_lossy(&out.stdout));
    if !out.stderr.is_empty() {
        text.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    (out.status.success(), text)
}

fn tail(text: &str, n: usize) -> String {
    let line = text.lines().last().unwrap_or("").trim();
    line.chars().take(n).collect()
}

impl Rig {
    /// 建 scratch 布局（临时目录 + 两个 git 仓库）。`fixture` 由调用方定位。
    pub fn new(data_dir: &Path, fixture: PathBuf, probe_bin: PathBuf) -> Self {
        assert!(
            Path::new(SANDBOX_EXEC).exists(),
            "{SANDBOX_EXEC} 不存在；本 spike 只支持 macOS"
        );
        // 同进程内的并行测试也要各自拥有独立 scratch。
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let scratch =
            std::env::temp_dir().join(format!("radian-spike-sb-{}-{}", std::process::id(), seq));
        std::fs::create_dir_all(&scratch).expect("create scratch");
        let ws = scratch.join("ws");
        let outside = scratch.join("outside");
        std::fs::create_dir_all(ws.join("proj")).expect("create ws");
        std::fs::create_dir_all(&outside).expect("create outside");

        // 普通仓库（在 ws 内，.git 被单独 deny）。
        let repo = ws.join("proj");
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "spike@radian.local"]);
        git(&repo, &["config", "user.name", "spike"]);
        std::fs::write(repo.join("readme.md"), "hello\n").unwrap();
        git(&repo, &["add", "readme.md"]);
        git(&repo, &["commit", "-qm", "init"]);

        // worktree 布局：mainrepo 的 .git 在“工作区”外，wt 是挂进工作区的树。
        let mainrepo = scratch.join("mainrepo");
        let wt = scratch.join("wt");
        std::fs::create_dir_all(&mainrepo).expect("create mainrepo");
        git(&mainrepo, &["init", "-q"]);
        git(&mainrepo, &["config", "user.email", "spike@radian.local"]);
        git(&mainrepo, &["config", "user.name", "spike"]);
        std::fs::write(mainrepo.join("a.txt"), "a\n").unwrap();
        git(&mainrepo, &["add", "a.txt"]);
        git(&mainrepo, &["commit", "-qm", "init"]);
        git(
            &mainrepo,
            &[
                "worktree",
                "add",
                "-q",
                &wt.display().to_string(),
                "-b",
                "feature",
            ],
        );

        let cargo_bin = home_cargo_bin();
        std::fs::create_dir_all(data_dir).expect("create data dir");
        Self {
            scratch: scratch.clone(),
            ws: real(&ws),
            outside: real(&outside),
            repo: real(&repo),
            mainrepo: real(&mainrepo),
            wt: real(&wt),
            fixture,
            probe_bin,
            cargo_bin,
            data_dir: data_dir.into(),
            profiles: Vec::new(),
        }
    }

    /// 生成一份 SBPL profile 写进 scratch，返回其路径。`ws` 必须是 realpath 后的。
    fn write_profile(&mut self, name: &str, ws: &Path, deny: &[String], net: bool) -> PathBuf {
        let deny_lines: String = deny
            .iter()
            .map(|d| format!("(deny file-write* {d})\n"))
            .collect();
        let net_line = if net { "(allow network*)\n" } else { "" };
        let ws = ws.display().to_string();
        let text = format!(
            "(version 1)\n\
             (deny default)\n\
             (allow file-read*)\n\
             (allow file-test-existence)\n\
             (allow process-exec*)\n\
             (allow process-fork)\n\
             (allow mach-lookup)\n\
             (allow sysctl-read)\n\
             {net_line}\
             (allow file-write-data (literal \"/dev/null\"))\n\
             (allow file-write* (subpath \"{ws}\"))\n\
             {deny_lines}"
        );
        let path = self.scratch.join(name);
        std::fs::write(&path, &text).expect("write profile");
        self.profiles.push(path.clone());
        path
    }

    fn profile_main(&mut self) -> PathBuf {
        let ws = self.ws.clone();
        let deny = format!("(subpath \"{}\")", ws.join("proj/.git").display());
        self.write_profile("profile-main.sb", &ws, &[deny], false)
    }

    fn profile_net_on(&mut self) -> PathBuf {
        let ws = self.ws.clone();
        let deny = format!("(subpath \"{}\")", ws.join("proj/.git").display());
        self.write_profile("profile-net-on.sb", &ws, &[deny], true)
    }

    fn profile_worktree(&mut self) -> PathBuf {
        let wt = self.wt.clone();
        let main_git = self.mainrepo.clone();
        self.write_profile(
            "profile-worktree.sb",
            &wt,
            &[
                // worktree 的 .git 是一个指向共享目录的指针文件，必须只读。
                format!("(literal \"{}\")", wt.join(".git").display()),
                // 共享 common git dir 整体只读。
                format!("(subpath \"{}\")", main_git.join(".git").display()),
            ],
            false,
        )
    }

    /// 把生成的 profile 文本复制进数据目录（证据）。
    pub fn persist_profiles(&self) {
        for name in [
            "profile-main.sb",
            "profile-net-on.sb",
            "profile-worktree.sb",
        ] {
            let src = self.scratch.join(name);
            if src.exists() {
                std::fs::copy(src, self.data_dir.join(name)).expect("persist profile");
            }
        }
    }

    /// 在沙箱里执行一条命令，继承当前环境。
    fn sbx(&self, profile: &Path, cmd: &[&str]) -> (bool, String) {
        assert!(!cmd.is_empty());
        let mut c = Command::new(SANDBOX_EXEC);
        c.arg("-f").arg(profile).arg("--").args(cmd);
        run_out(&mut c)
    }

    fn expect_success(
        &self,
        group: &'static str,
        name: &str,
        profile: &Path,
        cmd: &[&str],
    ) -> CaseReport {
        match self.sbx(profile, cmd) {
            (true, _) => CaseReport::ok(group, name, "退出码 0".into()),
            (false, text) => {
                CaseReport::mismatch(group, name, Expect::MustSucceed, tail(&text, 120))
            }
        }
    }

    fn expect_denied(
        &self,
        group: &'static str,
        name: &str,
        profile: &Path,
        cmd: &[&str],
    ) -> CaseReport {
        match self.sbx(profile, cmd) {
            (false, text) => CaseReport::denied(group, name, tail(&text, 120)),
            (true, _) => CaseReport::mismatch(
                group,
                name,
                Expect::MustBeDenied,
                "命令成功了，沙箱未拦截".into(),
            ),
        }
    }

    fn sh_write(path: &Path) -> Vec<String> {
        let script = format!("echo spike > {}", shell_quote(path));
        vec!["/bin/sh".into(), "-c".into(), script]
    }

    fn sh_c(script: &str) -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), script.into()]
    }

    fn strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    /// 全部用例。group 名同时是测试里断言的粒度。
    pub fn run_all(&mut self) -> Vec<CaseReport> {
        let mut cases = Vec::new();
        let main = self.profile_main();
        let worktree = self.profile_worktree();
        let ws = self.ws.clone();
        let outside = self.outside.clone();
        let repo = self.repo.clone();

        // ---- 正常用法必须仍然可用 ----
        let g = "normal_use";
        cases.push(self.expect_success(
            g,
            "ls 工作区",
            &main,
            &["/bin/ls", &ws.display().to_string()],
        ));
        cases.push(self.expect_success(
            g,
            "写工作区内文件",
            &main,
            &Self::strs(&Self::sh_write(&ws.join("a.txt"))),
        ));
        {
            let (_, text) = self.sbx(
                &main,
                &["/bin/cat", &ws.join("a.txt").display().to_string()],
            );
            let passed = text.trim() == "spike";
            let r = if passed {
                CaseReport::ok(g, "读工作区内文件", format!("cat 输出 {text:?}"))
            } else {
                CaseReport::mismatch(g, "读工作区内文件", Expect::MustSucceed, tail(&text, 80))
            };
            cases.push(r);
        }
        cases.push(self.expect_success(
            g,
            "git 只读 status（--no-optional-locks）",
            &main,
            &[
                "/usr/bin/git",
                "-C",
                &repo.display().to_string(),
                "--no-optional-locks",
                "status",
                "--short",
            ],
        ));
        cases.push(self.expect_success(
            g,
            "git log",
            &main,
            &[
                "/usr/bin/git",
                "-C",
                &repo.display().to_string(),
                "log",
                "--oneline",
            ],
        ));
        cases.push(self.expect_success(
            g,
            "cargo --version",
            &main,
            &[&self.cargo_bin.display().to_string(), "--version"],
        ));

        // ---- 逃逸：写工作区外，全部必须失败 ----
        let g = "escape_writes";
        cases.push(self.expect_denied(
            g,
            "绝对路径写工作区外",
            &main,
            &Self::strs(&Self::sh_write(&outside.join("x.txt"))),
        ));
        cases.push(self.expect_denied(
            g,
            "用 .. 绕出工作区",
            &main,
            &Self::strs(&Self::sh_c(&format!(
                "cd {} && echo spike > ../escape.txt",
                shell_quote(&ws)
            ))),
        ));
        // 工作区内符号链接指向工作区外，经链接写。
        let _ = std::fs::remove_file(ws.join("link-out"));
        std::os::unix::fs::symlink(&outside, ws.join("link-out")).expect("symlink");
        cases.push(self.expect_denied(
            g,
            "经符号链接写工作区外",
            &main,
            &Self::strs(&Self::sh_write(&ws.join("link-out/s.txt"))),
        ));
        // 子进程继承：沙箱里 sh -c 再套两层，仍不能写工作区外。
        let nested = format!(
            "/bin/sh -c \"/bin/sh -c 'echo spike > {}'\"",
            shell_quote(&outside.join("deep.txt"))
        );
        cases.push(self.expect_denied(
            g,
            "嵌套 sh 仍不能写工作区外",
            &main,
            &Self::strs(&Self::sh_c(&nested)),
        ));
        // 继承不破坏正常用法：嵌套 sh 写工作区内仍成功。
        let nested_ok = format!(
            "/bin/sh -c \"/bin/sh -c 'echo spike > {}'\"",
            shell_quote(&ws.join("nested.txt"))
        );
        cases.push(self.expect_success(
            g,
            "嵌套 sh 写工作区内",
            &main,
            &Self::strs(&Self::sh_c(&nested_ok)),
        ));

        // ---- .git 只读（普通仓库布局）----
        let g = "git_dir_readonly";
        cases.push(self.expect_denied(
            g,
            "写 .git/hooks",
            &main,
            &Self::strs(&Self::sh_write(&repo.join(".git/hooks/hook"))),
        ));
        cases.push(self.expect_denied(
            g,
            "写 .git/config",
            &main,
            &Self::strs(&Self::sh_write(&repo.join(".git/config"))),
        ));
        cases.push(self.expect_denied(
            g,
            "写 .git/info",
            &main,
            &Self::strs(&Self::sh_write(&repo.join(".git/info/x"))),
        ));
        // git config 设置 core.hooksPath（写 .git/config 的正门）。
        cases.push(self.expect_denied(
            g,
            "git config core.hooksPath",
            &main,
            &[
                "/usr/bin/git",
                "-C",
                &repo.display().to_string(),
                "config",
                "core.hooksPath",
                "/tmp/evil",
            ],
        ));
        cases.push(self.expect_denied(
            g,
            "git add（写 .git/index.lock）",
            &main,
            &[
                "/usr/bin/git",
                "-C",
                &repo.display().to_string(),
                "add",
                "readme.md",
            ],
        ));

        // ---- .git 只读（worktree 布局，共享目录在工作区外）----
        let g = "worktree_escapes";
        cases.push(self.expect_success(
            g,
            "worktree 内写文件",
            &worktree,
            &Self::strs(&Self::sh_write(&self.wt.join("n.txt"))),
        ));
        cases.push(self.expect_denied(
            g,
            "改 worktree 的 .git 指针文件",
            &worktree,
            &Self::strs(&Self::sh_write(&self.wt.join(".git"))),
        ));
        cases.push(self.expect_denied(
            g,
            "写共享 .git/hooks",
            &worktree,
            &Self::strs(&Self::sh_write(&self.mainrepo.join(".git/hooks/evil"))),
        ));
        cases.push(self.expect_denied(
            g,
            "写共享 .git/worktrees/<wt>/gitdir",
            &worktree,
            &Self::strs(&Self::sh_write(
                &self.mainrepo.join(".git/worktrees/wt/gitdir"),
            )),
        ));
        {
            let link = self.wt.join("g");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(self.mainrepo.join(".git"), &link).expect("symlink");
            cases.push(self.expect_denied(
                g,
                "经 worktree 内符号链接写共享 .git",
                &worktree,
                &Self::strs(&Self::sh_write(&link.join("evil3"))),
            ));
        }
        cases.push(self.expect_success(
            g,
            "worktree 内 git 只读 status",
            &worktree,
            &[
                "/usr/bin/git",
                "-C",
                &self.wt.display().to_string(),
                "--no-optional-locks",
                "status",
                "--short",
            ],
        ));

        // ---- 网络（关闭时）----
        let g = "network_off";
        let l1 = TcpListener::bind("127.0.0.1:0").expect("bind");
        let l2 = TcpListener::bind("127.0.0.1:0").expect("bind");
        let probe = self.probe_cmd();
        for (name, l) in [
            ("connect 自己的监听端口 A", &l1),
            ("connect 本机另一端口 B", &l2),
        ] {
            let addr = l.local_addr().unwrap().to_string();
            let cmd: Vec<String> = vec![probe.clone(), "probe".into(), "connect".into(), addr];
            cases.push(self.expect_denied(g, name, &main, &Self::strs(&cmd)));
        }
        // 对照：network* 放行后必须能连上（证明拦截来自沙箱而不是网络环境）。
        {
            let net_on = self.profile_net_on();
            let addr = l1.local_addr().unwrap().to_string();
            let cmd: Vec<String> = vec![probe.clone(), "probe".into(), "connect".into(), addr];
            cases.push(self.expect_success(
                "network_off",
                "对照：net-on profile 可连接",
                &net_on,
                &Self::strs(&cmd),
            ));
        }
        drop((l1, l2));

        // ---- 环境变量 ----
        let g = "environment";
        {
            // sandbox-exec 本身不过滤环境变量：继承时子进程能读到。
            let addr = String::new();
            let _ = addr;
            let out = Command::new(SANDBOX_EXEC)
                .arg("-f")
                .arg(&main)
                .arg("--")
                .arg(&probe)
                .arg("probe")
                .arg("env")
                .arg("SPIKE_SECRET")
                .env("SPIKE_SECRET", "topsecret")
                .output()
                .expect("spawn");
            let saw = String::from_utf8_lossy(&out.stdout).trim().to_string();
            cases.push(CaseReport {
                group: g,
                name: "sandbox-exec 不过滤环境变量（继承即可见）".into(),
                expected: Expect::MustSucceed,
                passed: saw == "topsecret",
                detail: format!("子进程读到 {saw:?}"),
            });
            // spawner 白名单：env_clear + 白名单后，未授权变量不可见。
            let cleared = Command::new(SANDBOX_EXEC)
                .arg("-f")
                .arg(&main)
                .arg("--")
                .arg(&probe)
                .arg("probe")
                .arg("env")
                .arg("SPIKE_SECRET")
                .env_clear()
                .env("SPIKE_ALLOWED", "yes")
                .output()
                .expect("spawn");
            let saw = String::from_utf8_lossy(&cleared.stdout).trim().to_string();
            cases.push(CaseReport {
                group: g,
                name: "spawner 白名单后未授权变量不可见".into(),
                expected: Expect::MustSucceed,
                passed: saw == "<unset>",
                detail: format!("子进程读到 {saw:?}"),
            });
            let allowed = Command::new(SANDBOX_EXEC)
                .arg("-f")
                .arg(&main)
                .arg("--")
                .arg(&probe)
                .arg("probe")
                .arg("env")
                .arg("SPIKE_ALLOWED")
                .env_clear()
                .env("SPIKE_ALLOWED", "yes")
                .output()
                .expect("spawn");
            let saw = String::from_utf8_lossy(&allowed.stdout).trim().to_string();
            cases.push(CaseReport {
                group: g,
                name: "白名单内变量可见".into(),
                expected: Expect::MustSucceed,
                passed: saw == "yes",
                detail: format!("子进程读到 {saw:?}"),
            });
        }

        // ---- MCP stdio 在沙箱里 ----
        cases.extend(self.mcp_cases());

        self.persist_profiles();
        cases
    }

    fn probe_cmd(&self) -> String {
        self.probe_bin.display().to_string()
    }

    /// fixture 以 --legacy 模式跑在沙箱里，宿主在沙箱外驱动 stdio。
    fn mcp_cases(&mut self) -> Vec<CaseReport> {
        let g = "mcp_stdio";
        let profile = self.profile_main();
        let log = self.ws.join("fixture.log");
        let mut child = Command::new(SANDBOX_EXEC)
            .arg("-f")
            .arg(&profile)
            .arg("--")
            .arg(&self.fixture)
            .arg("--legacy")
            .env_clear()
            .env("FIXTURE_LOG", &log)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sandboxed fixture");
        let mut stdin = child.stdin.take().expect("fixture stdin");
        let stdout = child.stdout.take().expect("fixture stdout");
        let mut reader = BufReader::new(stdout);

        let mut cases = Vec::new();
        fn send(stdin: &mut dyn Write, id: i64, method: &str, params: Value) {
            let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
            writeln!(stdin, "{msg}").expect("write to fixture");
            stdin.flush().expect("flush fixture");
        }
        let recv = |reader: &mut BufReader<std::process::ChildStdout>| -> Option<Value> {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return None,
                    Ok(_) => {
                        if let Ok(v) = serde_json::from_str::<Value>(&line)
                            && v.get("id").is_some()
                        {
                            return Some(v);
                        }
                    }
                }
            }
        };

        // initialize 往返。
        send(
            &mut stdin,
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "macos-sandbox-spike", "version": "0"}
            }),
        );
        let r = recv(&mut reader);
        let init_ok = r
            .as_ref()
            .and_then(|v| v.pointer("/result/serverInfo/name").and_then(Value::as_str))
            == Some("mcp-fixture");
        cases.push(CaseReport {
            group: g,
            name: "initialize 往返".into(),
            expected: Expect::MustSucceed,
            passed: init_ok,
            detail: match &r {
                Some(v) => format!(
                    "serverInfo.name = {:?}",
                    v.pointer("/result/serverInfo/name")
                ),
                None => "没有收到响应".into(),
            },
        });

        // 一次工具调用：echo 原样返回参数。
        send(
            &mut stdin,
            2,
            "tools/call",
            json!({"name": "echo", "arguments": {"hello": "sandbox"}}),
        );
        let r = recv(&mut reader);
        let echo_ok = r
            .as_ref()
            .map(|v| v.to_string().contains("sandbox"))
            .unwrap_or(false);
        cases.push(CaseReport {
            group: g,
            name: "tools/call echo".into(),
            expected: Expect::MustSucceed,
            passed: echo_ok,
            detail: match &r {
                Some(v) => tail(&v.to_string(), 120),
                None => "没有收到响应".into(),
            },
        });

        // 环境白名单对 MCP 子进程同样生效（env 工具读不到宿主的密钥）。
        send(
            &mut stdin,
            3,
            "tools/call",
            json!({"name": "env", "arguments": {"name": "SPIKE_SECRET"}}),
        );
        let r = recv(&mut reader);
        let env_hidden = r
            .as_ref()
            .and_then(|v| v.pointer("/result/content/0/text").and_then(Value::as_str))
            .is_some_and(|t| t == "<unset>");
        cases.push(CaseReport {
            group: g,
            name: "MCP 子进程的 env 白名单（宿主密钥不可见）".into(),
            expected: Expect::MustSucceed,
            passed: env_hidden,
            detail: match &r {
                Some(v) => format!("env 工具返回 {:?}", v.pointer("/result/content/0/text")),
                None => "没有收到响应".into(),
            },
        });

        drop(stdin);
        let _ = child.wait();
        cases
    }

    /// 放行矩阵：逐条移除一个 allow，看每类命令是否仍然工作。
    pub fn allow_matrix(&mut self) -> Value {
        const ALLOWS: [(&str, &str); 6] = [
            ("file-read*", "(allow file-read*)"),
            ("file-test-existence", "(allow file-test-existence)"),
            ("process-exec*", "(allow process-exec*)"),
            ("process-fork", "(allow process-fork)"),
            ("mach-lookup", "(allow mach-lookup)"),
            ("sysctl-read", "(allow sysctl-read)"),
        ];
        let ws = self.ws.display().to_string();
        let mut matrix = serde_json::Map::new();
        for (drop_name, _) in ALLOWS.iter().chain(std::iter::once(&("(none)", ""))) {
            let lines: Vec<&str> = ALLOWS
                .iter()
                .filter(|(n, _)| n != drop_name)
                .map(|(_, l)| *l)
                .collect();
            let profile = self
                .scratch
                .join(format!("matrix-{}.sb", drop_name.replace('*', "star")));
            std::fs::write(
                &profile,
                format!(
                    "(version 1)\n(deny default)\n{}\n(allow file-write-data (literal \"/dev/null\"))\n\
                     (allow file-write* (subpath \"{ws}\"))\n(deny file-write* (subpath \"{ws}/proj/.git\"))\n",
                    lines.join("\n")
                ),
            )
            .expect("write matrix profile");
            let repo = self.repo.display().to_string();
            let cargo = self.cargo_bin.display().to_string();
            let cat_target = self.ws.join("a.txt").display().to_string();
            let fixture = self.fixture.display().to_string();
            let checks: [(&str, Vec<&str>); 5] = [
                ("ls", vec!["/bin/ls", &ws]),
                ("cat", vec!["/bin/cat", &cat_target]),
                (
                    "git status",
                    vec![
                        "/usr/bin/git",
                        "-C",
                        &repo,
                        "--no-optional-locks",
                        "status",
                        "--short",
                    ],
                ),
                ("cargo --version", vec![&cargo, "--version"]),
                ("mcp initialize", vec![&fixture, "--legacy"]),
            ];
            let mut row = serde_json::Map::new();
            for (cmd_name, cmd) in checks {
                let ok = if cmd_name == "mcp initialize" {
                    self.mcp_init_ok(&profile)
                } else {
                    self.sbx(&profile, &cmd).0
                };
                row.insert(cmd_name.into(), json!(ok));
            }
            matrix.insert((*drop_name).into(), Value::Object(row));
        }
        Value::Object(matrix)
    }

    fn mcp_init_ok(&self, profile: &Path) -> bool {
        let mut child = Command::new(SANDBOX_EXEC)
            .arg("-f")
            .arg(profile)
            .arg("--")
            .arg(&self.fixture)
            .arg("--legacy")
            .env_clear()
            .env("FIXTURE_LOG", self.ws.join("matrix-mcp.log"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let mut stdin = child.stdin.take().expect("stdin");
        let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
        let msg = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},
            "clientInfo":{"name":"s","version":"0"}}});
        let _ = writeln!(stdin, "{msg}");
        let _ = stdin.flush();
        let mut line = String::new();
        let mut ok = false;
        while matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && v.get("id").is_some()
            {
                ok = v.get("result").is_some();
                break;
            }
            line.clear();
        }
        drop(stdin);
        let _ = child.wait();
        ok
    }

    /// 启动开销：有/无沙箱各跑 `n` 次同一命令，返回 (bare_ms, sandboxed_ms)。
    pub fn bench_startup(&mut self, n: usize) -> (Vec<f64>, Vec<f64>) {
        let profile = self.profile_main();
        let mut bare = Vec::new();
        let mut sandboxed = Vec::new();
        for i in 0..(n + 2) {
            let t = Instant::now();
            let _ = run_out(&mut Command::new("/usr/bin/true"));
            if i >= 2 {
                bare.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            let t = Instant::now();
            let _ = run_out(
                Command::new(SANDBOX_EXEC)
                    .arg("-f")
                    .arg(&profile)
                    .arg("--")
                    .arg("/usr/bin/true"),
            );
            if i >= 2 {
                sandboxed.push(t.elapsed().as_secs_f64() * 1000.0);
            }
        }
        (bare, sandboxed)
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        // fixture 起的 sleep 之类孙进程：scratch 里的布局不产生它们（没调用 crash_later/
        // spawn_grandchild），正常收尾只需删掉 scratch。
        let _ = std::fs::remove_dir_all(&self.scratch);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let full = ["-C", &dir.display().to_string()];
    let mut c = Command::new("/usr/bin/git");
    c.args(full).args(args);
    let (ok, text) = run_out(&mut c);
    assert!(ok, "git {args:?} in {}: {text}", dir.display());
}

fn home_cargo_bin() -> PathBuf {
    let home = std::env::var("HOME").expect("$HOME");
    let p = PathBuf::from(home).join(".cargo/bin/cargo");
    assert!(
        p.exists(),
        "本 spike 需要 {}（本机已确认存在）",
        p.display()
    );
    real(&p)
}

fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.display())
}

pub fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).expect("f64"));
    if s.len() % 2 == 1 {
        s[s.len() / 2]
    } else {
        (s[s.len() / 2 - 1] + s[s.len() / 2]) / 2.0
    }
}

/// nearest-rank p95。
pub fn p95(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).expect("f64"));
    let idx = (v.len() as f64 * 0.95).ceil() as usize - 1;
    s[idx.min(s.len() - 1)]
}

/// 结果表打印成文本（也写进数据目录）。
pub fn render_reports(cases: &[CaseReport]) -> String {
    let mut out = String::new();
    for c in cases {
        let want = match c.expected {
            Expect::MustSucceed => "应成功",
            Expect::MustBeDenied => "应被拒",
        };
        let verdict = if c.passed { "PASS" } else { "FAIL" };
        out.push_str(&format!(
            "{verdict:4} [{}] {}（{want}） {}\n",
            c.group, c.name, c.detail
        ));
    }
    let failed = cases.iter().filter(|c| !c.passed).count();
    out.push_str(&format!("\n共 {} 例，失败 {} 例\n", cases.len(), failed));
    out
}

/// `probe` 子命令：被放进沙箱里执行的小探针。成功退出 0。
pub fn probe(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("write") => {
            let Some(path) = args.get(1) else {
                eprintln!("usage: probe write <path>");
                return 2;
            };
            match std::fs::write(path, "spike\n") {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        Some("connect") => {
            let Some(addr) = args.get(1) else {
                eprintln!("usage: probe connect <addr>");
                return 2;
            };
            use std::net::TcpStream;
            match TcpStream::connect(addr) {
                Ok(_) => 0,
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        Some("env") => {
            let Some(var) = args.get(1) else {
                eprintln!("usage: probe env <VAR>");
                return 2;
            };
            println!(
                "{}",
                std::env::var(var).unwrap_or_else(|_| "<unset>".into())
            );
            0
        }
        _ => {
            eprintln!("usage: probe <write|connect|env> ...");
            2
        }
    }
}

/// 给 `cargo test` 用的时间上限保护（防止用例卡死而不是失败）。
pub fn with_timeout<T>(f: impl FnOnce() -> T + Send, secs: u64) -> T {
    // 单线程环境里没法真正取消；靠各用例自身都有超时/退出保证。这里只是兜底断言时长。
    let start = Instant::now();
    let out = f();
    assert!(
        start.elapsed() < Duration::from_secs(secs),
        "用例组耗时 {:?}，超过 {}s 上限",
        start.elapsed(),
        secs
    );
    out
}
