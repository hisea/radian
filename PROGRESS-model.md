# PROGRESS-model — ADR 0026 slices 2+3: `/model` switch, background refresh, `/thinking` (D11)

Branch `pi/model`, base `efddc11` + merge main (`170eddc`).
**This file is not committed**; it is the continuation record.

## 已完成（已提交，已核实）

- `fc4c0c1` — **第 1 步 D7**：`TurnOptions.context_budget`（`None` = 配置不变）；
  `AgentSession::compact_now` 增加同名参数（`/model` 确认压缩目标 B'）；turn 内的预算读取
  统一走 `ActiveRun::context_budget()`；`crates/agent-core/README.md` 写明加法改动理由；
  测试 `the_turn_budget_overrides_the_configured_one`、`compact_now_honours_the_budget_it_is_given`。
- `0c8452f` — **第 2 步 D5/D7/D8**：`radian-runtime` 增 `ModelRoute`（`{"model":{provider,model}}`，
  只含路由）、`BudgetFor` 闭包类型、`Driver::open_with_slot`、resume 重放最后一条
  `ConfigChanged`（`Recovery.last_change` → `apply_change` → slot + 重算 B）；
  模型变更时 driver 用宿主闭包从**本地目录缓存**重算 B（`radian_host::resolve_offline`，不联网），
  在 `run_turn` 里经 `options.context_budget` 交给下一轮。
  `radian-host` 增 `SwitchableModel`（按 slot 当前路由取/建/缓存 `HostModel`，key 每路由从
  `auth.json` 解析，缺凭据报 `/login <provider>`）、`RouteContext`；两个宿主（agent-cli、radian-tui）
  都已接线：共享 `ConfigSlot` + 传入 `budget_for`。
  `radian-host/tests/session.rs` 的两处 SessionSetup 已补字段。
- `3dcafeb`/`1525ce6` — 类型整理（`BudgetFor` 导出、clippy）。

## 已核实

- `radian-runtime` 的 `ConfigQueue`/`ConfigSlot` 机制与 driver 的 `apply_pending_config`（turn 边界）。
- `compact_now` 调用方：driver `run_control`（经 `ControlRequest.budget`）。
- 模型列表来源：描述符内建列表 + `Catalog::models_for`/`display_name`（models.dev 缓存）。
- facade（`LocalSession`）目前**不持有** ConfigQueue（driver 自建）——第 3 步要先把队列共享出来。

## 事故（2026-10-14）：D11 的 thinking 片被我自己弄丢了

我在修 clippy 时跑了 `git reset --hard HEAD~1`（撤掉了当时**未提交**的 D11 提交内容），
之后的重做只补了 clippy 那两行，D11 的 thinking 机制从未真正落盘；随后变异脚本里的
`git checkout -- <file>` 又把 app.rs/command.rs/provider.rs/model.rs 里未提交的 thinking
改动还原成了 HEAD。**结果：D11（七档级别、夹取、thinkingLevelMap、reasoning_effort、
/thinking）不在分支上**。已核实：`grep THINKING_LEVELS provider.rs` = 0、
`reasoning_effort` = 0、`Outcome::Thinking` = 0；而第 1–6 步（SwitchableModel、ModelRoute、
queue_config、resumed_model_route、driver 测试）都在。

教训：变异与还原只用 `git checkout --` 作用在**已提交**的文件上，或先把改动提交；绝不在
未提交的工作上跑 `git reset --hard`。

## 下一步（按 model-switch.md 顺序）

0. **已加提交**：`adf61eb`(第3/4步) `af3b71f`(取消跟踪本文件) `d321916`(第5步后台刷新)
   `2444a0c`(第6步跨provider) `cfacb1e`(D11) `efa6afc`(driver测试) `b19d20c`?(resume回退+sink)
   —— 以 git log 为准。
1. **第 3+4 步已完成（`adf61eb`）**：/model 命令树（两级补全，provider 来自 auth.json 可用条目，
   model 来自描述符内建列表）；loop 审核（凭据 D6、B' 经 budget_for_route 离线解析、T 取
   projection 的 budget report）；T≤B' 直接排队并提示；T>B' 弹硬编码 y/n 确认 Modal，y 走
   compact_with_budget(B') 成功后才排队，n/失败不排队；状态栏 next: <provider/model>，
   ConfigChanged 落地即清；D8 resume：显式 --model/--provider 与记录不同时排队覆盖并写
   ConfigChanged（两个宿主）。facade 增 queue_config/compact_with_budget/model_route/
   budget_for_route；Driver::open_with_slot 增队列参数。
   测试：`tests/model_switch.rs`（10 个 update 纯测试：两级补全、直接排队、超预算先问、y 先压缩、
   n 不切、压缩失败不切、未知 B' 直接排、next 标记清除、硬编码键、部分目标是用法提示）。
2. **待做**：

   - `command.rs`：`/model` 挂树，`model_route_level`（`<provider>/<model>` 一词两级补全，
     `Level::Needs(Fetch::ModelRoute)`）；`Outcome::Model(ModelSwitch)`；HELP 行。
   - `app.rs`：`Effect::ModelSwitch`；`Input::ModelChecked{provider,model,tokens,budget}` →
     T≤B' 直接 `Effect::QueueModel` + 一行提示 + 状态栏 `next: <model>`；T>B' 弹确认 Modal
     （复用硬编码 y/n 的 ConfirmCommand，`pending_switch` 记住目标）；
     `y` → `Effect::CompactForSwitch{provider,model,budget}`；压缩成功 → 排队切换，失败/取消 → 不切换并给原因；
     `Input::Runtime(ConfigChanged)` 清掉 `next_model`。
   - `main.rs` 循环：Effect 处理（凭据检查 `auth::resolve_key`、B' 用 `resolve_offline`、
     T 用 projection 的 budget report、压缩走 `compact_with`）。
   - 队列只含路由（D5），key/header 永不进入。
2. **第 4 步 resume D8**：宿主在 open 后比较 `switchable_model.current_route()` 与 CLI 的
   `--model`/`--provider`，不同则排队一条 `ConfigChanged`（命令行为准）。
3. **第 5 步 D0 后台刷新**：有 key 时后台 GET `/v1/models`（OpenAI `{"data":[{"id":…}]}` 形状，
   **未核实 opencode 的形状**——实现按此并写进报告）；缓存 `~/.radian/cache/providers/<id>.json`
   只存 key 指纹；TTL 1h，失败 5 分钟重试；不阻塞启动；/model 候选 = 内建 + 缓存 + models.dev 合并。
4. **第 6 步 D6 跨 provider**：两个适配器各自的请求构造测试加"上一轮来自另一家"用例；
   Anthropic 非数组 `provider_payload` 行为钉住（不改行为，报告里给建议）。
5. **D11 `/thinking`**：七档 `off|minimal|low|medium|high|xhigh|max`，默认 medium；
   模型层 `thinkingLevelMap` > provider 层映射 > 都没有则不发参数；OpenAI 发 `reasoning_effort`，
   Anthropic 映射 adaptive/enabled；夹取=向上取最近支持级别、上面没有再向下（deepseek-v4.1-flash:
   medium→high, minimal→low, xhigh→max, off→low）；`/thinking` 补全只列支持的级别，手输不支持则
   夹取并提示；opencode-go 内建列表更新到 `~/.pi/agent/git/github.com/monotykamary/pi-opencode-go-provider/models.json`
   (6837adb)；`ConfigChanged {"thinking": …}`，resume 恢复，`--thinking` 命令行为准；旧 Anthropic
   写法一个版本内接受并警告。
6. 测试（update/driver/pty）、变异、Verify 全套 + pty 3×、通知。

## 变异清单（待做）

立即切换（不等下一轮）；ConfigChanged 写入 key；T>B' 不确认直接切换；压缩失败仍切换；
resume 忽略 ConfigChanged；预算不随模型更新；（D11）夹取改向下取；不支持的级别仍显示；无映射表仍发参数。

## D11 重做（2026-10-14）提交清单

- `a7a7597` piece 1：七档级别 + thinkingLevelMap + 夹取 + 补全过滤 + 解析（radian-host）
- `db5327f` piece 2+3：`resolve_thinking`、`ModelArgs.reasoning_effort`、OpenAI 适配器 `reasoning_effort`
- `e4edb2f` piece 4：SwitchableModel 缓存键含 thinking、`current_thinking`
- `60b22bb` piece 5：`--thinking` 七档（两个宿主），旧写法警告
- `40e7e18` piece 6：`/thinking` 命令 + 补全 + clamp 提示 + 状态栏
- `f831881` piece 7：recover 合并**全部** ConfigChanged（审查 M3）
- `b474256` piece 8：thinking 补全按 partial 排序；空列表不再吞掉 Enter
- `442f55a` piece 9：无映射表不发参数；未知 wire 值不发
- `e03616d` 变异记录（T1–T3、C1）+ M4 取消跟踪本文件
- `5aa7193` 审查 M2：`set_budget_for` 立即为已恢复路由重算 B
- `35edc75` 旧 `enabled:<budget>` 测试改为"接受并夹取"
- 变异（一次性 worktree）：T1 夹取改向下、T2 列不支持级别、T3 无映射表仍发参数、C1 按种类各留最后一条 —— 4/4 抓住。

## 未做（明确）

- **审查 M1**（valueless `--resume` / `--pick` 用 config 的模型与预算）：需要把
  模型/provider/凭据/路由/window/budget 的解析**整体移到 picker 之后**（picker 才定下 id）。
  本片试过局部刷新，但 `credential`/`descriptor`/`route`/`window` 都在 picker 之前算好，
  局部补不全，已回退以保持分支绿。留给下一片。
