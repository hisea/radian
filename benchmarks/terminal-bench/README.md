# 用 Terminal-Bench（Harbor）评测 Radian

这个目录是 Radian 的 Terminal-Bench 接入层：一个 Harbor custom agent，把当前
commit 构建出的 `radian` 二进制放进任务容器跑分。它**不是** Rust workspace 的
一部分，不增加任何 Rust/Python 依赖。

官方入口说明：Terminal-Bench 旧版 `tb` harness 已被 Harbor 取代
（`harbor-framework/harbor`），新任务与评测都走 Harbor。本目录跟随 Harbor 的
`BaseInstalledAgent` 接口（`--agent module.path:Class`）。

## 文件

| 文件 | 说明 |
|---|---|
| `radian_agent.py` | Harbor agent：上传二进制、写 `auth.json`、跑 `radian exec -p <instruction>` |
| `radian_command.py` | 纯标准库的命令构造器（provider 选择、`auth.json` 渲染、shell 引用、脱敏），可独立单测 |
| `tests/test_radian_command.py` | 构造器的单测（无 Harbor 依赖） |
| `tests/test_radian_agent.py` | agent 的单测（Harbor 模块用替身，只覆盖传参/密钥/退出码链路） |
| `pyproject.toml` | 说明性 metadata，不参与 Rust 构建 |

## 关键设计（与评测有效性相关）

- **测的是 agent 循环，不是 sandbox**。默认 `--profile full`：任务容器本身就是
  隔离边界，在默认 Docker 配置里再嵌套 bwrap 经常被 user namespace/seccomp
  拒绝。用 `workspace` 档会把"宿主是否允许嵌套沙箱"混进分数。Radian 自身的
  sandbox 由 `crates/radian-sandbox` 的逃逸矩阵单独覆盖。
- **密钥只走环境变量**。`$RADIAN_BENCH_API_KEY` 是唯一出现密钥的地方；命令行
  与上传的 `auth.json` 只写 `$RADIAN_BENCH_API_KEY` 这个名字（ADR 0013，密钥
  不进 stdout、不进文件）。Radian 的 `anthropic` provider 不接受 `--base-url`
 （ADR 0026 D0），所以带自定义端点的 Anthropic 格式流量走 `openai-compatible`；
  只有无自定义端点的 Anthropic 格式才走 `--provider anthropic`。
- **无人值守**：每次 trial 固定带 `--yes --no-trust --offline-catalog`，`auth.json`
  权限 0600，session 与 stdout/stderr 落到 Harbor agent logs（`radian-sessions/`）。
- **可复现**：`install()` 记录二进制 sha256 与 `radian exec --version`，写进
  `radian-binary.json`；固定 `--context-window` 可跳过 models.dev 查询。

## 运行单测（不需要 Harbor、不需要模型、不需要 Docker）

```sh
python3 benchmarks/terminal-bench/tests/test_radian_command.py
python3 benchmarks/terminal-bench/tests/test_radian_agent.py
```

## 跑 smoke（3–5 个任务，需 Docker + Harbor + 模型）

```sh
# 1. 构建待测的 Linux 二进制（Harbor 任务容器是 Linux）
cargo build --locked --release
# 产物：target/release/radian，记下 sha256
sha256sum target/release/radian

# 2. 装 Harbor（另建 venv，不要污染本仓库）
uv tool install harbor  # 或 pip install harbor

# 3. smoke（示例，按 Harbor 当前文档核对 task 数据集名与版本）
harbor run \
  -t <task-id> \
  -a benchmarks.terminal-bench.radian_agent:RadianAgent \
  -m openai/gpt-5-mini \
  --agent-kwarg binary_path=/absolute/path/to/target/release/radian \
  --agent-kwarg binary_sha256=<sha256> \
  --agent-kwarg max_model_calls=100 \
  --agent-kwarg turn_timeout_secs=600 \
  --agent-kwarg context_window=200000
```

`PYTHONPATH` 需要包含仓库根（让 `benchmarks.terminal-bench.radian_agent` 可
import，且 `radian_command` 与它同目录）。smoke 先验证：二进制进容器、
密钥传递、工作目录成为 `--workspace`、verifier 能看到修改、非零退出正确
标记失败。smoke 全绿之前不跑完整集；全量前固定并记录：Radian git SHA、
binary sha256、Harbor 版本、数据集名/版本/task IDs、模型与参数（见方案）。

## 不测什么

- ATIF trajectory 转换：Terminal-Bench 以 verifier reward 为准；排障用 Radian
  原生 JSONL journal（`--session-dir` 指向 agent logs）。需要 token 级可视化时
  再从 journal 加转换器。
- MCP/skill：第一版不支持（`capabilities` 全关）；任务若要求 MCP 再议。
