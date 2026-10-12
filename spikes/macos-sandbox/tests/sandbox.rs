//! 每个关注点一组断言：任何一条逃逸成功或正常用法失败都会让对应的测试失败。

use std::path::PathBuf;

use macos_sandbox_spike::{Expect, Rig, median, p95};

fn fixture_bin() -> PathBuf {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/mcp-fixture");
    assert!(
        p.exists(),
        "找不到 mcp-fixture（{}）；先在仓库根 cargo build -p agent-extensions --bin mcp-fixture",
        p.display()
    );
    p
}

fn rig() -> Rig {
    // 测试不往仓库写数据；data 目录指向临时目录。
    let data = std::env::temp_dir().join(format!("radian-spike-data-{}", std::process::id()));
    Rig::new(
        &data,
        fixture_bin(),
        PathBuf::from(env!("CARGO_BIN_EXE_macos-sandbox-spike")),
    )
}

fn assert_group(rig: &mut Rig, group: &str) {
    let cases: Vec<_> = rig
        .run_all()
        .into_iter()
        .filter(|c| c.group == group)
        .collect();
    assert!(!cases.is_empty(), "组 {group} 没有产生用例");
    for c in &cases {
        assert!(
            c.passed,
            "[{group}] {} 未达预期（{:?}）：{}",
            c.name, c.expected, c.detail
        );
    }
}

#[test]
fn normal_use_still_works() {
    let mut rig = rig();
    assert_group(&mut rig, "normal_use");
}

#[test]
fn write_escapes_are_denied() {
    let mut rig = rig();
    assert_group(&mut rig, "escape_writes");
}

#[test]
fn git_dir_is_read_only() {
    let mut rig = rig();
    assert_group(&mut rig, "git_dir_readonly");
}

#[test]
fn worktree_layout_escapes_are_denied() {
    let mut rig = rig();
    assert_group(&mut rig, "worktree_escapes");
}

#[test]
fn network_is_denied_when_off_and_allowed_when_on() {
    let mut rig = rig();
    assert_group(&mut rig, "network_off");
}

#[test]
fn environment_filtering_is_spawner_side() {
    let mut rig = rig();
    assert_group(&mut rig, "environment");
}

#[test]
fn mcp_stdio_works_inside_the_sandbox() {
    let mut rig = rig();
    assert_group(&mut rig, "mcp_stdio");
}

#[test]
fn every_reported_expectation_is_consistent() {
    // 汇总一致性：整份报告里不允许有失败例（和单组断言互为冗余，但能让 `cargo test`
    // 的失败信息直接给出第一处失败的名字）。
    let mut rig = rig();
    let cases = rig.run_all();
    let failed: Vec<_> = cases.iter().filter(|c| !c.passed).collect();
    assert!(
        failed.is_empty(),
        "失败用例：{:?}",
        failed
            .iter()
            .map(|c| format!("[{}] {}", c.group, c.name))
            .collect::<Vec<_>>()
    );
    // 期望值覆盖了两种语义，防止 expect_* 接错。
    assert!(cases.iter().any(|c| c.expected == Expect::MustBeDenied));
    assert!(cases.iter().any(|c| c.expected == Expect::MustSucceed));
}

#[test]
fn stats_helpers() {
    // p95 nearest-rank：20 个样本取第 19 个。
    let v: Vec<f64> = (1..=20).map(f64::from).collect();
    assert_eq!(median(&v), 10.5);
    assert_eq!(p95(&v), 19.0);
    assert_eq!(p95(&[1.0]), 1.0);
}
