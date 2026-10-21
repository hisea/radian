# scripts

- `worktree.sh --from SRC <path> [<branch-or-base>]`: `git worktree add` plus a
  warm `target/` cloned from still tree `SRC` (`$RADIAN_WARM_TARGET_FROM` when
  `--from` is absent; APFS clone on macOS, reflink copy on Linux). Refuses when
  `SRC` is building unless `--force`. `worktree.sh --drop-target <worktree>`
  removes only that worktree's `target/`.
