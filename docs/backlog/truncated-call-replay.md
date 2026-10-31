# 被截断的工具调用原样回放，本地线路返回 500；以及只知道窗口时不发 max_tokens

- 状态：idea（bug，两个决定待定）
- 涉及 ADR：0017（截断的响应继续 turn）、0018（`max_tokens` 只在 C 和 O 都已知时发送）、0002（工具调用的执行标记与恢复）
- 来源：2026-10-31 本地线路重跑第一次运行（Hai-PC，LM Studio `qwen/qwen3-coder-30b`，`--context-window 32768`，radian 6b94155be），journal 在 Hai-PC `~/.cache/radian-repo-tasks/runs-2026-10-31-local/local-qwen-32k/TK0220/1/session/`

## 现象

1. 模型生成了一个退化的 `patch` 调用，输出 11,021 token，把 32,768 的窗口填满，被服务端截断（LM Studio 日志 `truncated = 1`）。radian 按 ADR 0017 没有执行它，工具结果 `is_error`、`error_code = "truncated"`。这一步是对的。
2. assistant 消息里 `tool_calls[0].arguments` 存的是截断处的原始字串（`{"path":"<parameter=edits>\n[{'new': …`，不是合法 JSON）。压缩把这个原子单元留在了保留部分，之后每次请求都原样回放。`agent-provider-openai-compatible/src/request.rs` 的 `arguments_string` 有意这么做，测试 `arguments_that_were_not_json_go_back_as_received` 固定了它。
3. LM Studio 在开始生成之前就返回 HTTP 500（日志里没有 "Running chat completion"），和它的 chat template 解析不了这段 arguments 一致。三次重试同一个请求体同样 500，turn 以 "model unavailable: HTTP 500" 失败。托管线路（opencode-go 等）接受这种请求体，所以此前没撞过。
4. 退化输出能填满窗口，是因为这条线路只有 `--context-window` 给的 C，O 未知。ADR 0018 规定只在 C 和 O 都已知时才发 `max_tokens = min(O, ⌊C/4⌋)`，这里没发，模型的输出就不受 R = 8,192 的约束，直接把预算里留给输出的那块和别的一起吃掉。

## 待决方向

A. 回放截断的调用时发什么：
1. 存储不变（journal 里保留原始字串），回放时如果 arguments 不是合法 JSON 且该调用的结果是 `truncated`，发 `{}`；工具结果照旧带 "Error: … truncated"。模型看到的是"你上次的调用被截断了"，这正是事实。配对关系（tool_call 与 tool 结果一一对应）保持，每个提供方都接受。
2. 回放时整个丢掉这一对（调用和结果）。更干净，但模型失去"刚才被截断"这个信息，容易重犯。
3. 保持原样。LM Studio 这类严格 template 的本地服务永远过不去。不推荐。

B. 只知道 C 时的 `max_tokens`：
1. 只要 C 已知（catalog 或 `--context-window` / `model.context_window`），就发 `max_tokens = ⌊C/4⌋`；O 已知时仍是 `min(O, ⌊C/4⌋)`。R 本来就按这个数预留，发出去只是让模型遵守。改 ADR 0018 §输出 那一句。
2. 保持不发。小窗口上一次失控输出就能把窗口填满。不推荐。

建议 A1 + B1。两处都是改决定，各自一个 docs-only 的 ADR 提交（0017 加一句回放规则，0018 改 `max_tokens` 那句）。

## 验收

- 回放：一条 arguments 为非法 JSON 且结果为 `truncated` 的调用，请求体里 `arguments` 为 `{}`；非截断的非法 JSON（模型发了就是发了）仍原样回放，现有测试改名不改义；Anthropic 适配器对应路径同样处理。
- `max_tokens`：`--context-window 32768` 且 catalog 无 O 时，请求体里 `max_tokens = 8192`；C 和 O 都有时不变；都没有时仍不发。手册 `providers.md` 那段同步。
- 复现：Hai-PC 线路上同一个 brief，turn 不再因 500 失败。
- 变异：去掉 `{}` 替换、去掉 C-only 的 `max_tokens`，各有测试失败。
