# 剩余时间感知：radian 知道这一轮的期限，并告诉模型

- 状态：idea（定是否做成产品功能）
- 涉及 ADR：0017（回合结束）、0036（`<runtime>` 层或运行中提示）、0032（`radian exec` 的 flag）
- 来源：Terminal-Bench smoke（2026-10-20）

## 背景（已核实）

- `compile-compcert` 在 40 分钟上限前 3 分钟才开始主构建；`cobol-modernization` 到 900 s 都没写交付物。两次都是被外部强制停掉，radian 没有自己结束这一轮，journal 不完整。
- `radian exec` 已有 `--turn-timeout-secs`；Terminal-Bench 适配器没有传它（见 `bench-adapter-followups.md`）。

## 待决方向

- 至少：调用方把期限传给 radian（`--turn-timeout-secs` 略短于外部上限），radian 到点自己收尾、写完 journal。
- 进一步：radian 在提示里告诉模型剩余时间（例如过半、剩 20% 时各一次），让它先交付再优化。这对"无人值守 worker"定位直接有用，但会改变提示词前缀稳定性，要按 ADR 0036 的缓存约束放（追加消息而不是改系统提示）。

## 验收

带期限的运行在期限前结束且 journal 有正常的 `TurnFinished`；子集评测里超时类失败减少。
