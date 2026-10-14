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

## 下一步（按 model-switch.md 顺序）

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
