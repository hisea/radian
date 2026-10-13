# Mutation checks — ADR 0015 context budget guard

Run in a one-time worktree (`git worktree add /tmp/budget-mut HEAD`) against commit
`58c4b7f` plus the strengthened `tests/budget.rs`, with `cargo test -p agent-core --test
budget --no-fail-fast`. Each mutation was restored from a copy and `git status --short`
was checked before moving on.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| M1 | On reaching the threshold, do **not** return: let the request go out anyway. | caught (rc 101) | `reported_usage_blocks_the_next_request_before_it_is_sent` (also 2, 4, 5) |
| M2 | In `project`, label a purely local projection `UsageSource::Reported`. | caught (rc 101) | `without_reported_usage_the_estimate_is_labelled` |
| M3 | In `AgentConfig::default`, guess `context_budget: Some(ContextBudget::new(128_000))`. | caught (rc 101) | `no_window_configured_disables_the_guard` |
| M4 | On blocking, `transcript.pop()` one message before returning. | caught (rc 101) | `blocking_does_not_change_the_transcript` |
| M5 | Do not consume the one-shot relax flag (the exemption persists within the turn). | caught (rc 101) | `relaxing_once_sends_that_request_and_restores_the_guard` |

Every mutation was caught. M1 is also caught by the estimate, Continue and transcript
tests, because all four exercise the same "the request must not be sent" invariant; the
named test is the one written for it.

## Follow-up review (round 2)

Run in a one-time worktree (`git worktree add /tmp/budget-mut2 HEAD`) against commit
`8c1b61d`, with `--no-fail-fast`. Each mutation was restored from a copy; `git status
--short` was clean before moving on.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| M6 | Estimate non-ASCII by `non_ascii_bytes / bytes_per_token` (the old under-count: Chinese at 3 bytes/char → 0.75 token/char). | caught (rc 101) | `chinese_text_estimates_at_least_one_token_per_character` |
| M7 | In `project`, label a reported-baseline-plus-growth projection `UsageSource::Reported` instead of `ReportedPlusEstimate`. | caught (rc 101) | `reported_usage_blocks_the_next_request_before_it_is_sent` |
| M8 | Drop `#[serde(other)]` from `TurnOutcome::Unknown`, so a future outcome is `unknown variant` again. | caught (rc 101) | `unknown_enum_values_are_mapped_to_unknown_not_refused`, `an_unknown_enum_value_in_a_runtime_event_maps_to_unknown` |

`RunStatus`'s `#[non_exhaustive]` is a compile-time property: removing the wildcard arm in
`wire_outcome` fails the build, so there is no runtime mutation for it.

Input-source coverage is a unit test rather than a mutation: `InputSource::Unknown` is
asserted by `input_sources_round_trip_as_snake_case_and_map_unknown_values` in
`agent-core/src/message.rs` (it used to assert `future_source` was a parse error).

Notes:

- M4's pop is a stand-in for any implicit compression. The test snapshots
  `request.messages` inside `before_model_call` on the blocked call and asserts the
  serialized bytes equal `AgentSession::transcript()` after the turn, so any removal or
  rewrite on the block path fails it.
- M5 required strengthening test 4 so a relaxed turn makes two model requests: the first is
  sent under the exemption, the second is blocked again. Without that, a "flag never
  consumed" mutation stayed green — the hole this record closes.

## Additive changes to `agent-core` / `radian-session`

`agent-core`:

- new module `budget` (`ContextBudget`, `BudgetReport`, `BudgetStatus`, `UsageSource`,
  `RequestSize`, `request_size`, `estimate_tokens`, `project`, `exceeds`) and its re-exports;
- `AgentConfig::context_budget: Option<ContextBudget>` (default `None`);
- `AgentSession::context_budget_status()` and the private `budget_baseline`/`budget_status`
  fields;
- `TurnOptions::context_budget_relax: bool` (default `false`);
- `RunStatus::ContextBudget(BudgetReport)`, `#[non_exhaustive]` on `RunStatus`, and its
  `wire_outcome` arm;
- `InputSource::Unknown` with `#[serde(other)]` (ADR 0006 §7(d)).

`radian-session`:

- `TurnOutcome::ContextBudget` and `TurnOutcome::Unknown` (`#[serde(other)]`);
- `TurnReason::Unknown`, `InboxLane::Unknown`, `PermissionDecision::Unknown`,
  `PermissionApprover::Unknown`, all `#[serde(other)]` (ADR 0006 §7(d));
- `RuntimeEvent::ContextBudgetReached` (+ wire data, `name`, `critical = false`);
- reader `EventData::ContextBudgetReached`, the `KNOWN_EVENTS` entry, and the export arm;
- ADR 0006 §7(d) + `fixtures/unknown-enum-values.jsonl` (+ README + check-fixtures.sh).

No existing variant, field or behavior was changed; only additions. The one behavior that
changed is intentional and specified: an unknown on-disk enum value is now `Unknown` instead
of a corruption error (ADR 0006 §7(d)).

## Round 3：resume 恢复基线（`restore_budget_baseline`）

一次性检出 `/tmp/radian-mut`（`git worktree add --detach HEAD`，HEAD = `b9c43c2`，含 B/C 全部改动）；
每个变异恢复文件、`git status --short` 为空后才继续；`cargo test -p agent-core --test budget
--no-fail-fast`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M9 | `restore_budget_baseline` 把越界前缀 clamp 进 transcript 而不是返回 `false`（伪造一个基线） | caught (rc 101) | `a_prefix_outside_the_transcript_leaves_the_estimate_alone` |
| M10 | 重算基线大小用**整个** transcript 而不是 `transcript[..prefix_len]` | caught (rc 101) | `a_restored_baseline_matches_an_uninterrupted_projection` |

M10 是「恢复的投影必须与不中断一直跑相同」的反向证明：前缀取错，投影就与 live 不同。

## Round 3：runtime 侧（见 `crates/radian-runtime/MUTATIONS.md`）

「resume 不恢复基线」「恢复了错误的那次报告（first-wins）」「`BudgetRelax::take` 的 `swap` 改
`load`」三条变异在 `radian-runtime` 的 budget 测试里被抓住，记录在那里。

## ADR 0018 round：去掉阻断（`pi/guard`，2026-10-13）

一次性 worktree `/tmp/radian-guard-mut`（`git worktree add --detach HEAD`，HEAD = `8825691`，
**独立 `CARGO_TARGET_DIR`**，用完删除），测试一律 `--no-fail-fast`；每个变异恢复文件、
`git status --short` 为空后才继续。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M11 | 超过 `B` 时改为结束这一轮（把 `Over` 分支换成 `return Ok(self.run.finish(RunStatus::Refused, events))`，即"仍然停"） | ✅ caught (rc 101) | `reported_usage_over_the_budget_still_sends_the_request`（模型只收到 1 次请求） |
| M12 | `over_budget` 的 `>` 改成 `>=`（`T == B` 也算超） | ✅ caught (rc 101) | `the_boundary_is_strictly_above_the_budget` |

M12 证明边界由测试钉住：`T == B` 必须在预算内，压缩只在严格超过 `B` 时才有理由触发。
