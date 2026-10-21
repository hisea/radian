# OpenAI 兼容端点的限额响应头（`agent-provider-openai-compatible`）

范围：只记 ADR 0027 切片 ④ 需要的限额响应头。本 adapter 的线格式主体是 OpenAI 的
Chat Completions 形状（SSE chunk、`usage` 三形态、`finish_reason`），那些不在这里记——本文件
不是该形状的完整 wire notes，只钉住限额头的名字与来源。

来源：`https://developers.openai.com/api/docs/guides/rate-limits`（旧址 `platform.openai.com/docs/guides/rate-limits` 301 到此；**p1 2026-10-17 已对照核实**：`x-ratelimit-{limit,remaining,reset}-{requests,tokens}` 与本表一致，reset 为 `1s`、`6m0s` 式时长；该页另列 `x-ratelimit-*-project-tokens`，本切片不解析）。原写（OpenAI 官方 rate-limits 指南：
`x-ratelimit-limit-*` / `x-ratelimit-remaining-*` / `x-ratelimit-reset-*` 三组，requests 与
tokens 各一对）。本离线 worktree 未重拉该页；头名按切片 ④ 任务单的方向取（请求数与 token
数的 limit / remaining / reset），adapter 只按前缀 `x-ratelimit-` 原样保留（`model.rs` 的
`RATE_LIMIT_PREFIX`），具体名字只出现在宿主的解析表（`radian-host` 的 `OPENAI_HEADERS`），
改名不需要动 adapter。

| 头 | 含义 | 宿主怎么用 |
|---|---|---|
| `x-ratelimit-limit-requests` | 请求数上限 | 保留在原始头里，不进快照 |
| `x-ratelimit-remaining-requests` | 请求剩余数 | `requests` 窗口的 `remaining`（计数） |
| `x-ratelimit-reset-requests` | 请求窗口重置（时长，如 `1s`） | `requests` 窗口的倒计时 |
| `x-ratelimit-limit-tokens` | token 上限 | 保留在原始头里，不进快照 |
| `x-ratelimit-remaining-tokens` | token 剩余数 | `tokens` 窗口的 `remaining`（计数） |
| `x-ratelimit-reset-tokens` | token 窗口重置（时长，如 `6m0s`） | `tokens` 窗口的倒计时 |

规则：只解析上表六个名字（精确小写匹配；兼容端点自加的其它头一律忽略）；值保持原文，
计数解析不出是整数则该窗口只剩倒计时，倒计时解析不出则只剩计数（ADR 0027：一个坏
字段只丢自己）。429 的 `Retry-After` 仍走旧的 `Retry-After` 解析
（`Unavailable.retry_after`），与这六个头无关。

**未验证**：兼容端点各行其是，某家不发送这组头时 `limits` 为 `None`，宿主不断言有值（契约的
`contract_14b` 与 `no_headers_is_no_snapshot` 钉住「缺席不断言缺席之外的任何事」）；某家用
别的头名时，本表加名即是全部改动。
