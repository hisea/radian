# `benchmarks/terminal-bench` mutation record

New rows use the id `<task id>-M<n>` (for example `TK0252-M1`); see "Mutation records" in `AGENTS.md`.

Base: `work/TK0252-journal-metrics-extractor` · method: apply the mutation to
`benchmarks/journal_metrics.py`, run the named test with
`python3 -m unittest test_metrics.<Class>.<test>`, expect a non-zero exit,
restore the file from a backup under `.radian/`.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| TK0252-M1 | `_radian_inputs`: a record marked `input_includes_cache: true` keeps all of `input` as uncached instead of subtracting `cache_read` | `JournalMetricsTest.test_a_radian_journal_yields_every_column` (`input_uncached` 111,000 instead of 110,800) | ✅ failed |
| TK0252-M2 | latency anchor moved to the request boundary: `_usage` measures from `_anchor` and an assistant message resets it before its usage record | `JournalMetricsTest.test_latency_uses_a_bucket_per_100k_prompt_tokens` (bucket 0 p50 0 ms instead of 2,000 ms) | ✅ failed |
| TK0252-M3 | pi's `bash` keeps its own column (`PI_TOOL_NAMES` not applied) | `JournalMetricsTest.test_a_pi_session_uses_the_same_columns_and_bash_is_shell` (`{'bash': 1}` instead of `{'shell': 1}`) | ✅ failed |
| TK0252-M4 | `_radian_inputs`: a record marked `input_includes_cache: true` subtracts only `cache_read`, leaving a cache write in the full-price input | `JournalMetricsTest.test_cache_accounting_follows_adr_0007` (uncached 800 instead of 500) | ✅ failed |
| TK0252-M5 | `_radian_inputs`: an unmarked record adds its `cache_read` into the prompt total again | `JournalMetricsTest.test_cache_accounting_follows_adr_0007` (unmarked total 1,200 instead of 1,000) | ✅ failed |
| TK0252-M6 | `_journal_metrics` delegates with `recursive=True`, so a nested JSONL artifact counts as a session journal | `JournalMetricsTest.test_journal_metrics_reads_only_the_session_directory` (`model_calls` 2 instead of 1) | ✅ failed |

## TK0253: repository task set

Base: `work/TK0253-repo-task-set` · method: apply the mutation to the named
script under `benchmarks/repo-tasks/`, run the named test with
`python3 benchmarks/repo-tasks/tests/test_repo_tasks.py <Class>.<test>`, expect a
non-zero exit, restore the file from a backup under `.radian/`.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| TK0253-M1 | `report.py` ignores `--baseline` (`build` sets `baseline = None`) | `ReportTest.test_ratios_divide_each_arm_by_the_baseline` (`KeyError: 'T1'` in `ratios`) | ✅ failed |
| TK0253-M2 | `check.sh` checks only the first crate (`cut -d, -f1`), so the second crate's clippy and nextest never run | `RunScriptTest.test_a_task_with_two_crates_runs_both_checks` (exit 0 instead of 1) | ✅ failed |
| TK0253-M3 | `run.sh` runs `git worktree add` in the caller's repository instead of the separate clone | `RunScriptTest.test_the_callers_repository_config_is_byte_identical` (the caller's `git worktree list` gains the run tree) | ✅ failed |

