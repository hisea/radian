# 删掉进程控制端口

- 状态：decided
- 涉及 ADR：0043、0011、0020、0008、0013、0049
- 来源：2026-11-21 精简调查

## 背景

每个 radian 进程（TUI 和 `radian exec`）默认在 `$RADIAN_HOME/run/<pid>.sock` 开一个 JSON-lines 控制端口（`docs/user/control.md`）。它原本的用户是 hub 和外部编排；hub 已删，operad 只用 `radian exec --output jsonl`（以后可能用 `radian acp`），不连这个端口。`radian attach` 读的是 journal，不走端口（`control.md` 说 attach 用它，这句是错的）。

## 决定（已定）

- 删掉控制端口：`radian-host` 的 `control.rs` 及其测试、TUI 和 exec 里绑定端口的代码、启动时的 `[control]` 行、`docs/user/control.md`。
- `[control]` 配置节按"已退役的键"处理：读到时给一行迁移警告并忽略，绝不拒绝配置文件（同 ADR 0008 对退役键的做法），并在 `configuration.md` 写明它是 inert。
- ADR 0043 删掉"`control.rs` 保留"的条款和验收里的 control 项；ADR 0011 里只由端口产生的输入来源（`Scheduler`，以及客户端声明的 `Subagent`/`Mcp`）若再无别的产生者，同一分支确认后从闭集里去掉或标为只读旧值，journal 里的旧值仍能读（ADR 0006 未知枚举值规则）。
- `observer.rs`（TUI follow 和 attach 用）保留。

## 落地前要核对

派发时先确认没有内部用户：`/goal`、herdr 上报、subagent 父子进程都不经过端口。若有，改用进程内通道，不保留端口。

## 验收

TUI 和 `radian exec` 启动后 `$RADIAN_HOME/run/` 下不出现 socket；带 `[control] enabled = false` 的旧配置照常启动并只多一行警告；`radian attach --follow` 照常工作；文档和 docs 漂移测试里没有 control 端口。
