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

## TK0262: a second history counts only once the fetch succeeded

| # | Mutation | Caught by | Result |
|---|---|---|---|
| TK0262-M1 | `leak.py` `Scanner._pointed`: `remote add` returns level 3 again (`return 1, f"remote {name} -> {location}"` → `return 3, ...`) | `LeakTest.test_a_remote_add_then_a_failed_fetch_is_level_one_with_two_rows` (also `test_a_remote_add_alone_is_level_one`) | ✅ failed |
| TK0262-M2 | `leak.py` `Scanner._second`: the `fatal:` check is removed (`if result and FATAL in result:` and its return deleted) | `LeakTest.test_a_remote_add_then_a_failed_fetch_is_level_one_with_two_rows` (also `test_a_fetch_of_a_url_is_level_three_only_when_it_succeeds`, `test_a_clone_that_failed_is_level_one`) | ✅ failed |
| TK0262-M3 | `leak.py` `scan`: the remembered remote names are cleared between journals (`remotes.clear()` at the top of the loop over journals) | `LeakTest.test_remembered_remotes_span_the_runs_journals` | ✅ failed |
| TK0262-M4 | `leak.py` `Scanner._pointed`: `remote add` option values are not skipped (`_words(rest, REMOTE_OPTION_ARGS)` → `_words(rest)`) | `LeakTest.test_a_remote_add_with_option_arguments_names_the_remote` (4 subtests: `-t main`, `--track main`, `-m main`, `--master main`) | ✅ failed |
| TK0262-M5 | `leak.py` `scan`: calls are de-duplicated by `(name, arguments)` again instead of call id | `LeakTest.test_two_fetches_of_one_remote_keep_the_second_result` | ✅ failed |
| TK0262-M6 | `leak.py` `second_history`: `fetch`/`pull`/`clone` option values are not skipped (`_words(rest, SECOND_HISTORY_OPTION_ARGS.get(sub, frozenset()))` → `_words(rest)`) | `LeakTest.test_fetch_options_that_take_a_value_do_not_shadow_the_remote` (4 subtests) and `test_a_clone_with_options_names_the_repository` (`--depth 1 --reference /srv/ref`) | ✅ failed |
