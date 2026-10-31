# `benchmarks/repo-tasks` mutation record

New rows use the id `<task id>-M<n>`, or the branch name without its prefix when
there is no task id; see "Mutation records" in `AGENTS.md`.

Method: apply the mutation to the named file, run
`python3 -m unittest discover -s benchmarks/repo-tasks/tests -k Leak`, expect a
failure, restore the file from a backup.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| repo-tasks-leak-rules-M1 | `leak.py` `_level`: the created branch name is not removed (`if created:` → `if False:`) | `LeakTest.test_a_branch_the_segment_creates_is_not_a_later_ref` (3 subtests: `switch -c`, `checkout -b`, `switch --create` give L3) | ✅ failed |
| repo-tasks-leak-rules-M2 | `leak.py` `git_level`: the unknown-name cap is removed | `LeakTest.test_a_later_name_git_did_not_know_is_level_one` | ✅ failed |
| repo-tasks-leak-rules-M3 | `leak.py` `unknown_to_git`: any `fatal:` lowers the level, whatever name it quotes | `LeakTest.test_a_later_name_git_showed_keeps_its_level` | ✅ failed |
| repo-tasks-check-builds-radian-M1 | `check.sh`: the `radian` binary build is skipped (`if false && grep ...`) | `CheckScriptTest.test_the_radian_binary_is_built_before_the_fixture_when_the_tree_has_both` | ✅ failed |

## TK0261: journal metrics compaction columns

Base: `work/TK0261-metrics-rereads-after-compaction` · method: apply the
mutation to `benchmarks/journal_metrics.py`, run the named test with
`cd benchmarks/terminal-bench/tests && python3 -m unittest
test_metrics.CompactionMetricsTest.<test>`, expect a non-zero exit, restore the
file from a backup under `.radian/`.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| TK0261-M1 | the re-read window is 6 model calls instead of 5 (`base + 5` → `base + 6`) | `CompactionMetricsTest.test_the_compaction_window_ends_after_five_model_calls` (6 re-reads instead of 5) | ✅ failed |
| TK0261-M2 | the "read before the compaction" check is removed (`path in earlier_paths` → `True`) | `CompactionMetricsTest.test_a_read_first_seen_after_the_compaction_is_not_a_reread` (2 re-reads instead of 1) | ✅ failed |
| TK0261-M3 | every `ContextCompacted` candidate counts, committed or not (no `TranscriptReplacementCommitted` match) | `CompactionMetricsTest.test_an_uncommitted_compaction_candidate_is_not_a_compaction` (2 re-reads instead of 0, 1 retained path instead of 0) | ✅ failed |
| TK0261-M4 | `compaction_seconds` starts at the first `CompactionUsageRecorded`'s own `ts` instead of the record before it | `CompactionMetricsTest.test_compaction_seconds_run_from_the_record_before_the_first_usage` (20 s instead of 30 s; also `test_the_totals_sum_and_recompute_the_compaction_columns`) | ✅ failed |

## TK0262: a second history counts only once the fetch succeeded

| # | Mutation | Caught by | Result |
|---|---|---|---|
| TK0262-M1 | `leak.py` `Scanner._pointed`: `remote add` returns level 3 again (`return 1, f"remote {name} -> {location}"` → `return 3, ...`) | `LeakTest.test_a_remote_add_then_a_failed_fetch_is_level_one_with_two_rows` (also `test_a_remote_add_alone_is_level_one`) | ✅ failed |
| TK0262-M2 | `leak.py` `Scanner._second`: the `fatal:` check is removed (`if result and FATAL in result:` and its return deleted) | `LeakTest.test_a_remote_add_then_a_failed_fetch_is_level_one_with_two_rows` (also `test_a_fetch_of_a_url_is_level_three_only_when_it_succeeds`, `test_a_clone_that_failed_is_level_one`) | ✅ failed |
| TK0262-M3 | `leak.py` `scan`: the remembered remote names are cleared between journals (`remotes.clear()` at the top of the loop over journals) | `LeakTest.test_remembered_remotes_span_the_runs_journals` | ✅ failed |
| TK0262-M4 | `leak.py` `Scanner._pointed`: `remote add` option values are not skipped (`_words(rest, REMOTE_OPTION_ARGS)` → `_words(rest)`) | `LeakTest.test_a_remote_add_with_option_arguments_names_the_remote` (4 subtests: `-t main`, `--track main`, `-m main`, `--master main`) | ✅ failed |
| TK0262-M5 | `leak.py` `scan`: calls are de-duplicated by `(name, arguments)` again instead of call id | `LeakTest.test_two_fetches_of_one_remote_keep_the_second_result` | ✅ failed |
| TK0262-M6 | `leak.py` `second_history`: `fetch`/`pull`/`clone` option values are not skipped (`_words(rest, SECOND_HISTORY_OPTION_ARGS.get(sub, frozenset()))` → `_words(rest)`) | `LeakTest.test_fetch_options_that_take_a_value_do_not_shadow_the_remote` (4 subtests) and `test_a_clone_with_options_names_the_repository` (`--depth 1 --reference /srv/ref`) | ✅ failed |

## TK0273: journal metrics memory columns

Base: `work/TK0273-memory-metrics` · method: apply the mutation to
`benchmarks/journal_metrics.py`, run the named test with
`cd benchmarks/terminal-bench/tests && python3 -m unittest
test_metrics.MemoryMetricsTest.<test>`, expect a non-zero exit, restore the
file from a backup under `.radian/`.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| TK0273-M1 | `_is_memory_path`: the component test is loosened to any path containing `memory` (`if "memory" in path: return True` before the component walk) | `MemoryMetricsTest.test_a_workspace_path_that_merely_contains_memory_is_not_a_memory_read` (`memory_reads` 1 instead of 0) | ✅ failed |
| TK0273-M2 | `_add_memory`: the success test is removed from `memory_writes` (`if not is_error and _is_memory_path(...)` → `if _is_memory_path(...)`) | `MemoryMetricsTest.test_a_denied_write_to_a_memory_path_is_not_a_memory_write` (`memory_writes` 1 instead of 0) | ✅ failed |

## TK0286: follow-ups for the leak scanner and the columns guard (2026-10-31)

Branch `work/TK0286-compaction-rereads-reserve-followups`. Each mutation was applied to the
named file, backed up under `.radian/mut/`, and the named test run outside the workspace
sandbox (the leak tests skip when the sandbox puts their temp tree inside a checkout).

| # | Mutation | Caught by | Result |
|---|---|---|---|
| TK0286-M6 | `leak.py` `FETCH_OPTION_ARGS` drops `-o` (the short `--server-option`) | `LeakTest.test_fetch_options_that_take_a_value_do_not_shadow_the_remote` (`git fetch -o foo origin` and `git pull -o foo origin main` stay level 1) | ✅ failed |
| TK0286-M7 | `leak.py` `scan` keeps the first representation of a call id again (`calls[position] = call` → `continue`) | `LeakTest.test_a_rewritten_call_is_scanned_with_its_final_arguments` (level 1 instead of 3: the rewrite's `git status` hides the `git fetch`) | ✅ failed |
| TK0286-M8 | `README.md`'s displayed-name list drops `compactions` and repeats `output` | `ColumnsDocTest.test_the_displayed_names_are_exactly_the_table_columns` (the list no longer equals `report.TABLE_COLUMNS`) | ✅ failed |

