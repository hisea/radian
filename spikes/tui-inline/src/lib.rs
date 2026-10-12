//! pty 驱动（照抄 tui-fullscreen spike 的方式）：byte log + vt100 模型 + 标记等待。
//! inline 原型从不直接跑在谁的终端里。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize};
use vt100::Parser;

pub fn app_bin() -> PathBuf {
    // 集成测试用 CARGO_BIN_EXE；driver 用 target/debug。
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/tui-inline-app");
    if p.exists() {
        return p;
    }
    panic!(
        "找不到 tui-inline-app（{}）；先 cargo build --bins",
        p.display()
    );
}

pub fn app_bin_from_exe_env() -> PathBuf {
    if let Ok(p) = std::env::var("TUI_INLINE_APP") {
        return PathBuf::from(p);
    }
    app_bin()
}

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

pub struct Session {
    bytes: Arc<Mutex<Vec<u8>>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    processed: usize,
    parser: Parser,
    cols: u16,
    rows: u16,
    exited: Option<u32>,
}

/// 在 pty 里跑一段 shell：prelude（PRE 历史）→ app → EXITED:$? → MARKER-$$ → stty -a。
pub fn spawn(
    prelude_lines: usize,
    app_args: &[&str],
    rows: u16,
    cols: u16,
) -> (Session, MasterHandle) {
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
    // prelude 用 printf 保证每行格式固定；之后 "$@" 跑 app，再打标记与 stty。
    let script = format!(
        "i=1; while [ $i -le {n} ]; do printf 'PRE-%04d\\n' $i; i=$((i+1)); done; \
         \"$@\"; echo \"EXITED:$?\"; echo \"MARKER-$$\"; stty -a",
        n = prelude_lines
    );
    cmd.arg(script);
    cmd.arg("sh");
    cmd.arg(app_bin_from_exe_env());
    for a in app_args {
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
    let session = Session {
        bytes,
        writer,
        child,
        processed: 0,
        parser: Parser::new(rows, cols, 5000),
        cols,
        rows,
        exited: None,
    };
    (
        session,
        MasterHandle {
            master: pair.master,
        },
    )
}

impl Session {
    pub fn send(&mut self, data: &[u8]) {
        self.writer.write_all(data).expect("write to pty");
        self.writer.flush().expect("flush pty");
    }

    /// 有界等待字节流里出现 needle（标记等待，10ms 轮询读真实状态）。
    pub fn wait_bytes(&mut self, secs: f64, needle: &[u8]) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < deadline {
            self.pump();
            {
                let bytes = self.bytes.lock().unwrap();
                if find_sub(&bytes, needle).is_some() {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// 把新字节喂进 vt100 模型；遇到光标位置查询（`\x1b[6n`）时按真实终端的行为应答
    /// `\x1b[<row>;<col>R`（1 起数）。inline viewport 初始化依赖这个应答。
    pub fn pump(&mut self) {
        let bytes = self.bytes.lock().unwrap();
        if bytes.len() > self.processed {
            // 字段拆借：parser/writer 与 bytes 互不相干，可在持有锁的同时应答。
            let Session {
                bytes: _,
                writer,
                parser,
                processed,
                ..
            } = self;
            let new = &bytes[*processed..];
            let mut rest = new;
            loop {
                match find_sub(rest, b"\x1b[6n") {
                    Some(p) => {
                        parser.process(&rest[..p]);
                        let (r, c) = parser.screen().cursor_position();
                        let _ = writer.write_all(format!("\x1b[{};{}R", r + 1, c + 1).as_bytes());
                        let _ = writer.flush();
                        rest = &rest[p + 4..];
                    }
                    None => {
                        parser.process(rest);
                        break;
                    }
                }
            }
            *processed = bytes.len();
        }
    }

    pub fn set_size(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
        self.rows = rows;
        self.cols = cols;
    }

    /// 调试用：上滚到 scrollback 顶 / 回到可见区的内容。
    pub fn debug_contents(&mut self, scrollback_top: bool) -> String {
        self.pump();
        if scrollback_top {
            self.parser.screen_mut().set_scrollback(usize::MAX);
        } else {
            self.parser.screen_mut().set_scrollback(0);
        }
        self.parser.screen().contents()
    }

    pub fn byte_log(&self) -> Vec<u8> {
        self.bytes.lock().unwrap().clone()
    }

    /// 等子进程退出，返回 (app 退出码, EXITED 起的字节)。
    pub fn finish(&mut self, secs: f64) -> (u32, Vec<u8>) {
        let deadline = Instant::now() + Duration::from_secs_f64(secs);
        loop {
            self.pump();
            if let Ok(Some(_)) = self.child.try_wait() {
                self.exited = Some(0);
                break;
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("app 在 {secs}s 内没有退出");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // 等 EXITED 标记落盘（shell 在 app 之后还要跑两条命令）。
        let deadline = Instant::now() + Duration::from_secs_f64(secs);
        while Instant::now() < deadline {
            self.pump();
            let bytes = self.bytes.lock().unwrap().clone();
            if find_sub(&bytes, b"MARKER-").is_some() {
                self.pump();
                let bytes = self.bytes.lock().unwrap().clone();
                let code = find_sub(&bytes, b"EXITED:")
                    .map(|p| {
                        let rest = &bytes[p + b"EXITED:".len()..];
                        let end = rest
                            .iter()
                            .position(|b| *b == b'\n' || *b == b'\r')
                            .unwrap_or(rest.len().min(8));
                        String::from_utf8_lossy(&rest[..end])
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
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("shell 未在 {secs}s 内打出 MARKER");
    }
}

pub fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

pub fn count_occurrences(hay: &[u8], needle: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while let Some(p) = find_sub(&hay[i..], needle) {
        n += 1;
        i += p + needle.len();
    }
    n
}

// ---------- 各用例 ----------

pub struct Verdict {
    pub name: String,
    pub passed: bool,
    pub detail: String,
}

fn assert_no_forbidden(bytes: &[u8]) -> Result<(), String> {
    for bad in [
        &b"\x1b[?1049h"[..],
        b"\x1b[?1049l",
        b"\x1b[?1000h",
        b"\x1b[?1002h",
        b"\x1b[?1003h",
        b"\x1b[?1006h",
        b"\x1b[?1015h",
        b"\x1b[2J",
    ] {
        if find_sub(bytes, bad).is_some() {
            return Err(format!(
                "字节流含禁止序列 {:?}",
                String::from_utf8_lossy(bad)
            ));
        }
    }
    Ok(())
}

fn assert_paste_paired(bytes: &[u8]) -> Result<(), String> {
    let on = count_occurrences(bytes, b"\x1b[?2004h");
    let off = count_occurrences(bytes, b"\x1b[?2004l");
    let on_pos = find_sub(bytes, b"\x1b[?2004h");
    let off_pos = find_sub(bytes, b"\x1b[?2004l");
    match (on, off, on_pos, off_pos) {
        (1, 1, Some(a), Some(b)) if a < b => Ok(()),
        _ => Err(format!("bracketed paste 开关不精确成对：on={on} off={off}")),
    }
}

/// 标准 1：流式输出与已有 scrollback 共存。
/// 方法边界：字节流 = 终端收到并会放进 scrollback 的内容（模型级）；真实模拟器的
/// scrollback 保留行为未验证（见结论文档）。
pub fn case_scrollback(data_dir: &Path) -> Vec<Verdict> {
    let mut v = Vec::new();
    let stats_path = data_dir.join("stream-stats.json");
    let _ = std::fs::remove_file(&stats_path);
    let (mut s, _m) = spawn(
        50,
        &[
            "--mode",
            "stream",
            "--lines",
            "30",
            "--interval-ms",
            "30",
            "--duration-secs",
            "20",
            "--stats",
            stats_path.to_str().unwrap(),
        ],
        40,
        120,
    );
    // 最后一行是同步标记；看到后才让 app 正常退出，不靠固定等待。
    assert!(s.wait_bytes(15.0, b"LINE-0029"), "未收到最后一条流式行");
    s.send(b"q");
    let (code, _) = s.finish(30.0);
    let bytes = s.byte_log();
    let pre = count_occurrences(&bytes, b"PRE-");
    let line = count_occurrences(&bytes, b"LINE-");
    v.push(Verdict {
        name: "退出码".into(),
        passed: code == 0,
        detail: format!("exit={code}"),
    });
    v.push(Verdict {
        name: "PRE 历史行仍在字节流（50 行）".into(),
        passed: pre == 50,
        detail: format!("PRE- 出现 {pre} 次"),
    });
    v.push(Verdict {
        name: "流式行进入字节流（30 行）".into(),
        passed: line == 30,
        detail: format!("LINE- 出现 {line} 次"),
    });
    v.push(match assert_no_forbidden(&bytes) {
        Ok(()) => Verdict::ok("无备用屏/无鼠标捕获/无整屏清除"),
        Err(e) => Verdict::fail("无备用屏/无鼠标捕获/无整屏清除", e),
    });
    // vt100 模型只展示一个 40 行视口；上滚到顶部可验证最早 40 条 PRE，回到可见区
    // 可验证收尾状态。完整 50 条 PRE 和 30 条 LINE 的存在由上面的原始 pty 字节流计数
    // 验证（该字节流是终端接收、因而可进入 scrollback 的内容）。
    let top = s.debug_contents(true);
    let model_pre = top.contains("PRE-0001") && top.contains("PRE-0040");
    let visible = s.debug_contents(false);
    let model_done = visible.contains("pushed:30/30");
    let alt = s.parser.screen().alternate_screen();
    v.push(Verdict {
        name: "vt100 模型：scrollback 顶部含最早 PRE，可见区含收尾状态，未进备用屏".into(),
        passed: model_pre && model_done && !alt,
        detail: format!("pre_0001_to_0040={model_pre} done={model_done} alt_screen={alt}"),
    });
    v
}

/// 标准 2：resize 后重排正确（120×40 → 60×40 → 120×40，期间持续流式）。
pub fn case_resize(data_dir: &Path) -> Vec<Verdict> {
    let mut v = Vec::new();
    let stats_path = data_dir.join("resize-stats.json");
    let _ = std::fs::remove_file(&stats_path);
    let (mut s, master) = spawn(
        20,
        &[
            "--mode",
            "resize",
            "--lines",
            "60",
            "--interval-ms",
            "40",
            "--duration-secs",
            "60",
            "--stats",
            stats_path.to_str().unwrap(),
        ],
        40,
        120,
    );
    // 等初始宽度标记（status 显示的是 viewport 区域，宽度 = 终端宽度）。
    let first = s.wait_bytes(15.0, b"size:120x");
    master.resize(40, 60);
    s.set_size(40, 60);
    let mid = s.wait_bytes(15.0, b"size:60x");
    master.resize(40, 120);
    s.set_size(40, 120);
    let second = s.wait_bytes(15.0, b"size:120x");
    assert!(
        s.wait_bytes(15.0, b"LINE-0059"),
        "resize 期间未收到最后一条流式行"
    );
    s.send(b"q");
    let (code, _) = s.finish(30.0);
    let bytes = s.byte_log();
    // 两个 size:120x：第一个来自初始，第二个来自 resize 回来（位置在 size:60x 之后）。
    let pos60 = find_sub(&bytes, b"size:60x");
    let mut pos120 = None;
    if let Some(p60) = pos60 {
        pos120 = find_sub(&bytes[p60..], b"size:120x").map(|p| p + p60);
    }
    let line = count_occurrences(&bytes, b"LINE-");
    v.push(Verdict {
        name: "三个宽度阶段标记按序出现".into(),
        passed: first && mid && second && pos120.is_some(),
        detail: format!("120→60→120 标记：{first}/{mid}/{second}，回位位置 {pos120:?}"),
    });
    v.push(Verdict {
        name: "流式行完整（60 行）".into(),
        passed: line == 60,
        detail: format!("LINE- 出现 {line} 次"),
    });
    // resize 路径 ratatui 会发整屏清除（2J）再重画；此处只验证无 ?1049/鼠标捕获。
    // 已滚上去的历史行如何处理取决于终端模拟器，不能用 vt100 当作真实终端证据；
    // 该边界及本次 2J 观测在结论文档中如实记录。
    let mut no_alt_no_mouse = true;
    let mut why = String::new();
    for bad in [
        &b"\x1b[?1049h"[..],
        b"\x1b[?1049l",
        b"\x1b[?1000h",
        b"\x1b[?1002h",
        b"\x1b[?1003h",
        b"\x1b[?1006h",
        b"\x1b[?1015h",
    ] {
        if find_sub(&bytes, bad).is_some() {
            no_alt_no_mouse = false;
            why = format!("含 {:?}", String::from_utf8_lossy(bad));
        }
    }
    v.push(Verdict {
        name: "无 ?1049/鼠标捕获（resize 允许 2J，另行记录）".into(),
        passed: no_alt_no_mouse,
        detail: if no_alt_no_mouse { "-".into() } else { why },
    });
    let clear_on_resize = count_occurrences(&bytes, b"\x1b[2J");
    v.push(Verdict {
        name: "观测：resize 字节流含 ratatui 全屏清除（非门槛）".into(),
        // 这是记录事实，不把 vt100 的历史行模型结果伪装成真实终端结论。
        passed: true,
        detail: format!("CSI 2J 出现 {clear_on_resize} 次；旧行回流/保留需真实终端人工确认"),
    });
    v.push(Verdict {
        name: "退出码".into(),
        passed: code == 0,
        detail: format!("exit={code}"),
    });
    v
}

/// 标准 3：选择/复制前提（无鼠标捕获、无备用屏、开关成对、内容在 scrollback）。
pub fn case_selection(data_dir: &Path) -> Vec<Verdict> {
    let mut v = Vec::new();
    let stats_path = data_dir.join("selection-stats.json");
    let _ = std::fs::remove_file(&stats_path);
    let (mut s, _m) = spawn(
        10,
        &[
            "--mode",
            "stream",
            "--lines",
            "15",
            "--interval-ms",
            "30",
            "--duration-secs",
            "20",
            "--stats",
            stats_path.to_str().unwrap(),
        ],
        40,
        120,
    );
    assert!(s.wait_bytes(15.0, b"LINE-0014"), "未收到最后一条流式行");
    s.send(b"q");
    let (code, _) = s.finish(30.0);
    let bytes = s.byte_log();
    v.push(match assert_no_forbidden(&bytes) {
        Ok(()) => Verdict::ok("无 ?1049 / 无鼠标上报 / 无整屏清除"),
        Err(e) => Verdict::fail("无 ?1049 / 无鼠标上报 / 无整屏清除", e),
    });
    v.push(match assert_paste_paired(&bytes) {
        Ok(()) => Verdict::ok("bracketed paste 开关成对"),
        Err(e) => Verdict::fail("bracketed paste 开关成对", e),
    });
    s.pump();
    let bp = s.parser.screen().bracketed_paste();
    let contents = s.parser.screen().contents();
    v.push(Verdict {
        name: "结束后模型不在 bracketed paste 模式，内容可选中（在 scrollback）".into(),
        passed: !bp && contents.contains("LINE-0014"),
        detail: format!(
            "bracketed_paste={bp}, scrollback 含 LINE-0014={}",
            contents.contains("LINE-0014")
        ),
    });
    v.push(Verdict {
        name: "退出码".into(),
        passed: code == 0,
        detail: format!("exit={code}"),
    });
    v
}

/// 标准 4：结束后 shell 提示符位置正确（normal / ctrl-c / panic 三条路径）。
pub fn case_prompt(kind: &str) -> Vec<Verdict> {
    let mut v = Vec::new();
    // 不写仓库，且三个并行集成测试各用独立文件；/dev/null 不能用于原子 rename。
    let stats_path = std::env::temp_dir().join(format!(
        "tui-inline-prompt-{}-{}.json",
        std::process::id(),
        kind
    ));
    let _ = std::fs::remove_file(&stats_path);
    let (mut s, _m) = spawn(
        5,
        &[
            "--mode",
            "stream",
            "--lines",
            "8",
            "--interval-ms",
            "30",
            "--duration-secs",
            "120",
            "--stats",
            stats_path.to_str().expect("stats path utf-8"),
        ],
        40,
        120,
    );
    // 等 app 真的开始（viewport 画出来，字节流出现 size:）。
    assert!(
        s.wait_bytes(15.0, b"size:120x"),
        "app 未启动（没看到 viewport 状态行）"
    );
    let log_start = s.byte_log().len();
    match kind {
        "normal" => s.send(b"q"),
        "ctrlc" => s.send(b"\x03"),
        "panic" => s.send(b"p"),
        other => panic!("kind {other:?}"),
    }
    let expected = if kind == "panic" { 101 } else { 0 };
    let (code, tail) = s.finish(30.0);
    v.push(Verdict {
        name: "退出码".into(),
        passed: code == expected,
        detail: format!("exit={code}（期望 {expected}）"),
    });
    let relevant = &s.byte_log()[log_start.min(s.byte_log().len())..];
    v.push(match assert_no_forbidden(relevant) {
        Ok(()) => Verdict::ok("退出路径无 ?1049/鼠标/整屏清除"),
        Err(e) => Verdict::fail("退出路径无 ?1049/鼠标/整屏清除", e),
    });
    v.push(match assert_paste_paired(&s.byte_log()) {
        Ok(()) => Verdict::ok("bracketed paste 成对"),
        Err(e) => Verdict::fail("bracketed paste 成对", e),
    });
    // ratatui 每帧 hide（?25l），退出必须补 Show（?25h 在最后一个 ?25l 之后）。
    let log = s.byte_log();
    // 最后一个 ?25l 之后必须还有 ?25h（ratatui 每帧 hide，退出必须补 Show）。
    let mut p = 0usize;
    let mut last_l = None;
    while let Some(q) = find_sub(&log[p..], b"\x1b[?25l") {
        last_l = Some(p + q);
        p = p + q + b"\x1b[?25l".len();
    }
    let cursor_restored = match last_l {
        Some(l) => find_sub(&log[l..], b"\x1b[?25h").is_some(),
        None => true, // 从没藏过
    };
    v.push(Verdict {
        name: "光标显示已恢复（最后的 ?25l 之后有 ?25h）".into(),
        passed: cursor_restored,
        detail: format!("restored={cursor_restored}"),
    });
    // stty 显示 cooked（raw 已复原）。
    let tail_text = String::from_utf8_lossy(&tail).to_string();
    let after = tail_text.split("EXITED:").nth(1).unwrap_or_default();
    v.push(Verdict {
        name: "stty 显示 cooked".into(),
        passed: after.contains("icanon") && !after.contains("-icanon"),
        detail: format!("-icanon 出现：{}", after.contains("-icanon")),
    });
    // 屏幕模型：MARKER 完整、行首对齐、上方空行受限、光标列 0。
    s.pump();
    let screen = s.parser.screen();
    let mut marker_row = None;
    let mut marker_at_col0 = false;
    for r in 0..s.rows {
        let mut text = String::new();
        for c in 0..s.cols {
            if let Some(cell) = screen.cell(r, c) {
                text.push_str(cell.contents());
            }
        }
        if let Some(p) = text.find("MARKER-") {
            marker_row = Some(r);
            marker_at_col0 = text.trim_start().starts_with("MARKER-")
                && text.chars().take(p).all(|ch| ch == ' ');
            break;
        }
    }
    v.push(Verdict {
        name: "MARKER 完整且在新行行首".into(),
        passed: marker_row.is_some() && marker_at_col0,
        detail: format!("row={marker_row:?} at_col0={marker_at_col0}"),
    });
    let mut blanks_above = 0u16;
    if let Some(mr) = marker_row {
        let mut r = mr;
        while r > 0 {
            r -= 1;
            let mut text = String::new();
            for c in 0..s.cols {
                if let Some(cell) = screen.cell(r, c) {
                    text.push_str(cell.contents());
                }
            }
            if text.trim().is_empty() {
                blanks_above += 1;
            } else {
                break;
            }
        }
    }
    v.push(Verdict {
        name: "MARKER 上方无大片空行（≤2）".into(),
        passed: marker_row.is_some() && blanks_above <= 2,
        detail: format!("空行 {blanks_above}"),
    });
    let (crow, ccol) = screen.cursor_position();
    v.push(Verdict {
        name: "光标列 0".into(),
        passed: ccol == 0,
        detail: format!("cursor=({crow},{ccol})"),
    });
    v
}

/// 附加记录（非门槛）：inline 高度钳制与 insert_before 开销。
pub fn case_clamp(
    data_dir: &Path,
    height: u16,
    rows: u16,
    cols: u16,
    tag: &str,
) -> serde_json::Value {
    let stats_path = data_dir.join(format!("clamp-{tag}.json"));
    let _ = std::fs::remove_file(&stats_path);
    let (mut s, _m) = spawn(
        3,
        &[
            "--mode",
            "stream",
            "--lines",
            "6",
            "--interval-ms",
            "20",
            "--duration-secs",
            "30",
            "--height",
            &height.to_string(),
            "--stats",
            stats_path.to_str().unwrap(),
        ],
        rows,
        cols,
    );
    assert!(
        s.wait_bytes(15.0, b"LINE-0005"),
        "clamp 用例未收到最后一条流式行"
    );
    s.send(b"q");
    let (code, _) = s.finish(30.0);
    assert_eq!(code, 0, "clamp 用例退出码");
    let stats: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&stats_path).unwrap()).unwrap();
    stats
}

impl Verdict {
    pub fn ok(name: &str) -> Self {
        Self {
            name: name.into(),
            passed: true,
            detail: "-".into(),
        }
    }

    pub fn fail(name: &str, detail: String) -> Self {
        Self {
            name: name.into(),
            passed: false,
            detail,
        }
    }
}

pub fn render(v: &[Verdict]) -> String {
    v.iter()
        .map(|x| {
            format!(
                "{} | {} | {}\n",
                if x.passed { "PASS" } else { "FAIL" },
                x.name,
                x.detail
            )
        })
        .collect()
}
