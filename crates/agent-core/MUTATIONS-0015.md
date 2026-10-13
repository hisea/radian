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
  `request_bytes`, `project`, `exceeds`) and its re-exports;
- `AgentConfig::context_budget: Option<ContextBudget>` (default `None`);
- `AgentSession::context_budget_status()` and the private `budget_baseline`/`budget_status`
  fields;
- `TurnOptions::context_budget_relax: bool` (default `false`);
- `RunStatus::ContextBudget(BudgetReport)` and its `wire_outcome` arm.

`radian-session`:

- `TurnOutcome::ContextBudget`;
- `RuntimeEvent::ContextBudgetReached` (+ wire data, `name`, `critical = false`);
- reader `EventData::ContextBudgetReached`, the `KNOWN_EVENTS` entry, and the export arm.

No existing variant, field or behavior was changed; only additions.
