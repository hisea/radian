# 32k 窗口：below_target 的"每 turn 一次"额度在 exec 单 turn 里等于"每任务一次"

- 状态：idea（待 owner 定）
- 涉及 ADR：0018（§Gain / below_target、额外一次压缩）
- 来源：2026-10-31 Hai-PC 本地线路（qwen3-coder-30b，`--context-window 32768`，二进制 e4232e8c7）

## 现象

ADR 0018 现在的规则：一次压缩后仍高于目标（`below_target`）时，这个 turn 再允许一次压缩；之后再超过 B 就以 BelowTarget 结束 turn。`radian exec -p` 一个任务就是一个 turn，所以这个额度实际是每任务一次。

本地轮 9 次运行里 2 次因此 Failed，都是同一形状：

| 运行 | 第一次 below_target | 额外的那次 | 之后 | 失败 |
|---|---|---|---|---|
| local-qwen-32k TK0223 k=2 | 37159 → 23247 | 27627 → 11600（达标） | 又正常压缩 2 次 | 30133 → 20899 below_target → 下一次直接 `failed: a previous compaction this turn was below target` |
| local-qwen-32k TK0226 k=2 | 27358 → 18700（目标 18432，差 268） | 26784 → 11331（达标） | 又压缩约 60 次、跑了近一小时 | 33157 → 23375 below_target → 同上 |

额外那次每次都达标，turn 回到普通规则，但 `extra_compaction_used` 不复位；一小时后第二次 below_target 出现时额度已经用掉，turn 就结束了。第二次 below_target 和第一次之间没有关系。

同一轮里 TK0228 k=1 也出现一次 below_target（32963 → 22675），额外那次 29415 → 11011 达标，之后没有再出现，运行完成。

## 待决方向

1. 额外压缩达标就复位：规则改成"连续两次压缩都 below_target 才结束 turn"。改动只在 `crates/radian-core/src/turn.rs` 的那个标志；ADR 0018 的那句话改写。
2. 不按 turn 计，按"自上次达标的压缩以来"计。与 1 等价，措辞不同。
3. 保持现状。不推荐：32k 窗口下任务越长越必然撞上。

## 验收

- compaction_flow 测试：below_target → 额外压缩达标 → 正常压缩 N 次 → 再 below_target → 再允许一次额外压缩；连续两次 below_target → 失败，reason 不变。
- 变异：复位去掉（现有"额外一次"测试仍过，新测试失败）。
- 复跑本地轮里的 TK0226 k=2 形状任务，不再因 BelowTarget 结束。

## 同轮的第二个现象：小模型的摘要不收缩，回退又把它原样留下

TK0226 k=2 中段连续 12 次 `compacted without a summary (N dropped)`，不是服务端故障（LM Studio 日志无错误）。journal 里是一个摘要雪球：

- 每次摘要调用都把上一份摘要喂给模型，qwen 不缩短它。各 revision 的摘要输出：rev 31 3734 tok、rev 34 4169、rev 41 4699、rev 46 5512、rev 47 5986、rev 49 6147；上限 `summary_max_tokens(B = 24576) = 6144`。
- rev 47 起每次压缩有两条 `CompactionUsageRecorded`（首次 + 一次缩短重试），两次都约 5.6–6.1k tok，即 TooLong 两次 → `SummarizeOutcome::Fallback` → 确定性回退：保留上一份摘要，附一句 "N dropped"，摘要文本每次再长约 86 字符（24004、24089、24175 … 字符）。
- 于是保留的摘要停在约 6k tok = B 的四分之一，可用空间缩小，每隔几次调用就压缩一次（该运行 70 次），最后一次没达标 → 第二次 below_target → 上面的失败。

待决方向（与上面独立）：

1. 摘要 TooLong 两次后，回退不再原样保留旧摘要：对旧摘要做一次确定性截断（例如截到上限的一半），再附 "N dropped"。
2. 摘要提示里给出字数上限，并要求替换而不是追加上一份摘要。只靠提示在小模型上不一定管用，但成本为零。
3. 小窗口下把摘要上限从 `min(K, max(8192, B/8))` 调低。要先看它对 128k 窗口的影响。

验收：同一运行形状下摘要 token 数不单调增长；连续两次 TooLong 后 journal 里的摘要长度下降而不是上升。

## 测试环境误差（报告用）

`check.sh` 不构建 `mcp-fixture`，`radian-host::mcp_cwd` 在模型没有自己构建整个 workspace 的运行里以 "mcp-fixture is not built" 失败（qwen TK0220、TK0228 撞到；Mac 上的 TK0220 都过）。这一项按环境失败计，不进通过率。
