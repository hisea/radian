# Mutation checks — footer cost & token accounting (ADR 0007, ADR 0027)

Base: `2ad8d40` (the last commit of this slice) on `dogfood/cost-fix`. Each mutation was
applied with a one-line `python3` edit, the named test was run with `--no-fail-fast`, and the
source was restored from a copy afterwards (`git diff` empty at the end, checked). The machine
was shared with other agents, so the runs are the per-crate ones named below rather than a
single `--workspace` run.

| # | Behaviour mutated | Mutation | Result |
|---|---|---|---|
| M1 | the fee bills only the **uncached** input (F1) | `cost_micros`: `add(usage.uncached_input(), …)` → `add(usage.input, …)` — the original double-charge | ✅ caught: `radian-host::cost::an_openai_shaped_cache_hit_is_billed_once_at_the_cache_rate`, `a_cache_write_is_billed_at_its_own_rate`; `radian-tui::footer::a_cache_hit_is_billed_once_at_the_cache_rate` |
| M2 | a zero counter is not billed (F7) | `billed \|= tokens > 0` → `billed = true` | ✅ caught: `cost::an_all_zero_report_has_nothing_to_bill`; `footer::an_all_zero_response_hides_the_cost` |
| M3 | each call is priced at its own route (F4) | `SessionProjection::add_usage` recomputes the whole fee from the session totals at the **current** price (the old "totals × price" formula) | ✅ caught: `footer::a_model_switch_prices_only_the_calls_after_it`, `footer::a_cache_hit_is_billed_once_at_the_cache_rate` |
| M4 | the summarizer's usage reaches the host (F2) | `turn.rs`: never emit `AgentEvent::CompactionUsage` | ✅ caught: `radian-core::compaction_flow::each_summarization_attempt_reports_its_usage_once` |
| M5 | the **retry** counts too (F2) | emit only when `attempt == 0` | ✅ caught: `each_summarization_attempt_reports_its_usage_once` (the event list is `[10, 20]`, the mutation yields `[10]`) |
| M6 | a summary with no reported usage emits nothing (F2) | emit `response.usage.clone().unwrap_or_default()` | ✅ caught: `a_summarization_call_without_usage_reports_nothing` |
| M7 | the TUI counts it (F2) | `events.rs`: `CompactionUsage` → `false` (no projection write) | ✅ caught: `radian-tui::events::tests::a_compaction_usage_joins_the_session_totals`, `footer::a_compaction_usage_reaches_the_footer_totals` |
| M8 | resume folds the journal back (F3) | `open_session`: replace the `restore_usage` call with `let _ = price_for;` | ✅ caught: `pty::pty_a_resume_shows_the_sessions_earlier_tokens_and_fee` (the host unit tests call `restore_usage` directly, so only the pty test covers this wiring — noted deliberately) |
| M9 | each restored call is priced at its own route (F3) | price every restored record with the **expensive** route (`price_for("anthropic", "expensive")`) | ✅ caught: `cost_resume::a_resume_restores_the_totals_and_the_fee_at_each_calls_own_price`, `an_unpriced_route_restores_the_tokens_without_inventing_a_fee`, `the_snapshot_route_prices_the_calls_before_the_first_switch` |
| M10 | the summarizer's restored usage counts (F3) | drop the `CompactionUsageRecorded` arm of the fold | ✅ caught: `a_resume_restores_the_totals_and_the_fee_at_each_calls_own_price`, `an_unpriced_route_restores_the_tokens_without_inventing_a_fee` |
| M11 | the snapshot route prices the first calls (F3) | `let mut price = None;` (only `ConfigChanged` sets it) | ✅ caught: `a_resume_restores_the_totals_and_the_fee_at_each_calls_own_price`, `an_unpriced_route_restores_the_tokens_without_inventing_a_fee` |
| M12 | `{cache_write}` is a known placeholder (F8) | drop it from `item_placeholders("tokens")` | ✅ caught: `footer::the_cache_write_tokens_show_only_when_the_format_asks_for_them` (asserts the config parses with **no** warning) |
| M13 | the default format does not grow (F8) | add `{cache_write}` to the `tokens` default format | ✅ caught: `the_cache_write_tokens_show_only_when_the_format_asks_for_them` |
| M14 | the adapter declares the input's cache semantics (F1/F6) | OpenAI-compatible `input_includes_cache`: `Some(true)` → `Some(false)` | ✅ caught: `agent-provider-openai-compatible::contract::contract_7c_input_cache_semantics`; `stream::usage_maps_openai_and_anthropic_field_names`, `a_cached_prefix_is_marked_so_the_full_rate_part_is_the_remainder` |
| M15 | …and the Anthropic one declares `false` | Anthropic `input_includes_cache`: `Some(false)` → `Some(true)` | ✅ caught: `agent-provider-anthropic::contract::contract_7c_input_cache_semantics`; `stream::usage_merges_message_start_and_message_delta_field_by_field` |
| M16 | an unmarked usage is refused (F6) | the adapter leaves `input_includes_cache: None` | ✅ caught: `agent-provider-contract::tests::an_unmarked_usage_fails_the_cache_semantics_check` (the check's own `#[should_panic]`), plus `check_input_cache_semantics` in both suites |
| M17 | the budget baseline is the whole prompt (ADR 0015) | `turn.rs`: `.and_then(ModelUsage::prompt_total)` → `.and_then(\|u\| u.input)` | ✅ caught: `radian-core::budget::the_reported_baseline_includes_the_cache_read_and_write` |
| M18 | …on the resume path too | `driver.rs`: `usage.prompt_total()` → `usage.input` | ✅ caught: `radian-runtime::driver::tests::an_anthropic_shaped_report_restores_the_whole_prompt_as_its_baseline` |

Two mutations were **inert on the first attempt** and are recorded as such:

- **M12 (first form)** — removing `cache_write` from the data list (`values`) is not a mutation
  at all: `expand` reads `data.values`, so the value is still there; only the *placeholder list*
  (used to warn about an unknown placeholder at config parse) matters. The test was strengthened
  to assert `apply_footer` returns no warning, and the mutation then failed as expected. The
  hole was in the test, not in the code.
- **M9 (first form)** — trying to mutate via `SessionProjection::add_usage` was impossible
  because the fold goes through `add_usage_at`; the mutation was written against the wrong
  function. The working form is the one in the table.

## Known gaps in this record

- The resume wiring (M8) is covered by a pty test rather than a unit test, so it is slower and,
  under heavy machine load, the surrounding pty suite has flaked once
  (`pty_the_shipped_default_footer_is_two_rows_without_a_config`, an unrelated test that passed
  5/5 in isolation and on three subsequent full runs). Not a hole in this slice's assertions.
- M14–M16 were verified during piece 1 (the contract work) rather than re-run here; the
  commands and outcomes above are from that run.
