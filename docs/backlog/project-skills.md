# 接上项目级 skill

- 状态：decided
- 涉及 ADR：0025、0031
- 来源：决定（ADR 0025 S1 / ADR 0031 T1 早已定下）；`docs/research/2026-10-18-area-deep-dive/README.md` 第 18 条（约 100 行，可顶替被删掉的提示模板；注：分报告 `8-extensibility.md` 不存在，内容在总报告 README 里）

## 背景

今天的行为（已对代码核实）：发现（`crates/radian-host/src/skills/mod.rs:105-133` 的 `discover_with_extras`）只扫用户级、`extra_paths` 与内建，没有项目路径；唯一调用方（`crates/radian-host/src/session/open.rs:246`）也只传用户级目录与 `extra_paths`。信任门只把 `<workspace>/.radian/skills/` 当触发资源（`crates/radian-host/src/trust.rs:424-432` 的 `has_skill_file`），信任与否都不加载。ADR 0025、0031、0020、0043 里"受信任才加载"的说法已改回现状。

## 决定（已定，按 ADR 0025 S1–S4 的机制接上）

- 受信任项目的 `<workspace>/.radian/skills/` 进入发现：系统提示节、`/skills`、`skill` 工具、`/skill:` 都与用户级 skill 一样可用；沿用 S1 的名字冲突保留先发现者、缺 `description` 不加载、校验失败只警告等规则。
- 未受信任时仍不加载（T1 现状不变）；信任触发、询问、记录流程不动。
- 支持 `$ARGUMENTS`（总报告第 18 条）：`/skill:` 后的参数与 `skill` 工具的 `args` 填进正文占位符，便于复用 Claude Code 格式的现成 skill。

## 验收

- 受信任项目：项目级 skill 出现在提示节与 `/skills`，`skill` 工具与 `/skill:` 可调用，`$ARGUMENTS` 被实参替换。
- 未受信任项目：仍不可见、不可调用；`config.toml` 的收窄照常应用。
- 落地分支一并核实 `trust::status_line` 的 `loaded (trusted)` 与非交互提示行是否与实现一致（今天代码先写了"已加载"的说法，实现没跟上）。
- 落地时同分支改 ADR 0025/0031（docs-only commit）并删掉本文件。
