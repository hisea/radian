//! 每条通过标准一个测试；全部经 pty 或 TestBackend 驱动，从不把全屏原型跑在
//! 构建进程自己的终端上。短时长跑证据用例的同一路径（正式数据由 `tui-driver`
//! 以更长时长产出，落在 data/）。

use std::path::PathBuf;

use tui_fullscreen_spike as lib;

fn data_dir() -> PathBuf {
    // 测试不往仓库写数据；用临时目录。
    let d = std::env::temp_dir().join(format!(
        "radian-tui-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).expect("test data dir");
    d
}

#[test]
fn stream_pty_p95_under_16ms_at_200_tokens_per_sec() {
    let stats = lib::case_stream("pty", 6, &data_dir());
    let p95 = stats["frame_p95_ms"].as_f64().expect("p95");
    let tokens = stats["tokens"].as_u64().expect("tokens");
    let frames = stats["frames"].as_u64().expect("frames");
    // 6 秒 × 200/s，允许调度抖动 ±10%。
    assert!(tokens >= 6 * 180, "token 速率不足：{tokens}/6s");
    assert!(frames > 100, "帧数异常偏少：{frames}");
    assert!(p95 < 16.0, "p95 单帧耗时 {p95:.2}ms ≥ 16ms");
    let size = stats["terminal_size"].as_array().expect("terminal_size");
    assert_eq!(
        (size[0].as_u64(), size[1].as_u64()),
        (Some(120), Some(40)),
        "应在 120×40 终端下测量"
    );
}

#[test]
fn stream_test_backend_p95_under_16ms() {
    let stats = lib::case_stream("test", 4, &data_dir());
    let p95 = stats["frame_p95_ms"].as_f64().expect("p95");
    assert!(p95 < 16.0, "TestBackend p95 {p95:.2}ms ≥ 16ms");
    assert!(stats["frames"].as_u64().unwrap() > 50, "帧数异常偏少");
}

#[test]
fn bracketed_paste_5000_bytes_arrives_intact() {
    let (stats, latency_ms) = lib::case_paste(&data_dir());
    assert_eq!(stats["paste_bytes"].as_u64().unwrap(), 5000, "字节数丢失");
    assert_eq!(stats["paste_events"].as_u64().unwrap(), 1);
    assert!(
        latency_ms < 2000.0,
        "paste 处理耗时 {latency_ms:.0}ms（卡）"
    );
    let max_frame = stats["frame_max_ms"].as_f64().unwrap();
    assert!(max_frame < 100.0, "paste 期间最大帧 {max_frame:.2}ms（卡）");
}

#[test]
fn cjk_emoji_width_matches_unicode_width() {
    let report = lib::case_width(&data_dir());
    // 稳定类不一致会让 case_width 自己 assert 失败；这里再确认报告完整。
    assert!(report.contains("S1"), "{report}");
    assert!(report.contains("S7"), "{report}");
}

#[test]
fn resize_leaves_no_residue() {
    assert!(lib::case_resize());
}

#[test]
fn terminal_state_restored_on_normal_exit() {
    lib::case_restore("normal");
}

#[test]
fn terminal_state_restored_on_ctrl_c() {
    lib::case_restore("ctrlc");
}

#[test]
fn terminal_state_restored_on_panic() {
    lib::case_restore("panic");
}

#[test]
fn idle_cpu_under_1_percent() {
    let samples = lib::case_idle_cpu(8, &data_dir());
    assert!(!samples.is_empty(), "没有采到 CPU 样本");
    let avg: f64 = samples.iter().sum::<f64>() / samples.len() as f64;
    let max = samples.iter().cloned().fold(0.0, f64::max);
    assert!(avg < 1.0, "空闲平均 CPU {avg:.2}% ≥ 1%");
    assert!(max < 5.0, "空闲峰值 CPU {max:.2}% 异常");
}
