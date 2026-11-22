# `/context` 估算复用核心计数

- 状态：decided
- 涉及 ADR：0015
- 来源：2026-11-21 精简调查

## 背景

`radian-host/src/context_estimate.rs`（约 48 KB，其中测试约 460 行）为 `/context` 按类别估算下一次请求。它说明自己"重新实现了计数"，因为 `radian_core::budget` 的 `add_str`/`add_json` 是私有的，再用测试把两边的总数钉在一起防止漂移。

## 决定（已定）

- `radian_core::budget` 公开按块计数的接口，`context_estimate.rs` 改用它，删掉重复的计数实现和只为防漂移存在的测试。
- 估算规则（ADR 0015 C2）和 `/context` 的显示不变。记忆类别随 `remove-memory.md` 一起删，不在本条目里做。

## 验收

`/context` 输出与改动前一致；`context_estimate.rs` 里没有自己的字节计数；Verify 全绿。
