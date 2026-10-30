# 评测：journal 指标提取器 + 仓库任务固定集

- 状态：decided
- 涉及 ADR：0006（会话 JSONL，只读消费，不改）、0007（usage 记录，只读）
- 来源：`research/2026-10-18-area-deep-dive/README.md` 第 1 条；分报告 6 §7；2026-10-29/30 的三次 A/B（codemode、工具集、read）

## 背景

radian 的每一次改进都靠 A/B 量收益，而每次 A/B 都在仓库外临时手写一套 journal 解析：codemode A/B 一套、工具集 A/B 一套（含 pi 会话的读法）、grep/read 比较又三套。它们各自定义"总输入"（含不含缓存读）、"步数"、"工具结果字节"（JSON 序列化前后）、"成功"（只跑检查，还是检查加评审），数字彼此不能直接比，做完就丢。

仓库里已有的 `benchmarks/terminal-bench/metrics.py` 只认 Harbor 的 job 目录，读 radian journal 的那一段埋在里面，不能单独对一个会话目录或一批 arc worker journal 用。Terminal-Bench 量的是通用任务；本仓库的决定（工具集、read/grep 缺省值、压缩阈值、prompt 改动）要在**本仓库的真实任务**上量，这类任务集现在只存在于两次 A/B 的临时目录里。

journal 本身已有全部原料：每次模型调用的 usage 与费用、工具起止与结果、压缩记录、时间戳（ADR 0006、0007）。缺的是一个固定口径的提取器和一个固定的任务集。

## 决定

1. **一个 journal 提取器，仓库内，stdlib only**：`benchmarks/journal_metrics.py`，输入是 radian 会话 journal 文件或目录（任意多个，可按文件名前缀过滤），输出每个 journal 一行加合计，`--json` 给机器读。它也读 pi 的会话 JSONL（`{"type":"message",…}` 格式），同一套列，pi 的 `bash` 归到 `shell` 列，这样 pi 对 radian 的比较不再各写一套。
2. **列与口径固定，写在脚本顶部的一张表里，README 从那张表生成**：模型调用数（= 步数）；输入 token 分未缓存与缓存读两列，`总输入` = 两者之和；输出 token 与其中的 reasoning；费用（radian 取 `fee`，pi 取 usage 里的 cost）；按工具名的调用数与结果字节（结果字节 = 结果文本的 UTF-8 字节，不含 JSON 包装）；`read` 的带范围/不带范围次数与字节、单次 ≥ 20 KB 的次数；每次调用的延迟（上一条就绪事件到 `ModelUsageRecorded`）按 prompt 大小分桶（每 100k 一桶）给 p50/p90；压缩次数与 `deterministic`；turn 结局；墙钟。超出这张表的指标在新一轮 A/B 里先临时算，证明有用再加列。
3. **`terminal-bench/metrics.py` 复用它**：`_journal_metrics` 改为调用新模块，列名不变，`--self-test` 的 fixture 结果不变。Verify 的步骤清单不加项：新模块的测试并入 `tests/test_metrics.py` 那一步（`metrics.py --self-test` 已在 `scripts/verify.sh` 里）。
4. **仓库任务固定集**：`benchmarks/repo-tasks/`，内容是任务清单（arc 任务 id、base sha、worker brief 原文、原始 attempt 的结局）、按 arm 跑一次的 runner（radian：`radian exec --output jsonl -p -`，工具集与模型由 arm 文件给；pi：同 brief、同模型）、crate 级检查脚本、用 arc 首轮评审 prompt 的评审脚本、生成报告的脚本。先收进现有的 4 个任务（TK0220、TK0223、TK0226、TK0228）；扩到 8–10 个时按规则挑：已合并、首轮通过、改动集中在一个 crate、不同 crate 各至少一个。任务目录不放密钥，runner 读本机 radian/pi 的凭据。
5. **报告格式固定**：逐任务一张表（arm × 列），合计一张，对基线的比值一张；基线由 `--baseline` 指定。
6. **判定规则进 README**：每个 arm 至少 k=2；同一 arm 自己两次的差就是噪声带，小于噪声带的效果当不存在；逐任务排序（每一次都优于基线每一次）比合计数可信；规则在跑之前写进该轮的 brief，跑完不改。这是三次 A/B 的教训：k=1 的合计差 20–25% 时，同配置重跑差到 1.76 倍。
7. **不做**：不跑在 CI 里（要模型调用，要钱）；不做仪表盘、数据库、图；不动 Terminal-Bench 的任务集与 Harbor 适配器；不在 arc 里加"评测"子命令。journal 格式不改（ADR 0006 不动），所以这一项落地时只删本文件，不改 ADR。

## 验收

- 用提取器跑 2026-10-30 工具集 A/B 的 `runs/*/session/` 与 attempt-1 的 journal，总输入、步数、按工具的结果字节与那次报告的数字一致（取整误差内）；跑 codemode A/B 的 pi 会话，与那次报告的 pi 列一致。
- `terminal-bench/metrics.py --self-test` 在改成复用后仍通过，fixture 输出不变。
- `tests/test_metrics.py` 新增用例覆盖：一个 radian journal fixture（含工具调用、一次压缩、一次失败的 turn）、一个 pi 会话 fixture、延迟分桶、`read` 范围列、JSON 输出与表格输出同源。
- `benchmarks/repo-tasks/` 对 4 个任务跑一个 arm 一遍，从仓库里一条命令跑起，产出按第 5 条格式的报告；README 写清每一列的定义与第 6 条的规则。
- 变异记录按仓库规则写进 `benchmarks/terminal-bench/MUTATIONS.md`（若无则新建）：改口径（如把缓存读算进未缓存）、改延迟锚点、改 pi 的 `bash` 归类，各有测试失败。
