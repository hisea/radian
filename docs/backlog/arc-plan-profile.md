# arc：plan 选自己的 gate profile

- 状态：decided（可以派发；很小，一个 worker）
- 涉及 ADR：0048（§3 对象、§10 gate、§15 配置）
- 来源：只改文档的 task 也要过本机测试、Linux 测试和模型 review，十几分钟起；配置里可以写多个 profile，但没有办法让某个 plan 用另一个

## 背景

`.radian/arc/config.toml` 可以定义多个 `[gate.profiles.<name>]`，但只有 `[gate] profile` 一处选择，对所有 plan、所有 task 生效。task 的 `extra_stages` 只能在 profile 上**加** stage，不能减。于是一个只改 `docs/` 的 plan 和一个改调度代码的 plan 走完全相同的 gate。

## 决定（已定）

1. **plan 有一个 `profile` 字段**：一个 `[gate.profiles.<name>]` 的名字；不写时用 `[gate] profile`。plan 的所有 task 用这个 profile，task 的 `extra_stages` 仍在它上面加 stage。
2. **谁设**：lead 在建 plan 时设（MCP `add_plan` 加一个可选的 `profile` 参数；`update_plan` 在 draft 上可改）。owner 在 confirm 前用 `show` 看得到它；confirm 之后不能改（与 request 一样）。名字不在配置里时，`add_plan` 和 `confirm` 都拒绝并列出已有的 profile 名。
3. **integration 不变**：不论 plan 用哪个 profile，merge 时照旧跑那个 profile 的 `key_commands`；一个 profile 没写 `key_commands` 时 integration 不跑命令，只做合并检查与推送。
4. `show`、`list --json` 和 MCP `show` 返回 plan 的 `profile`（未设时为 `null`，并给出生效的名字）。

本仓库打算的用法（配置，不在本项里）：一个 `docs` profile，只有 `commands`（fmt 与 `radian-coding` 的 docs 测试）和 `human`，`key_commands` 仍是完整 Verify。

## 验收

- 两个 profile：plan A 不设、plan B 设 `docs`；A 的 task 跑默认 profile 的 stage，B 的 task 跑 `docs` 的 stage，B 的 task 加 `extra_stages = ["review"]` 时再加 review。
- `add_plan` 给一个配置里没有的 profile 名被拒，消息列出已有的名字；配置在 confirm 前删掉了那个 profile，`confirm` 也拒。
- confirmed 的 plan 不能改 `profile`。
- merge 时跑的是 plan 的 profile 的 `key_commands`。
- 手册 `docs/user/arc.md`：plan 字段、`add_plan` 参数、选择规则；docs drift 测试看得到新字段。
