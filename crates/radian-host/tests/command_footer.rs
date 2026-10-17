//! The out-of-process footer `command` item (ADR 0027): it runs a real
//! fixture script (never the user's config or the network), and its value travels only through
//! the contribution store.
//!
//! Every fixture is a `/bin/sh` script written into a fresh scratch directory. `exec` is used
//! where a script would otherwise leave a grandchild behind (the 5 s timeout test), so the
//! runner's `kill_on_drop` reaps the direct child.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use radian_host::{
    CommandContributor, CommandSpec, ContributionStore, ContributorRunner, FooterContext,
    FooterSnapshot, SegmentRole,
};

fn scratch(label: &str) -> PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "radian-command-footer-{}-{label}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn spec(id: &str, program: &Path, period_secs: Option<u64>) -> CommandSpec {
    CommandSpec {
        id: id.to_owned(),
        program: program.as_os_str().to_owned(),
        args: Vec::new(),
        period: period_secs.map(Duration::from_secs),
    }
}

fn contributor(
    id: &str,
    program: &Path,
    dir: &Path,
    context: Arc<FooterContext>,
) -> Arc<CommandContributor> {
    Arc::new(CommandContributor::new(
        spec(id, program, Some(60)),
        context,
        "sess-1",
        Some(dir.display().to_string()),
    ))
}

/// A command's first stdout line reaches the store through the ordinary runner, with no symbol
/// and no stale mark.
#[tokio::test]
async fn the_command_output_reaches_the_store_through_the_runner() {
    let dir = scratch("store");
    let script = write_script(&dir, "hi.sh", "printf hi");
    let store = ContributionStore::new();
    let mut runner = ContributorRunner::new(
        contributor("thing", &script, &dir, FooterContext::new()),
        Arc::clone(&store),
    );
    let tick = runner.tick().await;
    assert!(tick.changed, "the first value is a change");
    assert!(tick.notice.is_none());
    let held = store.get("thing").expect("the value");
    assert_eq!(held.segments[0].text, "hi");
    assert_eq!(held.segments[0].role, SegmentRole::Normal);
    assert_eq!(held.symbol, None);
    assert!(!held.stale);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The stdin is exactly the documented JSON context; every field is display-safe.
#[tokio::test]
async fn the_stdin_is_the_documented_json_context() {
    let dir = scratch("stdin");
    let out = dir.join("stdin.json");
    let script = write_script(&dir, "capture.sh", &format!("cat > \"{}\"", out.display()));
    let context = FooterContext::new();
    context.set(FooterSnapshot {
        model: Some("claude-new".to_owned()),
        cwd: Some("/work/project".to_owned()),
        branch: Some("main".to_owned()),
        context_percent: Some(41.3),
        tokens_input: Some(15_000_000),
        tokens_output: Some(2_600_000),
        tokens_cache_read: Some(1_962_000_000),
        cost_usd: Some(14.51),
        tasks_running: Some(1),
        tasks_total: Some(3),
    });
    let store = ContributionStore::new();
    let mut runner = ContributorRunner::new(
        contributor("thing", &script, &dir, context),
        Arc::clone(&store),
    );
    // `cat > file` prints nothing, so the run fails; the stdin file is written regardless.
    let _ = runner.tick().await;

    let text = std::fs::read_to_string(&out).expect("the stdin was captured");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("the stdin is JSON");
    assert_eq!(doc["model"], "claude-new");
    assert_eq!(doc["cwd"], "/work/project");
    assert_eq!(doc["branch"], "main");
    assert_eq!(doc["context_percent"], 41.3);
    assert_eq!(doc["tokens"]["input"], 15_000_000u64);
    assert_eq!(doc["tokens"]["output"], 2_600_000u64);
    assert_eq!(doc["tokens"]["cache_read"], 1_962_000_000u64);
    assert_eq!(doc["cost_usd"], 14.51);
    assert_eq!(doc["tasks"]["running"], 1);
    assert_eq!(doc["tasks"]["total"], 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The child's environment is cleared to `PATH` plus the deliberate `RADIAN_*` allow-list: no
/// host variable leaks in.
#[tokio::test]
async fn the_environment_is_cleared_to_the_deliberate_allow_list() {
    let dir = scratch("env");
    let out = dir.join("env.txt");
    let script = write_script(&dir, "env.sh", &format!("env > \"{}\"", out.display()));
    // A uniquely named probe in this process must not reach the child.
    // SAFETY: the name is unique to this test and single-threaded for its duration.
    unsafe { std::env::set_var("RADIAN_FOOTER_LEAK_PROBE", "leaked") };
    let store = ContributionStore::new();
    let mut runner = ContributorRunner::new(
        contributor("thing", &script, &dir, FooterContext::new()),
        Arc::clone(&store),
    );
    let _ = runner.tick().await;
    unsafe { std::env::remove_var("RADIAN_FOOTER_LEAK_PROBE") };

    let text = std::fs::read_to_string(&out).expect("the env was captured");
    assert!(
        !text.contains("RADIAN_FOOTER_LEAK_PROBE"),
        "a host variable leaked:\n{text}"
    );
    assert!(!text.contains("HOME="), "HOME must not leak:\n{text}");
    assert!(
        text.lines().any(|line| line.starts_with("PATH=")),
        "PATH is always inherited:\n{text}"
    );
    assert!(
        text.contains("RADIAN_SESSION_ID=sess-1"),
        "the session id:\n{text}"
    );
    assert!(
        text.contains(&format!("RADIAN_CWD={}", dir.display())),
        "the cwd:\n{text}"
    );
    // Every variable the shell itself adds (PWD/SHLVL/_) is fine; nothing from the host is.
    for line in text.lines().filter(|line| !line.is_empty()) {
        let name = line.split('=').next().unwrap_or("");
        assert!(
            [
                "PATH",
                "RADIAN_SESSION_ID",
                "RADIAN_CWD",
                "PWD",
                "SHLVL",
                "_"
            ]
            .contains(&name),
            "an unexpected variable reached the child: {name:?}\n{text}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Only the first line is kept, and it is truncated to 256 bytes.
#[tokio::test]
async fn the_first_line_is_taken_and_truncated_to_256_bytes() {
    let dir = scratch("lines");
    let script = write_script(&dir, "two.sh", "printf 'first\\nsecond\\n'");
    let store = ContributionStore::new();
    let mut runner = ContributorRunner::new(
        contributor("thing", &script, &dir, FooterContext::new()),
        Arc::clone(&store),
    );
    runner.tick().await;
    assert_eq!(store.get("thing").unwrap().segments[0].text, "first");

    let long = write_script(&dir, "long.sh", "printf '%0400d' 0 | tr '0' 'x'");
    let store = ContributionStore::new();
    let mut runner = ContributorRunner::new(
        contributor("thing", &long, &dir, FooterContext::new()),
        Arc::clone(&store),
    );
    runner.tick().await;
    let text = store.get("thing").unwrap().segments[0].text.clone();
    assert_eq!(text.len(), 256, "the hard byte cap: {}", text.len());
    assert!(text.chars().all(|ch| ch == 'x'), "no mojibake");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A JSON line sets the text's theme role and an optional symbol, carried through the store.
#[tokio::test]
async fn json_output_sets_the_role_and_symbol() {
    let dir = scratch("json");
    let script = write_script(
        &dir,
        "json.sh",
        "printf '%s' '{\"text\":\"hi\",\"style\":\"warn\",\"symbol\":\"!\"}'",
    );
    let store = ContributionStore::new();
    let mut runner = ContributorRunner::new(
        contributor("thing", &script, &dir, FooterContext::new()),
        Arc::clone(&store),
    );
    runner.tick().await;
    let held = store.get("thing").unwrap();
    assert_eq!(held.segments[0].text, "hi");
    assert_eq!(held.segments[0].role, SegmentRole::Warn);
    assert_eq!(held.symbol.as_deref(), Some("!"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A timeout keeps the last good value and marks it stale; the first failure is not a notice.
#[tokio::test]
async fn a_timeout_keeps_the_last_value_and_marks_it_stale() {
    let dir = scratch("timeout");
    let marker = dir.join("good");
    std::fs::write(&marker, "1").unwrap();
    let script = write_script(
        &dir,
        "slow.sh",
        &format!(
            "if [ -f \"{}\" ]; then printf hi; else exec sleep 30; fi",
            marker.display()
        ),
    );
    let store = ContributionStore::new();
    let mut runner = ContributorRunner::new(
        contributor("thing", &script, &dir, FooterContext::new()),
        Arc::clone(&store),
    );
    runner.tick().await;
    assert_eq!(store.get("thing").unwrap().segments[0].text, "hi");

    std::fs::remove_file(&marker).unwrap();
    let tick = runner.tick().await;
    assert!(tick.changed, "becoming stale is a redraw");
    assert!(tick.notice.is_none(), "one failure only dims the value");
    let held = store.get("thing").unwrap();
    assert_eq!(held.segments[0].text, "hi", "the last value is kept");
    assert!(held.stale);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A non-zero exit keeps the last good value and marks it stale, like a timeout.
#[tokio::test]
async fn a_nonzero_exit_keeps_the_last_value_and_marks_it_stale() {
    let dir = scratch("nonzero");
    let control = dir.join("mode");
    std::fs::write(&control, "good").unwrap();
    let script = write_script(
        &dir,
        "exit.sh",
        &format!(
            "mode=$(cat \"{}\" 2>/dev/null || echo good)\n\
             if [ \"$mode\" = good ]; then printf good; exit 0; fi\n\
             printf ignored; exit 3",
            control.display()
        ),
    );
    let store = ContributionStore::new();
    let mut runner = ContributorRunner::new(
        contributor("thing", &script, &dir, FooterContext::new()),
        Arc::clone(&store),
    );
    runner.tick().await;
    assert_eq!(store.get("thing").unwrap().segments[0].text, "good");

    std::fs::write(&control, "bad").unwrap();
    let tick = runner.tick().await;
    assert!(tick.changed);
    let held = store.get("thing").unwrap();
    assert_eq!(held.segments[0].text, "good", "the last value is kept");
    assert!(held.stale);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Invalid JSON and an unknown style keep the last good value and mark it stale.
#[tokio::test]
async fn invalid_json_and_unknown_styles_keep_the_last_value() {
    let dir = scratch("badjson");
    let control = dir.join("mode");
    std::fs::write(&control, "good").unwrap();
    let script = write_script(
        &dir,
        "mode.sh",
        &format!(
            "mode=$(cat \"{}\" 2>/dev/null || echo good)\n\
             case \"$mode\" in\n\
             good) printf 'ok' ;;\n\
             json) printf '{{not json}}' ;;\n\
             style) printf '%s' '{{\"text\":\"x\",\"style\":\"neon\"}}' ;;\n\
             esac",
            control.display()
        ),
    );
    let store = ContributionStore::new();
    let mut runner = ContributorRunner::new(
        contributor("thing", &script, &dir, FooterContext::new()),
        Arc::clone(&store),
    );
    runner.tick().await;
    assert_eq!(store.get("thing").unwrap().segments[0].text, "ok");

    for mode in ["json", "style"] {
        std::fs::write(&control, mode).unwrap();
        runner.tick().await;
        let held = store.get("thing").unwrap();
        assert_eq!(
            held.segments[0].text, "ok",
            "{mode}: the last value is kept"
        );
        assert!(held.stale, "{mode}: it is marked stale");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// ADR 0027: the command's output, stdin and stderr never enter the
/// journal, a notice, the session transcript, the model context or a herdr report. The runner
/// has **no** handle to any of them — it is given a [`ContributionStore`] and a
/// [`FooterContext`] only — so the value cannot travel anywhere else. This reads the runner's
/// source and forbids the imports a leak would need (a mutation that wrote the output to a
/// journal file, or reported it to herdr, is caught here).
#[test]
fn the_command_runner_has_no_journal_or_herdr_path() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/command_footer.rs"
    ))
    .expect("command_footer.rs is readable");
    // Only the non-test code, and without comments (the prose names the rule on purpose).
    let code = source
        .split_once("\n#[cfg(test)]")
        .map_or(source.as_str(), |(before, _)| before);
    let code: String = code
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !(trimmed.starts_with("//") || trimmed.starts_with("///") || trimmed.starts_with("//!"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        "std::fs",
        "radian_session",
        "journal",
        "Journal",
        "herdr",
        "transcript",
        "session_dir",
        "model context",
    ] {
        assert!(
            !code.contains(forbidden),
            "command_footer.rs must not reach {forbidden:?}: the value travels only through the \
             contribution store"
        );
    }
    // The positive control: the value's only path is the store.
    assert!(
        code.contains("Contribution"),
        "the value must travel through a Contribution"
    );
}

/// A failure notice is generic and never carries the command's output.
#[tokio::test]
async fn a_failure_notice_is_generic_and_never_carries_the_output() {
    let dir = scratch("notice");
    let script = write_script(&dir, "fail.sh", "printf SECRET-OUTPUT; exit 1");
    let store = ContributionStore::new();
    let mut runner = ContributorRunner::new(
        contributor("thing", &script, &dir, FooterContext::new()),
        Arc::clone(&store),
    );
    let mut notice = None;
    for _ in 0..radian_host::NOTICE_AFTER {
        notice = runner.tick().await.notice;
    }
    let notice = notice.expect("a run of failures yields one notice");
    assert!(notice.contains("footer command"), "{notice}");
    assert!(
        !notice.contains("SECRET-OUTPUT"),
        "the output leaked: {notice}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
