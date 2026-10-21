# 合并集成测试二进制，压缩 Verify 时间

- 状态：idea
- 涉及 ADR：新 ADR（journal-selfcheck 的作用范围会变）
- 来源：`research/2026-10-15-build-timings.md` §3、§5；总报告第三轮"先不做"末条；`4-architecture.md` §6 ⑤

## 背景

一次 Verify 6–7 分钟，冷构建里链接 144 个集成测试二进制是最大的一项，测试 target 三天内从 162 涨到 200。

4-architecture ⑤ 把它写成"不碰 ADR、只改构建配置"，估轻了：14 个测试文件调用 `env::set_var`/`remove_var`，41 个文件有 `static` 的 `Mutex`、`OnceLock` 或原子量，合进一个进程会互相干扰（edition 2024 下 `set_var` 本来就是 `unsafe`），`journal-selfcheck` 的作用范围也跟着变。

## 范围

- 做：按 crate 把 `tests/*.rs` 合成少数几个二进制，先处理 `set_var` 和全局状态；可以配合 nextest（一个测试一个进程，`14-dev-process.md` ⑧）。
- 不做：`[profile.release]`、LTO、`codegen-units`，等有发布需求再说。
- 已经做了：`radian-tui` 的 build script 不再每次生成语法集，改为校验提交的 `assets/syntaxes.packdump`，40.9 s 这项已经没有了。

## 验收

Verify 墙钟时间前后对比；合并后每个原测试仍单独可定位；`journal-selfcheck` 覆盖范围不变或写明怎么变。
