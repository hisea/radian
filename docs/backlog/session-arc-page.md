# 会话 TUI 里的 `/arc`：不离开会话看 arc

- 状态：idea
- 涉及 ADR：0048（arc TUI 的页面与 owner 动作、lead 只走 `radian arc mcp`）、0023（Modal / Overlay / Surface 分层、`app.rs` 不读文件）、0027（斜杠命令）、0035（视觉语言）、0009（全屏 TUI，不冲突，不改）
- 来源：owner 提问「能不能不关闭 radian，用同一个 TUI 显示当前项目目录里的 arc 数据」

## 背景

今天看 arc 要另开终端跑 `radian arc`，或者退出会话。arc 的 plans、task、merge 三个页面已经在 radian 二进制里（`radian-tui` 的 `arc` 模块）：界面状态和画法（`ArcApp`、`arc::render`）不碰终端，终端与输入循环在 `cli/arc.rs`，和 attach 视图同样的分法。所以换一个宿主就能把这些页面放进会话 TUI。

## 方向（未定）

1. **`/arc` 切换 Surface。** arc 页面作为 ADR 0023 的一个 Surface，与「对话记录 + 输入框」并列；`/arc` 切过去，Esc 或 `q` 切回。会话照常运行，模型的流式输出、工具、后台命令都不停。
2. **数据是当前项目目录的 `.radian/arc/`**，与 `radian arc` 读同一组文件、同一套读接口（`radian_arc::read`、store 的加载函数）。没有 arc 时 `/arc` 只回一行，说法与 `radian arc` 未初始化时一致（指向 `radian arc init`）。
3. **分层不变。** 看 arc 页面时，会话里的审批、结构化提问仍是 Modal，压在 arc 页面之上；arc 自己的浮层（确认、日志）是 Overlay，绝不压过 Modal。
4. **owner 动作照旧。** 在 `/arc` 里按 confirm、start、merge、reject 等，是 owner 在操作，走与 `radian arc` TUI 相同的库调用；同样不持有 `runner.lock`，只探测它。模型一侧（工具、MCP、钩子、斜杠命令的模型可调形式）没有任何路径能触发这些动作。
5. **读文件在后台。** `app.rs` 不读文件（`tests/purity.rs`），arc 的加载与日志读取放到后台线程，结果作为输入送回界面，与现在 `cli/arc.rs` 的做法相同。

## 待决问题

- 刷新：会话 TUI 里 arc 页面按什么节奏重读文件（定时、只在显示时、还是监听文件变化）；不显示时是否完全不读。
- 回到对话时是否记住 arc 页面的位置（选中的 plan、task、tab）。
- 当前目录是 `.radian-worktrees/` 下的 worktree 时，读 worktree 自己的 `.radian/arc/` 还是主 checkout 的。
- footer 要不要显示 arc 摘要（例如 running / review 的数量），还是只在 `/arc` 里看。
- `radian arc` 独立 TUI 是否保留：建议保留，两个宿主共用同一组页面。

## 验收

未定。至少：在会话里 `/arc` 打开、Esc 返回，期间正在进行的回合不中断、输出不丢；arc 页面打开时出现的审批弹窗在最上层且可处理；`/arc` 的按键与 `radian arc` 相同并写进用户手册；一条测试证明模型一侧调不到 owner 动作；ADR 0048 与 0023 在实现分支上改成新的快照。
