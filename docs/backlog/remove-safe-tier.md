# 去掉 `safe` 档：档位只剩 `workspace` 与 `full`

- 状态：decided＼�
- 涉及 ADR：0016（主）、0012、0038、0043、0031、0008、0020、0021、0026、0033、0034、0036
- 来源：

## 背景

`safe` 今天是：工具只读（`write`/`patch`/`shell`/MCP 全 deny，`!` 不执行），关网，启动行写 "no sandbox needed: no command execution"。它**从不执行命令**，所以 `radian-sandbox` 里的 `Policy::safe()`、seatbelt 的 safe 测试和逃逸矩阵的 safe 行（含 Linux `safe_denied` 的已知限制行）在产品路径上没有人用，只有测试在跑。

它已经是二等档位：0043 §3 的启动行只定义了 `full` 和 `workspace` 两句；信任门的"这个项目用沙箱"选的是 `workspace`；dogfood 从没用过 `safe`；只读需求由 `explore` 子会话的工具集覆盖。

去掉之后会丢的唯一能力：**一个关网、无 shell 的只读会话**。`workspace` 开网且没有关网的配置键（0016 §1，`[sandbox]` 只有 `allow_unix_sockets`）。如果将来要"离线"，另开一个 `[sandbox] network = off` 的事项，不靠档位。

## 决定（已定）

- 档位只剩 `workspace` 与 `full`。`/access` 两个值；项目 `.radian/config.toml` 的 `profile` 只认 `workspace`（意思不变：本项目要求沙箱）；"两处取更窄"退化为"用户或项目任一方要沙箱就沙箱"。
- `radian-permissions` 删 `Preset::Safe` 与 `Profile::safe()`；`radian-sandbox` 删 `SandboxMode::Safe`、`Policy::safe()` 和逃逸矩阵的 safe 行；`describe` 删 "no exec" 一句；TUI 删 `Tier::Safe`、`/access safe` 补全、`!` 的 safe 拒绝分支；herdr 上报的 `tier` 只剩两个值（0034）；Terminal-Bench 适配器的 `profile` Literal 同步。
- 旧 journal 不动：`SessionCreated.profile = "safe"` 作为历史字符串原样读回，replay 与 `journal-selfcheck` 不因它失败（0006 的兼容要求）。

## 已记录的 `safe` 怎么处理＼�radian 还没发布，没有人用 `safe`）

不做映射、不留别名、不警告：`safe` 和任何打错的档位名一样，走现有的 fail closed 路径。`access.json` 里出现 `"safe"` 整文件拒绝，项目 `profile = "safe"` 按未知值处理（0008），`--profile safe` 与 `/access safe` 报未知档位。旧 journal 的 `profile = "safe"` 仍是一个原样读回的字符串，不需要特殊处理。

## 落地

- 一个 docs-only commit 改 12 个 ADR：0016 §1 改成两个档位；0012、0038、0043、0031、0008 的 `workspace | safe` 列表；0020/0033/0021/0026/0036 里"`safe` 档没有 shell/不建沙箱/禁读"的句子；0034 的 `tier` 值域；`docs/sandbox.md` 的 safe 行；`docs/schema/config/README.md`。
- 代码约 43 处、22 个文件，另有 `access`、`access_switch`、`question`、`status`、`pty`、`cli`、`rule`、`project` 等测试里的 safe 用例要删或改。`ask.rs`/0024 示例里的 `"safe"` 只是样本选项 id，与档位无关，不动。

## 验收

`--profile safe` 与 `/access safe` 报未知档位；`access.json` 里的 `"safe"` 整文件拒绝，项目配置里的按未知值处理；旧 journal 的 `profile = "safe"` 能 resume 且 `journal-selfcheck` 通过；信任门与 `/access` 只出现两个档位；逃逸矩阵没有 safe 行且 Linux 的已知限制表少一行；herdr 上报只有两个 `tier` 值。变异：`safe` 被悄悄当成 `full` 或 `workspace` 接受，测试应失败。
