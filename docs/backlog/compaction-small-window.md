# 小窗口下一个 turn 内第二次压缩够不到目标，turn 直接失败

- 状态：idea（bug，方向待定）
- 涉及 ADR：0018（保留目标 `K`、`below_target` 规则、确定性兜底的第 ① 步）、0043（工具结果上限：shell 每流 24 KiB、read 16 KiB、grep 16 KiB，与窗口无关）
- 来源：2026-10-31 本地线路首次运行（LM Studio `qwen/qwen3-coder-30b`，`--context-window 32768`），事件序列见 `~/tb2-results/contamination-2026-10-31.md` 末节

## 现象

C = 32,768，输出未知所以 R = 8,192，B = 24,576，K = 6,144。一个 turn 里压缩了 6 次。前 5 次都成功，但压缩后都停在 11–13k，约 2K。第 6 次：最新一个工具结果是 43,728 字节的 `cargo test` 输出（约 11k token，stdout 和 stderr 两个流各接近 24 KiB 上限），按原子单元规则整个保留在尾部，摘要加尾部 19,696 > K，记 `below_target: true`。下一次调用的工具结果 8.7 KB 又把 transcript 推过 B，turn 以 "the previous compaction this turn was below target, so it will not compact again" 失败，`radian exec` 退出 1。

与模型无关，与泄漏无关：任何 32k 左右窗口的线路，任务里连跑两次 `cargo test` 就会撞上。1M 窗口下 B = 750k，一个工具结果永远不到 B 的零头，所以 462 个 worker 会话里从没出现。

## 原因

三条规则各自合理，合在一起在小窗口上没有出路：

1. 工具结果上限是固定字节数，不随 B 缩放。B = 24,576 时一个 shell 结果就是 B 的一半。
2. 压缩只在 user/assistant 边界切，最新的原子单元整个保留。尾部单独就能超过 K。
3. 一个 turn 只许一次 `below_target` 的压缩，再超预算就结束 turn。确定性兜底的第 ① 步（保留部分里超过 2000 字符的工具结果截到 2000）只在摘要失败时才走，这里摘要没失败。

## 待决方向

1. 主动路径上，摘要之后如果保留尾部单独超过 K，先对保留部分执行兜底的第 ① 步（`truncate_kept_tool_results`），再判 `below_target`。改动最小，只动 `radian-core` 的压缩计划；被截的结果模型刚刚看过完整版，丢的是复述不是信息。
2. `below_target` 之后再超预算时，不直接失败，而是允许一次带截断的再压缩；仍不行再失败。和 1 二选一或叠加。
3. 工具结果上限随 B 缩放（例如每个结果不超过 B/8 的 token 估算），在工具层解决。动 0043 的三个上限和手册，面大，但也让小窗口上每一步的 prompt 都小。
4. 保持规则不变，只把失败信息改成可操作的（提示调大窗口或缩小工具输出）。不推荐：32k 用户没法调大窗口。

建议 1 加 2。3 作为后续按本地线路的测量再定。

## 验收

- 复现：`--context-window 32768`，一个 turn 里连跑两次产生 >20 KiB 输出的 `cargo test`，turn 必须完成而不是失败；journal 里的 `ContextCompacted` 记录 `tokens_after` ≤ K 附近，或第二次压缩成功。
- 1M 窗口的行为不变：`compaction_flow.rs` 现有测试全过，`below_target` 的停止规则在非尾部原因上仍生效。
- 变异：去掉尾部截断步、去掉再压缩一次、把截断长度改成 0，各有一个测试失败。
