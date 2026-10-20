# 首次启动流程（first launch）

- 状态：decided
- 涉及 ADR：0008
- 来源：决定（2026-10-17；`git show 356df1e -- docs/adr/0008-config-schema-v1.md`）

## 背景

今天的行为（已对代码核实）：配置文件缺失时 `load` 直接返回空配置，永不创建（`crates/radian-host/src/config/mod.rs:231-250`）；没有模型时 TUI 与 `radian exec` 都直接报 `no model: pass --model, or set model.model in the config file` 并退出（`crates/radian-tui/src/cli/run_resolve.rs:61`、`crates/agent-cli/src/setup.rs:113`），不打开任何选择器，不写文件。唯一写配置的入口是 `/theme`（`crates/radian-host/src/config/theme_write.rs:49`）。ADR 0008 §2/§5 曾写过一套没实现的行为，已改回现状。

## 决定（已定，照 owner 原文）

1. 文件不存在时，`radian` 与 `radian exec` 启动时创建它（目录 0700）：只有 `schema_version = 1` 与常用键的**注释**模板，启动行说一次「created a default config at <路径>」。文件已存在时一个字节都不动；创建失败只警告、照常继续。
2. 没有模型时 TUI 不退出：打开后直接进 `/model` 的选择（provider 没有 key 时接着走 `/login`，ADR 0026 D3/D5）；选定后把 `[model] provider` 与 `model` **写回**这个文件，保留原文、只改这两个键（与 `/theme` 同一写法）；写不了时只提示、本次照常用选中的模型。
3. `radian exec` 没有交互界面：照旧报 no model 并以非零退出，但消息写明默认配置已在哪里创建，以及 `--provider <p> --model <m>`。

这放宽「配置是只读输入」：radian 只在**没有文件时创建一份**，以及**用户在界面里选了模型时**写回这两个键；其他写入仍只在用户显式操作时发生（`/theme`）。

## 验收

- 删掉配置启动：文件按模板建好（含注释），启动行有路径，第二次启动一字节不动。
- 无模型进 TUI：出现 `/model` 选择，无 key 时能接 `/login`；选定后配置文件只有两个键变化，其余逐字节保留；写回失败时只提示、本次会话照常用。
- 无模型 `radian exec`：非零退出，消息同时点名已创建的文件与 `--provider/--model`。
- 落地时同分支改 ADR 0008（docs-only commit）并删掉本文件。
