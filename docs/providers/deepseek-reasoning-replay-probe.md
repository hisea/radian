# `deepseek-v4.1-flash` 推理回传实测（ADR 0014 Q3）

**日期：2026-10-15。** 目的：ADR 0014 Q3 要求用真实端点确认两件事——(a) 带 `tools`
的历史里，一条**不带** `reasoning_content` 的 assistant 消息是否 400；(b) 空串 `""` 是否被接受。
本文只记录实测，不改写 ADR 的依据。

## 方法与范围

- 模型 `deepseek-v4.1-flash`，经 opencode-go，基址 `https://opencode.ai/zen/go/v1`。
- 走 `agent-provider-openai-compatible` 适配器本身（`radian_host::build_model`），key 由
  radian 自己从 `~/.radian/credentials/auth.json` 读入；key 从未打印、记录或落盘。
- 请求带 `tools`（一个 `echo` 工具），先做**两步工具调用**：模型发起 `echo` 调用、收到工具结果、
  再发起第二次、再收到结果、最后给出可见答案。每一步都带 `reasoning_content`（实测推理文本
  174 / 24 / 239 字符）。
- 然后对同一条历史做三组对照请求（都带 `tools`）：
  - **(a)** 适配器关闭回传（`reasoning_replay = None`）：assistant 消息**不带** `reasoning_content`。
  - **(b1)** 适配器打开回传（`reasoning_replay = Some("reasoning_content")`）：写回真实推理。
  - **(b2)** 同 (b1)，但把最后一条 assistant 的推理清空：适配器写 `"reasoning_content": ""`。
- 另做两组更保守的 (a) 变体：把 `reasoning_effort = "high"` 一起发出；以及一条最小的
  「一条 assistant 工具调用（无推理）+ 一条工具结果」历史。

## 结果

| 组 | 请求形态 | 结果 |
|---|---|---|
| (a) | 完整两步历史，assistant 不带 `reasoning_content` | **200，无 400** |
| (a2) | 同上，另发 `reasoning_effort = "high"` | **200，无 400** |
| (a3) | 最小历史（一条 assistant 工具调用 + 工具结果），不带字段，`reasoning_effort = "high"` | **200，无 400** |
| (b1) | 完整两步历史，写回真实推理 | **200** |
| (b2) | 同上，`"reasoning_content": ""` | **200** |

## 与 ADR 修订 1 Q3 的关系

- **(b) 与 ADR 一致**：空串 `""` 被接受。
- **(a) 与 ADR 的假设相反**：本次实测中，带 `tools` 的历史里一条不带 `reasoning_content` 的
  assistant 消息**没有**得到 400。ADR Q3 的保守做法（每条 assistant 都补 `""`）因此**不是必需**，
  但仍然安全（(b) 接受空串）。ADR 背景里记录的 dogfood T4 400
  （`The reasoning_content in the thinking mode must be passed back to the API`）本次**未能复现**。

## 未验证 / 限制

- 单次、单端点、单时点。ADR 自己也写了「可能是上游最近才开始强制，也可能只在特定历史形态下才
  检查」；opencode-go 可能按后端分流，命中的后端不强制。**不能**由此断定该强制永远不存在。
- 没有复现 T4 的确切历史形态（那次会话没有留下可读的错误记录）。
- 未测其它 `deepseek-v4*` 模型、其它 opencode-go 路由或 deepseek 官方端点。
- 因此实现仍按 ADR 修订 1 的保守形态：目录（或 `[providers.<id>]`）要求回传时才回传，缺推理的
  assistant 消息补 `""`。是否据此修订 ADR Q3，由 决定。
