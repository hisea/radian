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
| TK0253-M4 | `lib.py` drops arc's field checks: a report with only `blocking` parses (no string `summary`, no item lists) | `ReviewTextTest.test_a_report_missing_arcs_fields_is_unparsed` (`True is not false`) | ✅ failed |
| TK0253-M5 | `review.sh` sets `review_ok` from the parsed block alone, ignoring the exit code, the timeout and the session outcome | `ReviewScriptTest.test_a_nonzero_reviewer_exit_fails_even_with_a_valid_report` (exit 0 instead of 1, `review_ok` true) | ✅ failed |
| TK0253-M6 | `report.py` emits only `bytes.<tool>` columns, dropping `calls.<tool>` | `ReportTest.test_a_per_tool_call_column_joins_its_byte_column` (`'calls.read' not found` in the columns) | ✅ failed |
| TK0253-M7 | `run.sh` drops the clone check, so `REPO_TASKS_CLONE=$PWD` runs `git worktree add` in the caller's repository | `RunScriptTest.test_a_clone_pointing_at_the_callers_repository_is_refused` (exit 0 instead of 2) | ✅ failed |
| TK0253-M8 | `check.sh` appends `$JOBS` after `cargo fmt -- --check`, so rustfmt receives `--jobs` | `CheckScriptTest.test_max_jobs_reaches_cargo_and_not_rustfmt` (fmt exits 1, checks fail) | ✅ failed |
| TK0253-M9 | `run.sh` runs the agent without `timeout_run.py`, so a machine without coreutils has no wall-time cap | `RunScriptTest.test_the_cap_does_not_depend_on_gtimeout` (`timed_out` false after the cap) | ✅ failed |

## TK0256: eval-metrics follow-ups

Base: `work/TK0256-eval-metrics-followups` · method: apply the mutation to the
named file under `benchmarks/`, run the named test (`cd benchmarks/terminal-bench/tests
&& python3 -m unittest test_metrics.<Class>.<test>` for the first two rows,
`python3 benchmarks/repo-tasks/tests/test_repo_tasks.py <Class>.<test>` for the
rest), expect a non-zero exit, restore the file from a backup under `.radian/`.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| TK0256-M1 | a pi `compaction` record does not note its timestamp, so a compaction cannot bound `wall_seconds` | `JournalMetricsTest.test_a_pi_compaction_bounds_the_session_time` (`wall_seconds` 0.0 instead of 35) | ✅ failed |
| TK0256-M2 | the command line prints a zero row for a path that names no journal instead of exiting 1 | `JournalMetricsTest.test_a_missing_file_is_an_error_that_names_it`, `test_a_missing_directory_is_an_error_that_names_it`, `test_a_directory_with_no_journal_after_the_prefix_is_an_error` (exit 0 instead of 1) | ✅ failed |
| TK0256-M3 | the text table keeps the old header aliases (`uncached_in`, `read_ranged`, `read_20kb`, `det_compactions`, `wall_s`) | `JournalMetricsTest.test_text_table_headers_use_the_fixed_column_names` (8 headers not a `COLUMNS` name) | ✅ failed |
| TK0256-M4 | `review.sh` parses the JSONL stdout for a radian reviewer instead of its session journal | `ReviewScriptTest.test_a_radian_reviewer_report_is_read_from_its_session` (exit 1, `parsed` false) | ✅ failed |
| TK0256-M5 | `run.sh` refuses every directory inside the caller or source, not only a repository or worktree root | `RunScriptTest.test_a_plain_directory_inside_the_caller_is_allowed_as_the_clone` (exit 2, "not a separate clone") | ✅ failed |
| TK0256-M6 | `timeout_run.py` installs no signal handler, so an interrupt kills only the wrapper | `TimeoutRunTest.test_an_interrupt_is_forwarded_to_the_command` (exit -15 instead of 143; the command outlives the wrapper) | ✅ failed |
| TK0256-M7 | `_inputs_without_journals` ignores the prefix, so a directory whose only journals miss it is not rejected by the new check and only the old empty-report fallback exits 1 | `JournalMetricsTest.test_a_directory_with_no_journal_after_the_prefix_is_an_error` (the old "no journal file found under:" message, no prefix detail) | ✅ failed |

