# 自动转后台阈值 120 s → 60 s，并在提示词里要求独立调用放同一条回复

- 状态：idea
- 涉及 ADR：0037（决定 2 的缺省值）、0036（基础提示词的角色规则）、0011（运行中 steer）
- 来源：`docs/research/2026-10-18-area-deep-dive/2-tools.md` P7 与 §5.11；总报告第 11 条（原来只占索引行）

## 背景

- `[shell] auto_background_secs` 缺省 120 s（`crates/radian-coding/src/provider.rs`）。omp 是 60 s，Codex 10 s 就返回 session id，opencode 前台缺省 2 min 但 UI 可随时转后台。
- 实测 shell p90 是 72 s，最长的都卡在 600 s 上限（`docs/research/2026-10-15-tool-concurrency-measurement.md` §3），约一成调用在 60–120 s 之间白等。
- 转后台的收益要靠模型在等的时候做别的事。`crates/radian-host/prompts/default.md` 里没有"互不依赖的工具调用放在同一条回复里"这句话；实测 93% 的回复只调一个工具（同文 §1）。两件事绑在一起才有意义。

## 待决方向（建议，未定）

1. 缺省改 60 s，0037 决定 2 同步一句。
2. `default.md` 加一句独立调用并行的规则（0036 管层次不管这句话，不需要改 ADR）。
3. 用评测集决定要不要再降到 30 s。

## 验收

同一批长构建任务比较墙钟时间和轮数；journal 里"一条回复多个工具调用"的比例上升；`docs/schema/config/shell-auto-background.toml` fixture 更新。
