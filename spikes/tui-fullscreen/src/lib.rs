//! pty 驱动逻辑：全屏原型从不直接跑在谁的终端里，全部经 portable-pty，用 vt100
//! 屏幕模型断言渲染结果。`PtySession` 聚合 writer/字节日志/子进程/vt100 模型。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize};
use vt100::Parser;

pub fn app_bin() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/debug/tui-app");
    if p.exists() {
        return p;
    }
    // 单独在 spike 目录里构建时的位置。
    let alt = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/tui-app");
    assert!(
        alt.exists(),
        "找不到 tui-app（{} / {}）；先 cargo build --bins",
        p.display(),
        alt.display()
    );
    alt
}

/// master 句柄单独持有（PtySession 用完 writer 后 master 只剩 resize 用途）。
pub struct MasterHandle {
    master: Box<dyn portable_pty::MasterPty + Send>,
}

impl MasterHandle {
    pub fn resize(&self, rows: u16, cols: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("resize pty");
    }
}

pub struct PtySession {
    bytes: Arc<Mutex<Vec<u8>>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    processed: usize,
    parser: Parser,
    cols: u16,
    rows: u16,
}

/// 在 pty 里跑 `sh -c '"$@"; echo EXITED:$?; stty -a'`：app 退出后由同一 pty 里的
/// shell 报告退出码与 tty 状态（stty 输出也进字节日志，供 raw-mode 复原核对）。
pub fn spawn_app_with_master(
    mode: &str,
    extra: &[&str],
    rows: u16,
    cols: u16,
    stats: &Path,
) -> (PtySession, MasterHandle) {
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.arg("-c");
    cmd.arg(r#""$@"; echo "EXITED:$?"; stty -a"#);
    cmd.arg("sh");
    cmd.arg(app_bin());
    cmd.arg("--mode");
    cmd.arg(mode);
    cmd.arg("--stats");
    cmd.arg(stats);
    for a in extra {
        cmd.arg(a);
    }
    let child = pair.slave.spawn_command(cmd).expect("spawn");
    let writer = pair.master.take_writer().expect("writer");
    let reader = pair.master.try_clone_reader().expect("reader");
    let bytes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&bytes);
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => log.lock().unwrap().extend_from_slice(&buf[..n]),
            }
        }
    });
    let session = PtySession {
        bytes,
        writer,
        child,
        processed: 0,
        parser: Parser::new(rows, cols, 1000),
        cols,
        rows,
    };
    (
        session,
        MasterHandle {
            master: pair.master,
        },
    )
}

pub fn spawn_app(mode: &str, extra: &[&str], rows: u16, cols: u16, stats: &Path) -> PtySession {
    let (session, _master) = spawn_app_with_master(mode, extra, rows, cols, stats);
    session
}

impl PtySession {
    pub fn send(&mut self, data: &[u8]) {
        self.writer.write_all(data).expect("write to pty");
        self.writer.flush().expect("flush pty");
    }

    /// 有界等待屏幕上出现 `needle`；轮询间隔 10ms，读的是真实状态。
    pub fn wait_screen(&mut self, secs: f64, needle: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < deadline {
            if self.screen_contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// 把新字节喂进 vt100 屏幕模型。
    pub fn pump(&mut self) {
        let bytes = self.bytes.lock().unwrap();
        if bytes.len() > self.processed {
            let new = &bytes[self.processed..];
            self.parser.process(new);
            self.processed = bytes.len();
        }
    }

    pub fn screen_size(&mut self) -> (u16, u16) {
        self.pump();
        self.parser.screen().size()
    }

    /// 第 `row` 行的文本（把每个 cell 的 contents 拼起来）。
    pub fn row_text(&mut self, row: u16) -> String {
        self.pump();
        let screen = self.parser.screen();
        let mut s = String::new();
        for col in 0..self.cols {
            if let Some(cell) = screen.cell(row, col) {
                s.push_str(cell.contents());
            }
        }
        s
    }

    pub fn cell(&mut self, row: u16, col: u16) -> String {
        self.pump();
        self.parser
            .screen()
            .cell(row, col)
            .map(|c| c.contents().to_string())
            .unwrap_or_default()
    }

    /// 屏幕上是否出现了 `needle`（任一行）。
    pub fn screen_contains(&mut self, needle: &str) -> bool {
        self.pump();
        let screen = self.parser.screen();
        for row in 0..self.rows {
            let mut s = String::new();
            for col in 0..self.cols {
                if let Some(cell) = screen.cell(row, col) {
                    s.push_str(cell.contents());
                }
            }
            if s.contains(needle) {
                return true;
            }
        }
        false
    }

    pub fn byte_log(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }

    /// 等子进程退出，返回 (app 退出码, EXITED 起的字节)。
    pub fn finish(&mut self, secs: f64) -> (u32, Vec<u8>) {
        let deadline = Instant::now() + Duration::from_secs_f64(secs);
        loop {
            if let Ok(Some(_st)) = self.child.try_wait() {
                self.pump();
                let bytes = self.bytes.lock().unwrap().clone();
                // 子进程是外层 sh；app 的真实退出码在 EXITED:N 标记里。
                let code = find_sub(&bytes, b"EXITED:")
                    .map(|p| {
                        let rest = &bytes[p + b"EXITED:".len()..];
                        let line_end = rest
                            .iter()
                            .position(|b| *b == b'\n' || *b == b'\r')
                            .unwrap_or(rest.len().min(8));
                        String::from_utf8_lossy(&rest[..line_end])
                            .trim()
                            .parse::<u32>()
                            .unwrap_or(1)
                    })
                    .unwrap_or(1);
                let tail = match find_sub(&bytes, b"EXITED:") {
                    Some(p) => bytes[p..].to_vec(),
                    None => bytes,
                };
                return (code, tail);
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("app 在 {secs}s 内没有退出");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// pty resize 后，屏幕模型也要跟着改尺寸，否则对不上。
    pub fn set_size(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
        self.rows = rows;
        self.cols = cols;
    }
}

pub fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// 读取 stats JSON（app 退出时原子写出）。
pub fn read_stats(path: &Path) -> serde_json::Value {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("读 {}: {e}", path.display()));
    serde_json::from_str(&text).expect("stats json")
}

/// 恢复终端的硬证据：bracketed paste 关、备用屏退、光标显示；退出后 shell 的 stty
/// 显示 cooked（icanon 存在、-icanon 不存在）。
pub fn assert_restored(tail: &[u8], what: &str) {
    assert!(
        find_sub(tail, b"\x1b[?2004l").is_some(),
        "{what}: 未见 DisableBracketedPaste（\\x1b[?2004l）"
    );
    assert!(
        find_sub(tail, b"\x1b[?1049l").is_some(),
        "{what}: 未见 LeaveAlternateScreen（\\x1b[?1049l）"
    );
    assert!(
        find_sub(tail, b"\x1b[?25h").is_some(),
        "{what}: 未见光标恢复（\\x1b[?25h）"
    );
    let text = String::from_utf8_lossy(tail).to_string();
    let after = text.split("EXITED:").nth(1).unwrap_or_default();
    assert!(
        after.contains("icanon") && !after.contains("-icanon"),
        "{what}: stty 显示 raw 未复原（{after:?}）"
    );
}

// ---------- 各用例 ----------

/// 流式帧耗时。pty 后端经真实 pty 写入；test 后端是 TestBackend 纯内存测量。
pub fn case_stream(backend: &str, secs: u64, data_dir: &Path) -> serde_json::Value {
    let stats_path = data_dir.join(format!("stream-{backend}.json"));
    let _ = std::fs::remove_file(&stats_path);
    match backend {
        "test" => {
            let st = Command::new(app_bin())
                .args([
                    "--mode",
                    "stream",
                    "--backend",
                    "test",
                    "--duration-secs",
                    &secs.to_string(),
                    "--stats",
                    stats_path.to_str().unwrap(),
                ])
                .output()
                .expect("run test-backend stream");
            assert!(
                st.status.success(),
                "test-backend stream 失败: {}",
                String::from_utf8_lossy(&st.stderr)
            );
        }
        "pty" => {
            let mut s = spawn_app(
                "stream",
                &["--duration-secs", &secs.to_string()],
                40,
                120,
                &stats_path,
            );
            let (code, tail) = s.finish(secs as f64 + 30.0);
            assert_eq!(
                code,
                0,
                "pty stream 退出码 {code}: {}",
                String::from_utf8_lossy(&tail)
            );
        }
        _ => panic!("backend {backend:?}"),
    }
    read_stats(&stats_path)
}

/// 5000 字节多行 bracketed paste：字节不丢（校验和一致）、不卡（限时内显示总数）。
pub fn case_paste(data_dir: &Path) -> (serde_json::Value, f64) {
    let stats_path = data_dir.join("paste.json");
    let _ = std::fs::remove_file(&stats_path);
    let mut s = spawn_app("paste", &["--duration-secs", "60"], 40, 120, &stats_path);
    assert!(s.wait_screen(10.0, "paste-waiting"), "app 未出现初始屏");

    // 5000 字节，含换行（多行 paste）。
    let mut payload = String::new();
    let mut sum = 0u64;
    let mut i = 0u8;
    while payload.len() < 5000 {
        let chunk = format!("line {:03} αβγ tail\n", i);
        let take = chunk.len().min(5000 - payload.len());
        for b in &chunk.as_bytes()[..take] {
            sum = sum.wrapping_add(*b as u64);
        }
        payload.push_str(&chunk[..take]);
        i = i.wrapping_add(1);
    }
    let sent = Instant::now();
    let mut seq = Vec::with_capacity(payload.len() + 32);
    seq.extend_from_slice(b"\x1b[200~");
    seq.extend_from_slice(payload.as_bytes());
    seq.extend_from_slice(b"\x1b[201~");
    s.send(&seq);

    let target = format!("paste:{}:{}:1", payload.len(), sum);
    let ok = s.wait_screen(5.0, &target);
    let latency_ms = sent.elapsed().as_secs_f64() * 1000.0;
    assert!(ok, "paste 后未在状态行看到 {target}（{latency_ms:.0}ms）");
    s.send(b"q");
    let (code, _) = s.finish(10.0);
    assert_eq!(code, 0, "paste 用例退出码 {code}");
    let stats = read_stats(&stats_path);
    assert_eq!(
        stats["paste_bytes"].as_u64().unwrap(),
        payload.len() as u64,
        "字节数不一致"
    );
    assert_eq!(stats["paste_sum"].as_u64().unwrap(), sum, "校验和不一致");
    (stats, latency_ms)
}

/// CJK/emoji 宽度：app 渲染样例，vt100 屏幕模型给出实际占位，与 unicode-width 的
/// 预期逐样例比较；返回记录文本。稳定类（CJK/emoji/全角/半角/谚文）不一致算失败，
/// 组合符单独记录（模型间差异如实写）。
pub fn case_width(data_dir: &Path) -> String {
    let stats_path = data_dir.join("width.json");
    let _ = std::fs::remove_file(&stats_path);
    let mut s = spawn_app("width", &["--duration-secs", "120"], 40, 120, &stats_path);
    assert!(s.wait_screen(10.0, "width-shown"), "width 屏未出现");

    let mut report =
        String::from("样例行 | 内容 | unicode-width 预期列数 | vt100 实际占用列数 | 判定\n");
    let mut stable_all_match = true;
    for row in 1..=7u16 {
        let text = s.row_text(row);
        let sample = text.trim_end();
        if sample.is_empty() {
            continue;
        }
        let Some(inner) = sample.split('|').nth(1) else {
            continue;
        };
        let expected = unicode_width::UnicodeWidthStr::width(inner);
        // 内容被渲染在两个 | 之间：实际占宽 = 两个 | 的列位置差 - 1。
        // vt100 里宽字符占两个 cell，这个差值就是双格的直接证据。
        let pipes = pipe_cols(&mut s, row);
        let (Some(first), Some(last)) = (pipes.first().copied(), pipes.last().copied()) else {
            continue;
        };
        let rendered = (last - first - 1) as usize;
        let is_combining = sample.starts_with("S7");
        let m = expected == rendered;
        if !is_combining && !m {
            stable_all_match = false;
        }
        report.push_str(&format!(
            "{} | {} | {} | {} | {}\n",
            sample.split(':').next().unwrap_or("?"),
            inner,
            expected,
            rendered,
            if is_combining {
                if m {
                    "一致（组合符）"
                } else {
                    "差异（组合符，单独记录）"
                }
            } else if m {
                "一致"
            } else {
                "不一致"
            }
        ));
    }
    s.send(b"q");
    let (code, _) = s.finish(10.0);
    assert_eq!(code, 0);
    assert!(stable_all_match, "稳定类宽度有不一致：\n{report}");
    report
}

/// 该行所有内容为 `|` 的列号（升序）。
fn pipe_cols(s: &mut PtySession, row: u16) -> Vec<u16> {
    (0..120u16).filter(|col| s.cell(row, *col) == "|").collect()
}

/// resize 无残影：120x40 → 80x24 → 120x40，每次等重画后核对边框四角与内部填充。
pub fn case_resize() -> bool {
    let dir = std::env::temp_dir().join(format!("radian-tui-resize-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create resize dir");
    let stats_path = dir.join("stats.json");
    let (mut s, master) =
        spawn_app_with_master("resize", &["--duration-secs", "120"], 40, 120, &stats_path);
    assert!(s.wait_screen(10.0, "size:120x40"), "初始 size 未出现");

    let check = |s: &mut PtySession, cols: u16, rows: u16| -> bool {
        let tl = s.cell(0, 0);
        let tr = s.cell(0, cols - 1);
        let bl = s.cell(rows - 2, 0);
        let br = s.cell(rows - 2, cols - 1);
        let inner = s.cell(2, 2);
        [tl == "┌", tr == "┐", bl == "└", br == "┘", inner == "X"]
            .iter()
            .all(|b| *b)
    };
    assert!(check(&mut s, 120, 40), "初始 120x40 边框不完整");

    master.resize(24, 80);
    s.set_size(24, 80);
    assert!(s.wait_screen(10.0, "size:80x24"), "resize 后 size 未更新");
    assert!(check(&mut s, 80, 24), "80x24 边框/填充不完整（残影？）");
    assert_eq!(s.screen_size(), (24, 80));

    master.resize(40, 120);
    s.set_size(40, 120);
    assert!(
        s.wait_screen(10.0, "size:120x40"),
        "resize 回 120x40 未更新"
    );
    assert!(check(&mut s, 120, 40), "回到 120x40 边框/填充不完整");

    s.send(b"q");
    let (code, _) = s.finish(10.0);
    assert_eq!(code, 0);
    let _ = std::fs::remove_dir_all(&dir);
    true
}

/// 终端状态恢复：正常退出 / Ctrl-C / panic 三条路径。
pub fn case_restore(kind: &str) {
    let dir =
        std::env::temp_dir().join(format!("radian-tui-restore-{kind}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create restore dir");
    let stats_path = dir.join("stats.json");
    let mut s = spawn_app("idle", &["--duration-secs", "120"], 40, 120, &stats_path);
    assert!(s.wait_screen(10.0, "idle"), "idle 屏未出现");
    let log_start = s.byte_log().len();
    match kind {
        "normal" => s.send(b"q"),
        "ctrlc" => s.send(b"\x03"),
        "panic" => s.send(b"p"),
        other => panic!("kind {other:?}"),
    }
    let (code, _) = s.finish(10.0);
    let expected_code = if kind == "panic" { 101 } else { 0 };
    assert_eq!(code, expected_code, "{kind} 退出码");
    let log = s.byte_log();
    assert_restored(&log[log_start.min(log.len())..], kind);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 空闲 CPU：跑 idle 模式 secs 秒，每秒采样 `ps -o %cpu=`，返回样本。
pub fn case_idle_cpu(secs: u64, data_dir: &Path) -> Vec<f64> {
    let stats_path = data_dir.join("idle.json");
    let _ = std::fs::remove_file(&stats_path);
    let mut s = spawn_app(
        "idle",
        &["--duration-secs", &secs.to_string()],
        40,
        120,
        &stats_path,
    );
    let pid = s.pid().expect("pid");
    let mut samples = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(1));
        if let Ok(out) = Command::new("/bin/ps")
            .args(["-o", "%cpu=", "-p", &pid.to_string()])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if let Ok(v) = text.parse::<f64>() {
                samples.push(v);
            }
        }
    }
    let (code, _) = s.finish(30.0);
    assert_eq!(code, 0, "idle 退出码 {code}");
    samples
}
