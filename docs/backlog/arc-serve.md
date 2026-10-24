# arc：serve 接口、通知钩子与远程客户端（iOS / desktop）

- 状态：idea
- 涉及 ADR：0048（调度循环与 `runner.lock`、owner 动作、v0 排除项里的 daemon）、0013（监听口与出站推送都是新的网络面）、0032（预留的 `serve` 子命令）、0034（上报内容的隐私口径）、0006 与 0028（会话 journal 投影成 block）、0045（钩子命令的形状）、0043
- 来源：`docs/backlog/arc.md` 里「v1 要 desktop / iOS / web app」一条；`research/2026-10-22-arc-app-product-design.md`

## 背景

arc 的全部状态是 `<repo>/.radian/arc/` 下的文件，单写者，没有常驻进程，没有监听口。今天只有三个本机入口：CLI、`radian arc mcp`（lead 用，刻意不含 confirm / start / merge / cancel / auto）、正在做的 arc TUI。一部手机想做下面四件事，今天只能 ssh 进来跑 CLI：

- 浏览 plan 与 task（含 state、phase、gate 结论、diff）；
- confirm、cancel、调 plan 的 `auto`；
- 看 worker 会话与 review 会话（两者都是 radian exec 的 journal，session id 记在 attempt 的 `execution.json`）；
- 锁屏时收到 `review.awaiting_human`、`failed` 的通知。

## 方向（未定）

1. **runner 抽库。** 把调度 pass、事件广播、serve 做成 radian-arc 里的一个模块；`radian arc run --watch`、arc TUI、desktop app 是它的三个壳，谁跑它谁持有 `runner.lock`。ADR 0048 里「TUI 与 `run` 用同一状态机、同一组文件」不变。
2. **serve 只听 loopback，走 HTTP + SSE。** 读接口返回与 `list --json` / `show --json` 相同的结构；写接口只有 owner 动作（confirm、cancel、auto，后续可加 merge / reject）；事件流一行一个状态迁移；会话接口把 journal 投影成 TUI 用的 block 列表再发出，正在跑的会话续推新 block，客户端不解析 journal 格式。暴露到局域网外由 Tailscale（`tailscale serve`）或 ssh 端口转发完成，radian 不管。可选 bearer token 作第二道锁。
3. **通知钩子。** 状态迁移时执行配置里的一条命令（形如 `[hooks] on_transition`），传 task id、旧 phase、新 phase，与 `arc-landing.md` 的 `after_merge` / `after_fail` 同一族。锁屏通知只能经 APNs：钩子命令自己调 APNs，或推给自托管的 Bark / ntfy。payload 只带 id 与 phase，正文由客户端连 serve 取回（ADR 0034 的口径）。
4. **客户端。** iOS 永远是 client；desktop app 可 host（本机跑循环加 serve，持有 `runner.lock`，有任务在跑时持防睡眠断言）也可 client（循环在别处）。UI 只面向一个接口，两个实现：进程内直调与 HTTP。应用设计文档里「app 打开即持有 `runner.lock`」只对 desktop 成立。

## 待决问题

- serve 合进 `run --watch`（`--serve <port>`）还是独立子命令；若独立，用 ADR 0032 预留的 `serve` 名还是 `arc serve`。
- token 是必须项还是可选项；`tailscale serve` 的身份头要不要认。
- 菜单栏常驻的 desktop app 是否算 ADR 0048 v0 排除的「resident daemon」；落地时 ADR 要明说。
- APNs 直发是否进 radian（desktop app 持 `.p8` 密钥）：是一个新的出站网络面，默认关闭。
- 与远程 worker 的 pull 模式共用 serve 与否：claim 语义比手机端需求重得多，现在只做手机这一半，接口留余地即可。
- 接口版本：复用 `schema_version` 冻结的四组字段，还是单独给 serve 一个版本号。

## 验收

未定。至少：手机在 tailnet 内不装 ssh 客户端也能完成上面四件事；serve 进程不在时锁屏通知仍能送达；`run --watch`、TUI、desktop host 三个壳对同一组文件产生相同的状态迁移。
