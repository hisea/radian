# arc plan 优先级：P0–P5

- 状态：decided（可以派发；排在 `arc-run-nonblocking.md` 落地之后，两边都改 `run.rs` 的领取）
- 涉及 ADR：0048（§3 对象、§1 命令、§12 auto、§13 调度循环）

## 背景

调度循环领取 ready task 的顺序是写死的：plan 按 id 从小到大，plan 内按 task 顺序，有空名额就领。编号大的 plan 永远排在后面。要让某个 plan 先做，今天只能把其他 plan 的 auto 改成 `off`，或者手动 `radian arc start`；两种都要事后再改回来，也不表达"谁更要紧"。

## 决定（已定）

1. **plan 有一个 `priority` 字段**，取值 `P0`、`P1`、`P2`、`P3`、`P4`、`P5`，`P0` 最高。默认 `P3`。task 没有自己的优先级，跟随所在 plan。
2. **领取顺序**：先按 plan 的 `priority`（`P0` 先），同级按 plan id 从小到大，plan 内仍按 task 顺序与 `after`。`[limits] concurrency` 不变；优先级只决定空名额给谁，不抢占已经在跑的 attempt。
3. **gate 与合并的顺序**：同一轮里待验证、待合并的 task 也按同样的顺序排（同一阶段的锁仍然排队，优先级高的先进队）。
4. **谁设**：owner 命令 `radian arc priority <PLAN> <P0..P5>`，任何状态的 plan 都可以改，立即对下一轮生效。lead 没有设它的 MCP 工具（与 confirm、auto 同属 owner）；MCP `show` 与 `list` 返回它。
5. **显示**：`radian arc list` 与 `show` 在 plan 行显示优先级；`list` 的 plan 按领取顺序排列，与调度一致。arc TUI 的 plans 页同样显示并按此排序（TUI 那边改优先级的按键跟随 TUI 的 owner 操作再定）。
6. **旧数据**：没有 `priority` 字段的 plan 文件按 `P3` 读，不需要迁移。

## 验收

- 两个 confirmed、auto `run` 的 plan，编号大的设 `P1`：名额先给它的 task；同为 `P3` 时仍按编号。
- 优先级不抢占：低优先级的 attempt 在跑时，高优先级 plan 只拿空名额。
- `priority` 改动下一轮生效；非法值（`P6`、`p1`、`high`）被拒，退出码 3，消息列出全部取值；取值列表只有一处，解析和测试共用。
- 没有字段的旧 plan 文件按 `P3` 读。
- `list` 的 plan 顺序与领取顺序一致。
- 手册 `docs/user/arc.md`：`priority` 命令（参数、全部取值、默认、何时生效、退出码）、plan 字段、领取顺序一段；docs drift 测试看得到取值列表。
