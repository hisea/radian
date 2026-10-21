# 面向用户的文字里不出现 ADR 引用

- 状态：decided＼�「用户不知道 ADR 是什么，把所有命令说明、面对用户的文档里的 ADR 引用全部去掉」）
- 涉及 ADR：0008（配置模板）、0027（footer 预设）、0035（视觉规则不变，只改文字）；不改任何决定
- 来源：

## 背景（2026-10-20 在 c8a3ba10 上核实）

ADR 编号是给开发者和 agent 看的；用户看到「(ADR 0026 D3)」只会困惑。现在出现在这些地方：

- **命令行帮助**：`radian --help`、`radian exec --help`、`theme`/`footer`/`login` 等子命令的帮助共 49 处，例如「(ADR 0029)」「(ADR 0021 D7)」。来源是 clap 从 `///` 文档注释生成的帮助文字，所以要改的是 CLI 结构体字段与子命令上的那些文档注释（只改会进帮助的那部分）。
- **TUI 里的命令说明与通知**：非测试代码的字符串字面量里 12 处，例如 `/compact` 的说明「… (ADR 0018)」（`crates/radian-tui/src/command.rs` 7 处）、`[budget] … (ADR 0018)`（`agent-cli/src/chat.rs`）、权限拒绝「denied by ADR 0038: …」（`radian-permissions/src/profile.rs`）、登录向导与 provider 的「(ADR 0026 D10)」、控制口拒绝「(ADR 0043)」。`format!` 拼出来的文字也算。
- **首次启动写给用户的配置模板**：`crates/radian-host/src/config/default.rs` 的 `DEFAULT_CONFIG_TEMPLATE` 6 处（如 `# radian configuration (ADR 0008).`）。
- **footer 预设**：`crates/radian-tui/presets/deck.toml`、`full.toml` 各 1 处注释，`radian footer export` 会原样打印给用户。
- **面向用户的文档**：根目录 `README.md` 26 处、`docs/sandbox.md` 46 处、`docs/pi-migration.md` 2 处。

## 决定（已定）

- 上面这些地方的 ADR 引用全部去掉。去掉引用不等于去掉内容：引用后面带的说明改写成用户能懂的一句话（例如「denied by ADR 0038: .git entries git can execute … are read-only」→「denied: in the workspace tier, .git entries that git can execute (config, hooks) are read-only」）。
- 只改用户看得到的文字。代码注释、测试、`docs/adr/`、各 crate 的 `README.md`、设计文档、`docs/perf-baseline.md`、`docs/provider-contract.md` 是开发者与 agent 的文档，保留 ADR 引用。
- 发给模型的文字（系统提示、工具描述）不在本条范围。
- 以后的面向用户文字（包括将来内置的用户文档，见 `docs/backlog-bundled-docs` 分支上的条目）同样不引用 ADR。

## 验收

- 一个测试遍历 `radian` 与每个子命令的 `--help` 输出，断言不含 `ADR`。
- 一个测试遍历 TUI 斜杠命令表的说明与 `/help` 文字，断言不含 `ADR`。
- 配置模板与四个 footer 预设不含 `ADR`（测试）。
- `README.md`、`docs/sandbox.md`、`docs/pi-migration.md` 不含 `ADR`（检查脚本或测试）。
- 变异：在任一处帮助文字里加回「(ADR 0008)」，对应测试必须失败。
- 已有断言里引用了旧文字的测试同步改，不放松断言。
