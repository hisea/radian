# 测试去重：pty 与 exec cli 套件

- 状态：decided
- 涉及 ADR：0023、0032
- 来源：2026-11-21 精简调查；相关条目 `verify-test-binaries.md`

## 背景

几个测试文件远超被测代码：`radian-tui/tests/pty.rs` 约 548 KB 只有 31 个测试，`radian-exec/tests/cli.rs` 约 421 KB；`render.rs`、`card.rs`、`footer.rs` 都对同一批块的渲染结果做断言，和 pty 测试可能大量重叠。pty 测试最慢、最易受机器负载影响，是 Verify 时间的大头。

## 决定（已定）

- 逐个列出 pty 测试覆盖的行为；凡是 `render`/`card`/`footer` 的帧断言已经覆盖的，从 pty 套件删掉，pty 只留必须经过真实终端的路径（启动与退出、按键、resize、panic 路径前提，见 `radian-tui/README.md` 手测表）。
- `radian-exec/tests/cli.rs` 按命令拆文件，删除与 `jsonl`、`acp`、`subagent` 套件重复的断言，大段内联 fixture 移到共享的 fixture 文件。
- 每删一个测试，在 PR 说明里写出覆盖它的那个测试；删完后用一次变异检查确认关键行为仍有测试失败（记入对应 crate 的 `MUTATIONS.md`）。
- 本条目只删重复，不降低覆盖；合并测试二进制仍归 `verify-test-binaries.md`。

## 验收

pty 套件的测试数与文件体积明显下降，Verify 的 pty 步骤耗时下降；每个删掉的测试都有对应的覆盖测试；Verify 全绿。
