# radian 桌面 app：内置 radian 与 arc，一个 hub 看所有仓库（2026-10-29）

状态：产品与进程模型的研究稿，idea，不开工。本文在 `2026-10-22-arc-app-product-design.md`
（三个界面、plan 层、全自动）之上回答三个问题：hub 怎么支持多个仓库；hub 是独立 UI app 还是也要
CLI；app 怎么"内置" radian 与 arc。控制面的字段、锁、attempt 对账、gate 隔离不在本文重写，只引用
ADR 0048。桌面 app 的 UI 框架选择（gpui spike）不在本文。

决定在力的 ADR：0013（默认无遥测、无出站连接）、0016（层级与 macOS 的 sandbox-exec）、0020
（`~/.radian/` 布局）、0031（项目信任）、0032（一个二进制）、0043（编排在进程边界）、0048（arc）。
相关 backlog：`docs/backlog/arc-serve.md`（hub 的 HTTP 接口与通知钩子）、`docs/backlog/arc-ios.md`。

## 一句话

一个桌面 app，链接 radian 的 crate 做 UI，bundle 里带一份 `radian` 二进制给所有子进程用，
数据仍在 `~/.radian/` 和各仓库的 `.radian/arc/`；app、终端里的 `radian`、iOS 看到的是同一组状态。

## 1. hub 支持多个仓库：已定的一半与要补的一半

`arc-serve.md` 第 1 条已经定了 hub 的形态：一个进程管本机登记过的所有仓库，登记是显式列表，不自动
发现；读是直接读各仓库的 arc 文件，返回与 `list --json` / `show --json` 相同的结构；写是 owner
动作，调 CLI 的同一组库函数，只拿该仓库的 `state.lock`。那条决定原来还说 hub 不跑调度循环，本文
把它改掉（见下一条，`arc-serve.md` 同步改）。

要补的：

- **多仓库读取做成库函数，所有前端共用。** 登记表的读写、逐仓库的 store 读取放进 `radian-arc`。
  桌面 app（进程内调用）、`radian hub`（HTTP）、iOS（经 hub）走同一组函数。
- **hub 就是登记仓库的调度循环。** `radian hub` 对每个登记的仓库各拿一把 `runner.lock`，在自己的
  tokio task 里跑该仓库的 pass，与 `radian arc serve` 调同一个库函数 `run_pass(repo)`。ADR 0048 §13
  的单写者规则是每仓库一把锁、持有者是唯一写者，它没有说一个进程只能持有一把；仓库之间的状态文件
  本来就分开。某仓库已有 `arc serve` 持锁时，hub 拿不到锁就只读它、不调度它，并在接口里如实报告，
  这是今天"第二个 run 拿不到锁就退出"的行为。`radian arc serve` 保留为单仓库形态，给没有 hub 的
  用户与 TUI（TUI 打开时没锁就起一个 `arc serve` 的规则不变）。
- **合并的收益。** 一处知道所有仓库，才能有全机的 worker 上限 `[hub] concurrency`（N 个 `arc serve`
  互不知道，三个仓库就是三份 `[limits] concurrency` 相加）；无头的机器只需一个 launchd 项；
  没有第二个 `serve` 子命令。
- **合并的代价与兜法。** 故障隔离：每个仓库的 pass 在自己的 task 里，错误记进该仓库的 `loop.log`，
  HTTP 继续服务；gate 与 worker 本来是子进程。二进制被替换时的重启（ADR 0048 验收项）会断一次 SSE，
  `arc-serve.md` 已定事件不 durable、客户端重连先重读列表，吃得下。网络面的权限不变：接口本来就能
  merge，token 泄露的后果合不合并一样。
- **会话也进来。** `~/.radian/sessions/` 是全局的，每个会话记着 workspace；`radian attach` 是只读
  视图。hub 的仓库行下可以列该仓库正在跑的 radian 会话并只读 attach。
- **不做 TUI 的 repos 页。** `radian arc` 保持单仓库 TUI；多仓库视图只在桌面 app 与 iOS。

## 2. hub 本体是服务，不是 UI；CLI 省不掉

三个前端只能有一个数据源，iOS 已定只连 hub 的 HTTP 接口，所以 hub 的全部逻辑（登记表、多仓库读取、
调度循环、owner 动作、SSE、token）在 radian 二进制里；桌面 app 和 iOS 一样是它的前端。

最少的 CLI，每条都有 UI 替代不了的原因：

| 命令 | 为什么必须是 CLI |
|---|---|
| `radian hub` | 无参数就是起服务（调度循环 + HTTP），与 `radian arc` 无参数就是主形态的习惯一致；无头跑、`tailscale serve` 或 ssh 转发时前面没有 UI；集成测试靠它起一个 hub |
| `radian hub add <repo>` / `remove` / `list` | 登记表是显式列表，脚本与测试要有入口 |
| `radian hub pair` | 打印地址 + token 的二维码给 iOS 扫 |
| `radian hub token rotate` | 换 token；token 存 `~/.radian/` 下 sandbox 不可读的位置（ADR 0020） |

命名：hub 同时看会话与 arc，位于 arc 之上，所以是 `radian hub ...`，不是 `radian arc hub`；
`radian arc` 保留为单仓库 TUI 与 CLI。要改 ADR 0032 的子命令表。

规则：**app 能做的，CLI 都要能做。** 这是手册（AGENTS.md "每个改变行为的输入都写进手册"）与
测试的前提；app 不能有只在 GUI 里的功能。

## 3. "内置"拆成库与进程两件事

radian 与 arc 有两种运行形态，app 对两种各有一种内置方式。

**库：在 app 进程内调用。** TUI 今天就是这样（ADR 0048 §11：owner 动作是库调用不是子进程，只拿
`state.lock`）。app 链接 `radian-arc`、`radian-host` 等 crate：读登记表与各仓库的 arc 文件、做
confirm / merge / reject、读 journal 做只读 attach。本机不经 HTTP。`radian hub` 的 HTTP 服务仍然
存在，给 iOS 用，两边是同一组库函数。

**进程：bundle 里带一份 `radian` 二进制。** 这一份省不掉，因为编排全在进程边界（ADR 0043）：
worker 是独立的 `radian acp` 进程，gate 的 review 是 `radian exec`，调度循环是 `radian hub`（或
单仓库的 `radian arc serve`），`subagent` 工具再起子进程。app 把 bundle 内的路径交给库，并装一个命令行入口（一个指向 bundle 的
symlink，像 VS Code 的 `code`），终端里的 `radian` 与 app 是同一个二进制、同一个版本。
Claude Code 不打包，只检测 PATH。

**数据：`~/.radian/` 仍是唯一的家。** config、sessions、credentials、trust 都在那里（ADR 0020），
app 与终端里的 radian 看到同一组会话、同一份凭据，没有第二份存储。arc 的状态仍在各仓库的
`.radian/arc/`。

## 4. 进程模型

```
desktop app (gpui, links radian crates)
  ├─ embedded terminal pane ── lead: Claude Code or radian, with `radian arc mcp` registered
  └─ radian hub (detached; one per machine)
        ├─ HTTP + SSE for iOS; same library as the app's own reads
        └─ per registered repo ── one tokio task, holds that repo's runner.lock, runs run_pass(repo)
              ├─ radian acp            one worker per task attempt, in its worktree
              └─ gate stages           commands / mutation / review (radian exec) / integration

radian arc serve (single-repo form, for a repo without a hub; the TUI starts it when nothing holds the lock)
```

- `radian hub` 是 detached 进程：app 退出后继续跑，再打开时靠登记表与各仓库的 `runner.lock` 重新接上。
  app 打开时本机没有 hub 就起一个，这是今天 `radian arc` TUI 对 `arc serve` 的做法。
- 一个仓库同时只有一个持锁者。hub 与 `arc serve` 抢到谁就谁跑，另一个只读；接口的仓库列表报告持锁的是
  谁（hub、某个 pid、还是没有）。
- lead 跑在 app 内嵌的终端 pane 里。这是桌面 app 比 TUI 多的那一块：ADR 0048 的约束"arc TUI 不
  内嵌终端"只限 TUI，桌面 app 的意义之一就是把 lead 的终端放进来（见 `2026-10-22-arc-app-product-design.md`
  第一个界面）。
- app 自己不发任何出站连接（ADR 0013）。hub 的监听口只在用户启动时存在，默认只绑 loopback
  （`arc-serve.md` 第 2 条）。

## 5. 落地前要核的事实

这些是约束，不是选择；每条在真机上核一遍再定分发方式。

- **macOS 分发走 Developer ID + notarization，不上 App Store。** `workspace` 层在 macOS 用
  sandbox-exec 起子进程（ADR 0016）；App Sandbox entitlement 下的 app 不能这样起进程。bundle 内的
  `radian` 要用同一身份签名并开 hardened runtime，否则 Gatekeeper 在第一次 spawn 时拦下。
  未核：hardened runtime 的哪些 entitlement 是 sandbox-exec 子进程需要的。
- **Linux 需要 user namespaces** 给 bwrap；AppImage 与 deb 都可以带二进制。未核：AppImage 的
  FUSE 挂载点在 bwrap 的只读 bind 列表里怎么处理。
- **升级即替换二进制。** `radian arc serve` 已处理"磁盘上的二进制被替换就重启，正在跑的 gate 不受
  影响"（ADR 0048 验收项），`radian hub` 沿用同一规则，重启会断一次 SSE；正在跑的 `radian acp` worker 持有旧映像，不受影响。app 更新要原子替换
  整个 bundle 目录，不原地覆盖文件。未核：macOS 上替换目录时 symlink 入口是否要重建。
- **自动更新是一个新的出站连接**，按 ADR 0013 默认关、显式开，与 hub 的监听口同一口径。
- **登记的仓库是否必须是 trusted（ADR 0031）。** 循环会在那里起 worker、读 `.mcp.json` 与
  hooks.json；倾向必须，`radian hub add` 对未信任的仓库拒绝并指向信任命令。

## 6. 对 ADR 的影响

| ADR | 改什么 |
|---|---|
| 0032 | 子命令表加 `radian hub ...`；"一个二进制"加一句：桌面 app 是前端，bundle 内带这一个二进制，不是第二个 radian |
| 0048 | §13 把"每仓库一个 `serve` 进程"改为"每仓库一把锁，持有者是 `arc serve` 或 `hub`"；hub 的决定（登记表、多仓库读取、`[hub] concurrency`）；"arc TUI 不内嵌终端"改为只限 TUI |
| 0020 | 登记表与 hub token 的文件位置进目录表（`arc-serve.md` 留了两个选项） |
| 0013 | hub 监听口与更新检查两个新的网络面，都是用户显式开 |
| 0016 / 0031 | 不改决定；分发与信任的约束引用它们 |

## 7. 顺序

1. hub 库函数 + 这几条 CLI（登记表、多仓库读取、多仓库循环、`[hub] concurrency`）。
2. `radian hub` 的 HTTP + SSE（`arc-serve.md` 原文），iOS 可从这里并行。
3. gpui spike：内嵌终端是核心问题（三个不兼容的 gpui 分支，终端适配器都在 alacritty_terminal 上），
   先做它再定 UI 框架。
4. 桌面 app：链接 crate，bundle 带 `radian`，签名与分发按第 5 节核过的结果。

## 待决

- 桌面 app 的代码放这个仓库还是单独仓库：gpui 的构建与签名流程和 Rust 工作区的 Verify 不同，
  与 `arc-ios.md` 的同一个问题一起定。
- 登记表的位置（`~/.radian/config/config.toml` 的一节，还是 `~/.radian/` 下单独一个文件）。
- app 关闭时是否提供"停止 hub"，还是只保留 detached 的默认。
- `[hub] concurrency` 与每仓库 `[limits] concurrency` 的关系：两个都要有空位才认领（与 flow 的双上限同一规则），还是全机上限覆盖仓库上限。
- 内嵌终端里的 lead 默认是 Claude Code 还是 radian：`2026-10-22-arc-app-product-design.md`
  定的是 Claude Code 为默认，终端交互时 radian 可用；本文不改。
