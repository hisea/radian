# 变异记录：Anthropic adapter 与契约的两个出口（Phase 3 提前）

日期：2026-10-12。全部变异在一次性检出 `/tmp/anthropic-mut`（`git worktree add --detach HEAD`，HEAD = `a5dd07e`）里做：变异 → `cargo build`（由 `cargo test` 隐式完成）→ 只跑对应测试（`cargo test ... -- --exact --no-fail-fast`）→ 恢复文件。跑完 `git status --short` 为空，worktree 已删除。

每条契约条目一个针对性变异（破坏 adapter 对应行为，确认该条契约失败），外加两个出口的四个反向变异。

| 变异 | 目标测试 | 结果 |
|---|---|---|
| M1 在 `finish` 丢掉文本内容 | `contract_1_plain_text` | 抓住（rc=101） |
| M2 停止发出 `ModelTextDelta` | `contract_2_streamed_deltas` | 抓住 |
| M3 忽略 `input_json_delta`（参数恒空） | `contract_3a_single_tool_call` | 抓住 |
| M4 只保留第一个 tool call | `contract_3b_multiple_tool_calls` | 抓住 |
| M5 分片参数只保留第一片 | `contract_4_split_arguments` | 抓住 |
| M6 重复 id 不改名 | `contract_5_call_ids_unique` | 抓住 |
| M7 去掉 `pause_turn` 映射 | `contract_6_finish_reasons` | 抓住 |
| M8 **adapter 丢掉 `total` 却不声明**（静默） | `contract_7a_usage_shapes` | 抓住（未声明字段必须按原值断言） |
| M9 `total = input + output` | `contract_7b_total_not_computed` | 抓住 |
| M10 把 500 归为 `Protocol` | `contract_8_error_classification` | 抓住 |
| M11 去掉 `complete` 的取消分支 | `contract_9_cancellation` | 抓住 |
| M12 不再 `mask()` 错误文本 | `contract_10_secret_not_leaked` | 抓住 |
| M13 出口 1：harness 删掉 `unsupported_reasons` 覆盖（静默不声明） | `contract_6_finish_reasons` | 抓住（`ContentFiltered` 段随即失败） |
| M14 出口 1：`reasons_to_check` 忽略声明、恒返回全部 | 契约 crate `tests::a_declared_reason_is_skipped` | 抓住 |
| M15 出口 2：`expected_usage` 忽略声明、保留脚本值 | 契约 crate `tests::a_declared_unreported_field_must_be_absent` | 抓住 |
| M16 出口 2：去掉「声明字段必须缺席」的断言 | 契约 crate `tests::a_declared_field_that_is_filled_fails` | 抓住（该 `#[should_panic]` 测试不再 panic，于是失败） |

结论：**12 条契约子检查（1/2/3a/3b/4/5/6/7a/7b/8/9/10）全部有「变异 → 测试失败」的证据**（7a 的 M8 证明：未声明就丢字段会被抓；若声明了却填值，由 M16 对应的契约测试证明会被抓）。两个出口各有正反两面：声明了就生效（M14/M15 破坏后失败）、没声明就必须满足（M13/M8 破坏后失败）、声明了却仍然填值必须被抓（M16 破坏后失败）。

adapter 专有行为（`tests/http.rs` 的 32 个用例）与 lib 单测（35 个）另有自己的断言，本次没有为它们逐个做变异（任务单只要求契约条目与出口）。

## 扩展思考（ADR 0014 R10 落实，2026-10-13）

一次性检出 `/tmp/radian-mut`（`git worktree add --detach HEAD`，HEAD = `b9c43c2`），每个变异
用 `git checkout --` 恢复、`git status --short` 为空；跑 `cargo test -p agent-provider-anthropic
--lib <精确测试名>`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M17 | `finish` 构造 thinking 块时丢掉 `signature`（只留 `thinking`） | 抓住 (rc=101) | `thinking_blocks_are_preserved_and_never_become_content` |
| M18 | `assistant_message` 把保留块 `reverse()`（顺序颠倒） | 抓住 | `preserved_thinking_blocks_are_echoed_before_tool_use_in_order` |
| M19 | `assistant_message` 把思考块放到 `tool_use` **之后** | 抓住 | `preserved_thinking_blocks_are_echoed_before_tool_use_in_order` |
| M20 | 流解析只认 `thinking`，丢掉 `redacted_thinking` 块 | 抓住 | `redacted_thinking_blocks_round_trip_verbatim` |
| M21 | 请求重建的过滤器只放行 `thinking`（丢掉 `redacted_thinking`） | 抓住 | `preserved_thinking_blocks_are_echoed_before_tool_use_in_order` |
| M22 | `thinking_delta` 同时 `emit(ModelReasoningDelta)`（把思考写进推理通道/UI） | 抓住 | `thinking_blocks_are_preserved_and_never_become_content`（也由 http 的 `a_thinking_turn_echoes_the_blocks_back_on_the_next_request` 抓） |
| M23 | 关闭 thinking 时也插入一个 `thinking` 对象（`config.thinking` 缺省时 `Adaptive`） | 抓住 | `thinking_off_leaves_the_request_body_unchanged` |

M22 的 UI 侧断言（`http.rs`）与 lib 单测都覆盖；两条都是对新行为的具名测试。

**JSONL 往返**：`provider_payload` 的字节保真由既有的
`radian-session` 测试 `write.rs::provider_payload_is_written_byte_for_byte` 钉住（该测试在本切片
之前就存在，本切片未改动它）；adapter 侧的「从 JSON 读回后仍回传」由
`a_payload_read_back_from_json_still_echoes_thinking` 钉住。

## anthost 追加（2026-10-16，端到端 resume）

一次性 worktree `/tmp/anthost-mut`（从 `47ede1a` 创建，用完删除），`--no-fail-fast`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M24 | `thinking_blocks` 的过滤器改成永不匹配（resume 后请求里没有思考块） | 抓住 (rc=101) | `radian-runtime` 的 `thinking_resume::a_thinking_block_survives_close_and_resume`（真会话文件 + 关闭/重开 + Anthropic mock） |

M24 与 M17–M23 不同：那些是 adapter 单测/直接 `complete` 层面，M24 是**跨关闭与 resume**
的端到端断言（thinking 块连同 signature 必须从 JSONL 读回后原样回传）。

## 网络异常矩阵追加（resilience，2026-10-16）

一次性 worktree `/tmp/resil-mut`（从 `20a8ac7` 创建，用完删除），`--no-fail-fast`，每个变异后从副本还原。

| # | 变异 | 结果 | 抓住它的测试（`tests/resilience.rs`） |
|---|---|---|---|
| M25 | `mask` 的 scrub 改成恒等（错误里带上 key） | 抓住 (rc=101) | `a_stream_error_that_echoes_the_key_is_masked` |
| M26 | `run` 去掉 `idle_timeout` 的 `tokio::time::timeout` 包裹（空闲超时不生效） | 抓住（看门狗触发） | `a_silent_server_hits_the_idle_timeout` |
| M27 | `complete` 去掉取消 `select!` 分支（取消后不返回） | 抓住（看门狗触发） | `cancelling_returns_instead_of_hanging` |
| M28 | `finish` 容忍 `done=false`/缺 `stop_reason`（中途断开当作成功） | 抓住 (rc=101) | `an_early_eof_after_deltas_is_a_transport_error` |

## ADR 0018 超长错误分类（overflow，2026-10-13）

一次性 worktree `/tmp/radian-overflow-mut`（HEAD = `cc71ab4`，独立 target），`--no-fail-fast`；
变异后 `git checkout --`、`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M29 | 分类忽略状态码（`is_overflow_signature` 不按 status 配对，`classify_overflow` 的门也去掉） | 抓住 (rc=101) | `overflow::tests::other_statuses_are_not_overflows`（`500`/`429` 的 400 形态被判为超长） |
| M30 | 不看 `error.type`（只看 message；413 无条件成立） | 抓住 (rc=101) | `overflow::tests::the_error_type_matters` |
| M31 | `mask()` 对 `ContextOverflow` 不 scrub | 抓住 (rc=101) | `a_context_overflow_body_that_echoes_the_key_is_masked` |

**等价变异（记录，不是漏网）**：只把 `classify_overflow` 的门从 `400|413` 改成 `400|413|429`
（不动 `is_overflow_signature`）不改变行为——`is_overflow_signature` 自己也按 status 配对，429
仍返回 false。真正改变行为的是 M29。

## 提示缓存（`cache_control`，2026-10-15）

一次性检出 `/private/var/.../opencode/anthropic-cache-mut`（`git worktree add --detach HEAD`，
HEAD = `1395e3e`），每个变异后 `git checkout -- .` 恢复、`git status --short` 为空，worktree 用完删除。
跑 `cargo test -p agent-provider-anthropic --lib --no-fail-fast`（共享主 worktree 的 target）。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M32 | 去掉工具定义上的断点（`tools` 分支不再 `insert(cache_control)`） | 抓住 (rc=101) | `auto_places_the_three_breakpoints_where_stated`、`breakpoints_follow_what_the_request_carries` |
| M33 | `None` 仍然放断点（`caching` 恒为 `true`） | 抓住 (rc=101) | `none_sends_the_body_the_adapter_sent_before_prompt_caching`（及 5 条断言旧字节形状的既有测试） |
| M34 | 每条消息都放断点（三消息请求上出现第 5 个断点） | 抓住 (rc=101) | `auto_places_the_three_breakpoints_where_stated`（`count_cache_breakpoints == 3` 变 5），另 3 条 |
| M35 | Auto 下 system 仍是纯字符串 | 抓住 (rc=101) | `auto_places_the_three_breakpoints_where_stated`、`breakpoints_follow_what_the_request_carries` |
| M36（附加） | `thinking`/`redacted_thinking` 被当作可缓存 | 抓住 (rc=101) | `auto_never_marks_a_thinking_block` |
| M37（附加） | 把限量断言退回「整份 body 深度扫描 `cache_control`」（并解掉该辅助函数的 `#[cfg(test)]`） | 抓住 (rc=101) | `auto_does_not_mistake_a_user_data_key_for_a_breakpoint` |
| M38（评审发现） | `is_cacheable` 把空文本块当作可缓存（`Some("text") => true`） | 抓住 (rc=101) | `auto_skips_a_trailing_empty_text_block`、`auto_never_marks_an_empty_text_block` |

M32/M35 的两条捕获都同时来自「放置位置」与「断点计数」测试；M34 证明计数测试能抓住超限（第 5 个）
断点，而不是只在恰好 3 个时才成立。M37 单独覆盖新增行为：限量断言只数 adapter 自己放的断点，工具
schema / 工具调用参数里同名的键是调用方数据，不能被误当成断点（M37 让它们被数进去，于是该请求触发
`debug_assert` 而 panics）。M38 覆盖空文本块规则：`blocks()` 会丢掉空的 `ContentPart::Text`，但
`resource["text"]` 为空的 `EmbeddedResource` 仍会变成 `{"type":"text","text":""}`，Anthropic 不接受
它上面的 `cache_control`（wire notes §2.4）；两条新测试要求断点回退到前一个非空块、或在只有空块时
不放断点（M32–M37 在 1395e3e 的临时 worktree 里跑；M37/M38 就地跑，文件从副本恢复并逐字节比对）。

## 限额头 `anthropic-ratelimit-*`（ADR 0027 切片 ④，2026-10-17）

Base `c09b614` · branch `dogfood/footer-s4`。变异在源码原地改，只跑抓住它的测试二进制
（`--no-fail-fast`），跑完从 `/tmp` 的 `cp` 副本还原并 `cmp` 一致。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| RL-A | 限额头前缀误读（`anthropic-ratelimit-` → `anthropic-ratelimits-`） | ✅ 1 failed (rc=101) | `contract_14_rate_limit_headers_are_kept_raw`（`response.limits.expect` 为 `None`）；`contract_14b` 仍过（无头不断言有值） |

命令：`timeout 300 cargo test -p agent-provider-anthropic --features testing --test contract
--locked --no-fail-fast contract_14`。`contract_14` 在旧代码（`limits: None` 恒成立）下同样失败：
`expect("the rate-limit headers are kept")` 为空；`contract_14b` 在旧代码下通过（它是「不编造」
的断言，不是行为变异）。

