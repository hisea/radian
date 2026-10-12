# `radian-coding` mutation record (Phase 3 coding tools)

Base: `0a8dda4` · one-shot worktree `/tmp/tools-mut` (created from HEAD, deleted after) ·
every mutation run with `cargo test -q -p radian-coding --no-fail-fast`; `git checkout --`
after each; `git status --short` empty at the end.

| # | Requirement mutated | Mutation | Result |
|---|---|---|---|
| M1 | 1 absolute path escape | remove **both** overlapping absolute-path checks (`is_absolute()` and the `Component::RootDir` arm) | ✅ caught (`an_absolute_read_path_is_refused`, `an_absolute_path_inside_the_workspace_is_still_refused`, `write_absolute_and_dotdot_paths_are_refused`) |
| M2 | 2 `..` escape | drop `Component::ParentDir` from the component check | ✅ caught (`parent_dir_escapes_are_refused`) |
| M3 | 3 symlink escape (read) | remove `ensure_inside` from `resolve_existing` | ✅ caught (`a_symlink_pointing_outside_is_refused_for_read`) |
| M4 | 4/9 symlinked parent (write) | remove `ensure_inside` from `resolve_parent_for_write` | ✅ caught (`write_into_an_outside_symlinked_directory_is_refused`) |
| M5 | 5 write through an existing link | drop the `guard_mutation_target` call in `write` | ✅ caught (`write_through_a_symlink_to_outside_is_refused`, `a_link_swapped_in_after_a_write_is_refused_by_the_next_write`) |
| M6 | 6 hard link | make the `nlink > 1` check unreachable | ✅ caught (`write_and_patch_refuse_a_hard_linked_file`) |
| M7 | 7 TOCTOU (`O_NOFOLLOW`) | remove `custom_flags(O_NOFOLLOW)` from the write open | ❌ **not caught** — see below |
| M8 | 8 symlinked workspace root | stop canonicalizing the root in `WorkspaceRoot::new` | ✅ caught (`a_symlinked_workspace_root_works_and_still_confines`) |
| M9 | read size limit | make the `max_bytes > max_read_bytes` check unreachable | ✅ caught (`read_refuses_a_budget_above_the_hard_cap`) |
| M10 | read size limit | ignore the byte budget (`.take(u64::MAX)`) | ✅ caught (`read_reports_truncation_and_the_total_size`) |
| M11 | `patch` auditability | let multiple matches fall through and edit anyway | ✅ caught (`patch_refuses_an_ambiguous_needle_and_leaves_the_file_alone`) |
| M12 | `.git` must not be written | remove **both** `--no-optional-locks` and `GIT_OPTIONAL_LOCKS=0` | ✅ caught (`git_status_and_diff_report_the_worktree_and_leave_dot_git_untouched`) |

## The two mutations that were not behaviour changes

- **M7 (`O_NOFOLLOW`)**: the destination symlink is already refused by `guard_mutation_target`
  before the open, so removing `O_NOFOLLOW` changes nothing a test can see. It is
  **defence in depth** against the destination being swapped for a link between the guard
  and the open; no deterministic test can produce that race, so this slice does not claim
  it is covered. The parent-directory window is not mitigated at all.
- **M1 first attempt**: removing only `is_absolute()` was an *equivalent* mutation, because
  the `Component::RootDir` arm of the second check refuses absolute paths too. Both must be
  removed to change behaviour. The record keeps this because it is the honest picture: the
  requirement is enforced by one of two overlapping checks, not by `is_absolute()` alone.

## A mutation that needed both mechanisms removed

**M12**: `--no-optional-locks` and `GIT_OPTIONAL_LOCKS=0` are equivalent switches; removing
either alone leaves git lock-free, so only removing both made the `.git` snapshot change
(the test's repo has a stale index stat after the modification, which is exactly when git
would refresh and write `.git/index`).

## Coverage added because of this pass

`an_absolute_path_inside_the_workspace_is_still_refused` was added: the rule is
"workspace-relative only", so an absolute path that happens to point inside must be refused
too. Without it, the absolute-path requirement had no test that distinguished it from the
canonicalization check.

## `shell` tool (Phase 3 sandbox slice)

Base: `fd0388d` (re-run at the final tests commit) · one-shot worktree `/tmp/shell-mut` (created from HEAD, deleted after) ·
each mutation run with `cargo test -p radian-coding --test shell <test> -- --exact
--no-fail-fast`; files restored after each; `git status --short` empty at the end; no orphan
processes left (`ps` check).

| # | Requirement mutated | Mutation | Result |
|---|---|---|---|
| M1 | no sandbox ⇒ no shell, no naked run | the `None` execute arm runs `/bin/sh -c` directly instead of refusing | ✅ caught (`shell_is_absent_without_a_sandbox`) |
| M2 | timeout/cancel kill the whole process group | `killpg(pid)` → `kill(pid)` (only the direct child) | ✅ caught (`a_timeout_kills_the_whole_process_group`; the background `sleep 600` survives) |
| M3 | output cap | `take_from_budget` returns `want` and ignores the budget | ✅ caught (`output_over_the_cap_is_truncated_and_reported`) |
| M4 | environment filtered in every mode | remove the `env_clear` + whitelist block for the transparent backend | ✅ caught (`the_host_environment_does_not_reach_the_shell`) |
| M5 | `cwd` stays inside the workspace | `root.resolve_existing(dir)` → `root.root().join(dir)` (no canonicalize/containment) | ✅ caught (`cwd_escapes_are_refused`) |
| M6 | `full-access` is never a default | `CodingTools::new` builds a transparent `FullAccess` sandbox internally | ✅ caught (`full_access_is_never_the_default`) |
| M7 | a failing backend never downgrades | `Sandbox::from_backend` falls back to a transparent sandbox when `probe()` fails | ✅ caught (`an_unavailable_backend_never_yields_a_sandbox_or_a_shell`) |

7/7 caught; no misses. M2 is the one that shows the "kill the direct child only" bug is a
real hazard: with it the background `sleep 600` is still alive after the timeout.
