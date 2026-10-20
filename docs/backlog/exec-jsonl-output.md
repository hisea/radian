# `radian exec --output jsonl`

- 状态：decided＼�
- 涉及 ADR：0032（A3）、0010、0006；新 ADR 定输出格式与退出码
- 来源：`docs/research/2026-10-18-area-deep-dive/README.md` 第 13、32 条；

## 背景

无头模式没有机器可读输出：`-p` 的 stdout 只有模型文本（0032 A3），要结构化信息只能自己读 journal 或连控制 socket。Terminal-Bench 适配器（`benchmarks/terminal-bench/radian_agent.py`）只拿得到文本和退出码，而"没完成、超预算、取消、失败"今天都是退出码 1，评测分不出来。Codex（`--json`）、pi（`--mode json`）、Claude Code（`--output-format json`）、opencode（`--format json`）都有。

无头 exec 的另一半问题（没有审批人、shell 的 `ask` 一律拒）由 `sandbox-auto-allow-shell.md` 解决，这里只管输出和退出码。

## 决定（已定）

- **`--output jsonl`**：stdout 每行一个 JSON 对象。durable 行就是 0006 的 journal 记录原样（同一序列化，不另造 schema）；live 行（流式文本 delta、reasoning delta）按 0010 的划分另加 `"live": true`，消费者可以过滤。最后一行是 `Result` 记录：`session_id`、`outcome`、`exit_code`、`stop_reason`、`turns`、`usage`（0007）、`cost`、最后一条 assistant 文本。缺省输出不变（`-p` 仍只有模型文本）。状态、提示、错误仍走 stderr。
- **退出码按结局分开**，在 0032 A3 的表上加行，对所有 exec 模式生效、不只 jsonl：0 完成；1 本轮失败（模型/hook 错误，0017）；2 用法/设置；3 超预算或轮数上限；4 结束但没有完成的回答（模型停了、空回复重试用尽、工具被拒后放弃）；130/143 取消（二次 Ctrl-C / SIGTERM，沿用 128+signal）。"拒答"不是退出码：无法可靠判定，只作为 `Result.stop_reason` 的一个值由模型文本之外的信号给出时填写。
- **`-p -` 从 stdin 读提示**，到 EOF 为止；与 `--output jsonl` 组合即可被脚本驱动。
- 不做：`--output json`（整段一个对象）、流式以外的新事件、控制 socket 客户端（另开）。

## ADR 改动

新 ADR：行格式、`Result` 字段、live 行标记、退出码表；0032 A3 的表改为引用它。

## 验收

golden 测试：一次 mock 会话的 jsonl 输出逐行过 `docs/schema/check-fixtures.sh` 的校验器（durable 行与 journal 文件逐字节相同；`Result` 行有自己的 fixture）；五种结局各一个测试钉住退出码（变异：任一结局退回 1 应失败）；`-p -` 读 stdin 并在 EOF 后开始；`-p` 的 stdout 字节与改动前相同（变异：jsonl 行漏到缺省输出应失败）；Terminal-Bench 适配器改读 `Result` 行并在 README 里记下。
