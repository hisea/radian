# Mutation checks — ADR 0017 (transient model errors are retried)

Base: `f3226d6` (the four implementation commits of revision 1) in this worktree, with the
strengthened cancel test in `crates/agent-core/tests/retry.rs` applied. Source files were copied
to a scratch dir before each mutation and restored from the copy afterwards; `git status --short`
was clean (only the intended `retry.rs` change) before the next mutation. Each mutation was run
with `cargo test -p agent-core --locked --no-fail-fast <test>`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| M1 | The retry branch drops the `is_transient()` guard (`if retries_used < max_retries`), so a 400 is retried too | caught (rc 101) | `retry::a_protocol_error_is_never_retried` (the second request was sent and the retried protocol error never fails the turn) |
| M2 | `retry_delay` ignores the provider's `Retry-After` (`Some(_after) => RETRY_BASE_DELAY`) | caught (rc 101) | `retry::a_retry_after_is_honoured_exactly` (announced 2 s, expected 1 s) |
| M3 | `retry_delay` drops the total-wait cap (`.min(MAX_RETRY_WAIT.saturating_sub(waited))`) | caught (rc 101) | `retry::the_total_wait_is_capped_at_sixty_seconds` (delays were 40/40/40 s, expected 40/20/0) |
| M4 | The backoff wait is not raced against the interrupt (`tokio::time::sleep(delay).await; continue`) | caught (rc 101) | `retry::cancelling_during_the_backoff_ends_the_turn_without_another_request` (2.217 s of paused-clock time passed before the turn ended) |
| M5 | The failed attempt's streamed text is committed as an interrupted assistant message before the retry | caught (rc 101) | `retry::two_dropped_streams_are_retried_and_the_turn_succeeds` (assistants were `["halfway", "lost", "done"]`, expected `["done"]`) |
| M6 | `is_transient()` treats every `Transport` as transient (`ModelError::Transport(_) => true`), so cancellation would be retried | caught (rc 101) | `model::tests::only_temporary_errors_are_transient` |
| M7 | The retry branch drops the spent-budget stop (`&& waited < MAX_RETRY_WAIT`), so once the 60 s are used up the remaining attempts fire back to back | caught (rc 101) | `retry::a_spent_wait_budget_stops_retrying_instead_of_firing_back_to_back` (the error read `after 3 retries`, expected `after 1 retry`) |

7/7 caught; none was equivalent.

## Notes on two of the mutations

- **M4** exposed a **hole** in the first version of the cancel test. Cancelling from the event
  sink on `ModelRetrying` left the interrupt already set when the loop came back around, and the
  next attempt's `race` (it is `biased`, so it checks the interrupt first) returned `Interrupted`
  before polling the model — so `requests().len() == 1` and `Cancelled` held even when the wait
  itself was never raced. The test now also asserts that no paused-clock time passes:
  `tokio::time::Instant::now() - started == Duration::ZERO`. With the mutation that gap becomes
  2.217 s and the test fails; with the real code it is zero. The fix is in the same commit as this
  file.
- **M5** is the faithful executable form of "the failed attempt's text is discarded": the retry
  branch commits `tap.take()` as an interrupted assistant message (exactly the shape the real
  interrupted-stream path uses) before `continue`. The successful response's own content is what
  the transcript gets on the happy path, so only a *commit* of the tap can make the failed
  attempt's text survive — the mutation adds exactly that.
- **M7** is a review fix. The first implementation capped each wait by what
  was left of the 60 s, so a spent budget made `retry_delay` return 0 and the remaining retries
  fired back to back — for `Retry-After: 60` on every attempt, retries 2 and 3 hit the provider
  immediately after it said it was busy. The retry branch now also requires `waited <
  MAX_RETRY_WAIT`; a spent budget fails the turn with the retries really made. Removing the guard
  is M7, caught by the new `a_spent_wait_budget_stops_retrying_instead_of_firing_back_to_back`
  (one retry then failed, not three). The old total-cap test's `[40, 20, 0]` expectation became
  `[40, 20]` (the 0 was the bug).

`journal-selfcheck` (`cargo test -p agent-core --locked --features journal-selfcheck`, one of the
seven Verify commands) replays every journal against its transcript and therefore also catches an
M5-shaped leak that reaches a durable record.
