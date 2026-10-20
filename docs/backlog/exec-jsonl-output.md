# `radian exec --output jsonl`

- 状态：idea
- 涉及 ADR：0032
- 来源：`docs/research/2026-10-18-area-deep-dive/README.md` 第 13、32 条

## 背景

无头模式没有机器可读输出，五种结局的退出码都是 1，`-p` 不能从 stdin 读（总报告第三轮"接入场景"小节）。

## 待决方向

`radian exec --output jsonl`（附结果行、细分退出码、`-p -` 从 stdin 读），约 2–3 天；给评测和 CI 用。之后再做控制口客户端（herdr 改走它）与 `radian acp`。

## 验收

五种结局（未完成、拒答、超预算、取消、失败）有区分的退出码与结果行；`-p -` 能从 stdin 读。

## 补充Ｐ）

- Terminal-Bench 适配器已合并（`benchmarks/terminal-bench/radian_agent.py`），它只拿得到 `-p` 的文本和退出码，所以这条的优先级高于研究报告里的位置。
- 无头 exec 的另一半问题（没有审批人、shell 的 `ask` 一律拒）由 `sandbox-auto-allow-shell.md` 解决，这里只管输出和退出码。
