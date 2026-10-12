//! 每条通过标准一个具名测试（§8 inline 行）。全部经 pty，绝不直接跑原型。

use std::path::PathBuf;

use tui_inline_spike as lib;

fn data_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "radian-inline-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).expect("test data dir");
    d
}

fn check(v: &[lib::Verdict], what: &str) {
    let failures: String = v
        .iter()
        .filter(|x| !x.passed)
        .map(|x| format!("FAIL | {} | {}\n", x.name, x.detail))
        .collect();
    assert!(failures.is_empty(), "{what} 未通过：\n{failures}");
}

#[test]
fn streaming_coexists_with_scrollback() {
    check(
        &lib::case_scrollback(&data_dir()),
        "标准 1：流式与 scrollback 共存",
    );
}

#[test]
fn resize_keeps_inline_layout_correct() {
    check(&lib::case_resize(&data_dir()), "标准 2：resize 后重排正确");
}

#[test]
fn native_selection_preconditions_hold() {
    check(&lib::case_selection(&data_dir()), "标准 3：选择/复制前提");
}

#[test]
fn shell_prompt_position_correct_on_normal_exit() {
    check(&lib::case_prompt("normal"), "标准 4（normal）：提示符位置");
}

#[test]
fn shell_prompt_position_correct_on_ctrl_c() {
    check(&lib::case_prompt("ctrlc"), "标准 4（ctrl-c）：提示符位置");
}

#[test]
fn shell_prompt_position_correct_on_panic() {
    check(&lib::case_prompt("panic"), "标准 4（panic）：提示符位置");
}

#[test]
fn inline_height_clamping_is_recorded() {
    // 附加记录（非门槛）：请求高度 50、终端 40 行 → 实际 viewport 高度被钳制。
    let stats = lib::case_clamp(&data_dir(), 50, 40, 120, "h50-term40-test");
    let observed_h = stats["observed_viewport"][1].as_u64().unwrap();
    assert!(
        observed_h <= 40,
        "viewport 高度 {observed_h} 超过终端 40 行"
    );
    assert_eq!(stats["lines_pushed"].as_u64().unwrap(), 6, "流式行未推完");
}
