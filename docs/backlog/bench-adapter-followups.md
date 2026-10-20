# Terminal-Bench 适配器的后续修补

- 状态：decided
- 涉及 ADR：无（`benchmarks/terminal-bench/`，Python）
- 来源：Terminal-Bench smoke 与审查（2026-10-20）

## 背景（已核实）

- Harbor 的作业汇总里 `n_input_tokens`、`n_output_tokens`、`cost_usd` 都是 `null`：适配器没有把 radian 记下的 usage 回报给 Harbor（`populate_context_post_run`）。
- `auth.json` 用 `environment.upload_file` 传进容器后只 `chmod 600`，没有 `chown`（`radian_agent.py` 的 `run()`）。四个 smoke 任务都以 root 跑，没暴露；任务镜像若用非 root 的 agent 用户，radian 读不到这个 0600 文件。Harbor 自带带 chown 的 `_upload_config_text`。
- 适配器没有给 radian 传 `--turn-timeout-secs`，到点被外部杀掉（见 `deadline-awareness.md`）。

## 决定（已定）

1. 从试验的 `radian-sessions/*.jsonl` 汇总 `ModelUsageRecorded`（token 与费用）回报给 Harbor 的 context。
2. `auth.json` 改走 Harbor 的带 chown 上传（或等效 chown），对非 root 默认用户可读。
3. 传 `--turn-timeout-secs` = 任务 agent 超时减一个余量（例如 60 s），让 radian 自己收尾。

## 验收

单测覆盖三项（usage 汇总、chown 调用、timeout 参数）；一次 smoke 后 Harbor 汇总里 token/cost 不为 null。
