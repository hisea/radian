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
