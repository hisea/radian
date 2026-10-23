# arc：可配置的 reviewer，只审 diff，问题分级，按级别返工

- 状态：decided（1–6 已定；「待决」一节未定）
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
2. **只审 diff。** reviewer 拿到的是 task 自己的 diff（merge-base 到 head）和 brief。它可以读调用方来确认一个问题，但每条问题都必须落在 diff 改到的行上，或者是这些改动直接造成的。
   - 返工轮（同一个 task 的第 2 次及以后的 review）只给增量 diff，即上一次 review 过的 sha 到现在，再加上一次的问题列表。
   - 返工轮只判两件事：上一次的问题修好了没有；这次的改动有没有引入新问题。
   - 上一次就存在、但上一次没指出的问题，不能升级成打回的理由。照常列出，标 `pre_existing`，不计入返工门槛。
3. **一次列全。** prompt 要求把找到的问题全部列出来，不要找到一个就停；每条都要写触发条件和 `file:line`。
4. **三级问题，用户选修到哪一级。**
   - 输出：`findings` 列表，每条 `{severity, file, line, summary, trigger, pre_existing}`。`severity` 是闭集 `critical`、`major`、`minor`。
   - 级别的含义写在缺省的 prompt 文件里：
     - `critical`：安全、数据丢失或损坏、无法自愈的状态、挂起或泄漏；
     - `major`：结果错误、与 brief 或 ADR 矛盾、手册写了代码没做的事、测试失败或不可能失败；
     - `minor`：其余，比如少见的边界情况、措辞、命名、风格。
   - `fix_level`：闭集 `critical`、`major`、`minor`，缺省 `major`。高于或等于这一级、且不是 `pre_existing` 的问题让 stage 失败（reason `review_findings`）。低于这一级的只记录，不打回。
   - task 可以覆盖 `fix_level`：task 文件字段 `review_fix_level`，`add_task` 与 `radian arc add` 加同名参数。
5. **verdict。** `review.json` 记录全部 findings，以及当时生效的 `fix_level`。`show`、TUI 的 attempts 页和返工说明按级别列出。旧形状的 verdict（`blocking` / `suggestions`）照样能读：`blocking` 当 `major`，`suggestions` 当 `minor`。
6. **没修的问题不丢。** 一个 task 合并时，没修的 findings（低于 `fix_level` 的和 `pre_existing` 的）汇总进 `runs/<task>/open-findings.json`。lead 据此开收尾 task；arc 不自动开。

## 待决

- `review` 和 `commands` 是否并行跑：每轮能省 10–15 分钟；commands 失败时那一轮 review 就白跑了。
- review 失败后，是否由 arc 按 findings 自动写返工说明，不等 lead（同 PL0022 的冲突返工那样）。
- 一个 gate profile 能不能挂多个 reviewer（例如安全 reviewer 加通用 reviewer），各自有自己的 `fix_level`。

## 验收

- 两个 reviewer profile 用不同的模型和 prompt；gate profile 选哪个就用哪个。
- 返工轮的 prompt 只含增量 diff 和上一次的问题；一个只在旧代码里的问题被标成 `pre_existing`，不让 stage 失败。
- `fix_level = major` 时只有 `minor` 的结果算通过；`fix_level = minor` 时同样的结果算失败；task 的 `review_fix_level` 覆盖 profile 的值。
- 旧形状的 verdict 仍能被 `show` 和 TUI 读出。
- 手册写全新的键：`gate.reviewers.<name>.*`、`gate.profiles.<name>.reviewer`、task 字段 `review_fix_level`，每个都写类型、缺省、全部允许值、何时生效。
