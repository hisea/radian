# arc：可配置的 reviewer，只审 diff，问题分级，按级别返工

- 状态：decided（1–10 已定；不做的一项见末尾）
- 涉及 ADR：0048（gate 的 `review` stage、verdict、rework）
- 关联：`arc-flows.md` 第 2 步（per-profile 的 `review_exec_args` / `review_prompt_file` / `review_timeout_secs` 由本项的 reviewer profile 取代）

## 背景

`review` stage 已落地：一个只读的 `radian exec` 会话审 `base..sha`，输出 `summary`、`blocking`、`suggestions`、`verified`，`blocking` 非空就打回。实际跑下来：

- 每次返工后 review 都把整个 diff 重新审一遍，每轮只挖出一个之前漏掉的新问题。TK0017 连续五轮被打回（选择顺序 → help 文字 → 滚动），每个问题都是真的，但没有一个是那一轮返工引入的。
- 一轮最少 30–40 分钟（worker、commands、review 各一段），一个一行的修正也一样。
- `blocking` 只有是或否，用户没法说"小问题先记下，不要打回"。

## 决定（已定）

1. **reviewer profile。** 配置里加 `[gate.reviewers.<name>]`，每节一个 reviewer：
   - `exec_args`：模型等，含义同 `[worker] exec_args`；
   - `prompt_file`：审查范围与标准，写这个 reviewer 关心什么、不关心什么；
   - `timeout_secs`：一个 review 会话的时限；
   - `fix_level`：见第 4 条。

   一个 gate profile 用 `reviewer = "<name>"` 选 reviewer，缺省 `default`。`init` 写出的 `default` 节承接今天 `[gate.review]` 的三个键。

   第一轮之后不再追查旧问题，所以第一轮最重要。reviewer 可以给第一轮和返工轮分别设置：`first.exec_args`、`first.timeout_secs` 写了就覆盖第一轮的值，例如第一轮用更强的模型、给更长的时限；没写就两轮相同。
2. **只审 diff。** reviewer 拿到的是 task 自己的 diff（merge-base 到 head）和 brief。它可以读调用方来确认一个问题，但每条问题都必须落在 diff 改到的行上，或者是这些改动直接造成的。
   - 返工轮（同一个 task 的第 2 次及以后的 review）只给增量 diff，即上一次 review 过的 sha 到现在，再加上一次的问题列表。
   - 返工轮只判两件事：上一次的问题修好了没有；这次的改动有没有引入新问题。
   - 上一次就存在、但上一次没指出的问题，不能升级成打回的理由。照常列出，标 `pre_existing`，不计入返工门槛。
   - `pre_existing` 由 arc 机械判定，不由模型判断：一条问题的 `file:line` 不在这次增量 diff 改到的行里，就是 `pre_existing`。模型给出的级别可能偏高或偏低，「这一行这次改没改过」不会算错。
3. **一次列全。** prompt 要求把找到的问题全部列出来，不要找到一个就停；每条都要写触发条件和 `file:line`。
4. **三级问题，用户选修到哪一级。**
   - 输出：`findings` 列表，每条 `{severity, file, line, summary, trigger, pre_existing}`。`severity` 是闭集 `critical`、`major`、`minor`。
   - 级别的含义写在缺省的 prompt 文件里，每一级附两三个取自真实 review 的例子用来校准（只写抽象定义，模型给的级别不稳定）：
     - `critical`：安全、数据丢失或损坏、无法自愈的状态、挂起或泄漏；
     - `major`：结果错误、与 brief 或 ADR 矛盾、手册写了代码没做的事、测试失败或不可能失败；
     - `minor`：其余，比如少见的边界情况、措辞、命名、风格。
   - `fix_level`：闭集 `critical`、`major`、`minor`，缺省 `major`。高于或等于这一级、且不是 `pre_existing` 的问题让 stage 失败（reason `review_findings`）。低于这一级的只记录，不打回。
   - task 可以覆盖 `fix_level`：task 文件字段 `review_fix_level`，`add_task` 与 `radian arc add` 加同名参数。
5. **verdict。** `review.json` 记录全部 findings，以及当时生效的 `fix_level`。`show`、TUI 的 attempts 页和返工说明按级别列出。旧形状的 verdict（`blocking` / `suggestions`）照样能读：`blocking` 当 `major`，`suggestions` 当 `minor`。
6. **没修的问题变成待确认的 task。** 一个 task 合并时，如果还有没修的 findings（低于 `fix_level` 的和 `pre_existing` 的），arc 在同一个 plan 里提出一个收尾 task：`after` 原 task，brief 由 arc 按固定格式写成（逐条列出级别、`file:line`、问题、触发条件），一个 task 最多提出一个。这个 task 处于新的待确认状态，下面第 7 条定义。
7. **task 级的 propose / confirm。** 今天只有 plan 能 confirm；本项加 task 级的确认：
   - 新阶段 `todo.proposed`：调度循环和 `start` 都不领取它，`list`、`show`、TUI 里标成待确认。
   - 谁能提出：arc（第 6 条）；lead 用 MCP `add_task` 往一个已 confirm 的 plan 里加 task 时，也落成 `todo.proposed`（加到 draft plan 的 task 照旧随 plan 一起 confirm）。
   - owner 用 `radian arc confirm <TASK>` 确认，task 变成 `todo.ready`，所在 plan 不回到 draft，`auto` 不变；不要的用 `radian arc cancel <TASK>`（`todo` 本来就可以 cancel，进 `failed.cancelled`）。
   - TUI 的 merge 页加一个待确认 task 的列表（与 TK0018 的 owner 操作同一处）。
8. **review 与 commands 并行。** 两个 stage 同时开始；commands 失败时 review 的结果照常记录，但这一轮按 commands 的失败处理。一次 deepseek review 约 0.1–0.2 美元，白跑一次的代价远小于每轮省下的 10–15 分钟。
9. **review 失败后 arc 自动写返工。** 达到 `fix_level` 的 findings 由 arc 按固定格式写成 `## Rework <n> (review)` 并回到 `todo.ready`，不等 lead。它计入 `auto_rework`，到 `max_auto_rework` 照旧暂停 plan。lead 仍可在 `review.rework_pending` 时用 `add_rework` 手写（`auto` 为 `off` 的 plan 就走这条）。
10. **级别校准是已知风险。** 级别由模型给出，会有偏差；靠三件事压住：第 4 条的例子、第 2 条机械判定的 `pre_existing`（最影响返工轮数的一项不交给模型），以及 verdict 里留全部 findings，偏差能在 TUI 和 `show` 里被看到、拿来改 prompt。

## 不做（现在）

- 一个 gate profile 挂多个 reviewer（例如安全 reviewer 加通用 reviewer），各自有 `fix_level`：等 flows 落地、真有需要时再加。

## 验收

- 两个 reviewer profile 用不同的模型和 prompt；gate profile 选哪个就用哪个。
- 返工轮的 prompt 只含增量 diff 和上一次的问题；一个只在旧代码里的问题被标成 `pre_existing`，不让 stage 失败。
- `fix_level = major` 时只有 `minor` 的结果算通过；`fix_level = minor` 时同样的结果算失败；task 的 `review_fix_level` 覆盖 profile 的值。
- 旧形状的 verdict 仍能被 `show` 和 TUI 读出。
- 有没修的 findings 时，合并后同一 plan 里多一个 `todo.proposed` 的收尾 task；循环不领取它；`confirm <TASK>` 后它变成 `todo.ready`，plan 状态和 `auto` 不变；`cancel` 后进 `failed.cancelled`。
- lead 往已 confirm 的 plan 里 `add_task` 得到 `todo.proposed`。
- review 与 commands 同时开始；review 失败时 task 带着 arc 写的返工回到 `todo.ready`，`auto_rework` 加一。
- 手册写全新的键：`gate.reviewers.<name>.*`、`gate.profiles.<name>.reviewer`、task 字段 `review_fix_level`，每个都写类型、缺省、全部允许值、何时生效。

## 已落地

第 6、7 条（没修的问题变成 `todo.proposed` 的收尾 task；task 级 propose / confirm）已实现；第 8 条（review 与 commands 并行）已实现。第一轮专用的 `first.exec_args` / `first.timeout_secs` 暂时落在 `[gate.review.first]` 上，键名与 reviewer profile 相同；reviewer profile 落地时移入 `[gate.reviewers.<name>]`。
