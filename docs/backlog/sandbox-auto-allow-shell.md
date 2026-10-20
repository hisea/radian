# workspace 档沙箱内自动放行 shell

- 状态：idea
- 涉及 ADR：0012、0016、0043
- 来源：`docs/research/2026-10-18-area-deep-dive/README.md` 第 8、16 条；`4-architecture.md` §6 ①；评估

## 背景

workspace 与 full 两档 shell 都是 `ask`（`crates/radian-permissions/src/profile.rs`），开沙箱换不回任何一次提示，dogfood 只能用 `full + --yes`。

影响面比"少点几次确认"大：权限层对**非人类回合**的 `ask` 一律 deny（`crates/radian-permissions/src/hooks.rs`，fail closed）。无头 `radian exec`、subagent 子进程、steer 和后台任务通知触发的回合、herdr 发起的回合，今天碰到 shell 都只能靠 `--yes`。这正是"无人值守 worker"定位（总报告第三轮）下最大的摩擦。

## 待决方向

沙箱探针成功、且这次调用确实在沙箱里执行时，workspace 档的 shell 由 `ask` 改 `allow`；`deny` 规则仍最先生效。命令因沙箱被拒（EPERM、只读文件系统、网络不通）时，结果里标明"沙箱拒绝"，模型可以带理由显式请求越界，这一次走 `ask`，绝不自动去沙箱外重跑。

做完之后：

- 无头 `radian exec --profile workspace` 不再需要 `--yes` 就能跑沙箱内的 shell；越界请求在无头下仍被拒（fail closed，不变）。
- subagent 子进程继承父的档位（0043 1a），自然得到同一条规则，不需要单独改；"把仍要问的 ask 转回父会话"是另一个事项（`subagent-ask-forwarding.md`）。
- workspace 档的 MCP 仍是 `ask`，无头下仍被拒；这条不动它（见 `mcp-full-tier-ask.md`）。

"本次会话允许"的授权从完整命令串放宽到前缀、加禁止清单、按项目持久化：这部分要受 `crates/radian-permissions/README.md` 里 shell 匹配器的限制约束（复合命令、`eval`、`source`、程序自身的选项都看不见），所以前缀授权只能对 README 定义的"简单命令"生效，复合命令仍然逐段判断或照旧 `ask`。

## 前提（必须先解决）

Linux bwrap 不限制 Unix socket（`docs/sandbox.md`：已实测、接受为已知限制，决定 4）。owner 必须先重开这一条（seccomp 堵上），否则不能做。

## 成本

中高，不是 4-architecture 估的"中"：权限层加一个依赖沙箱状态的谓词，沙箱层加失败分类，再加上 seccomp 前提和前缀授权的匹配器约束。

## 验收

workspace 档下 `cargo test` 不出现提示；写 `~/x` 被沙箱拒绝后结果带"沙箱拒绝"标记，再次请求时走 `ask`；探针失败时不放行（变异测试）；无头 `radian exec --profile workspace` 不带 `--yes` 能跑沙箱内 shell，越界请求被拒且结论写明；同一组任务在 workspace 档下统计提示次数和墙钟时间。
