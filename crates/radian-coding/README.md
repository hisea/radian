# radian-coding

Built-in coding tools for radian, bound to one workspace root. **No shell, no arbitrary
command execution, read-only git.** See the crate docs (`src/lib.rs`) for the security
contract; this file is the practical summary.

## Tools

| Tool | Concurrency | What it does |
|---|---|---|
| `read` | `Shared` | Read a UTF-8 file, bounded by `max_bytes` (default 64 KiB, hard cap 1 MiB). Reports `total_bytes`/`truncated`; refuses non-UTF-8. |
| `write` | `Exclusive` | Create or overwrite a file with exact content (no newline added). Parent must exist. Never follows a symlink at the destination. |
| `patch` | `Exclusive` | Replace exactly one occurrence of `old` with `new`. Zero or multiple matches refuse the edit and leave the file untouched (no fuzzy matching). |
| `list` | `Shared` | List a directory, sorted, with kind and size; symlinks shown but not followed; capped by `max_entries`. |
| `git_status` | `Shared` | `git status --porcelain=v1 --branch --untracked-files=all`. |
| `git_diff` | `Shared` | `git diff [--cached] [-- <path>]`; the path must stay inside the workspace. |

`write`/`patch` are `Exclusive` because they mutate files: running them alone keeps each
edit's read-modify-write atomic with respect to the other tools and avoids two writers
racing on one file. The read-only tools are `Shared` so a batch of reads/diffs can overlap.

## Limits (`Limits`, all overridable)

| Field | Default | Why |
|---|---|---|
| `max_read_bytes` / `default_read_bytes` | 1 MiB / 64 KiB | A tool result goes straight into the model context and the session JSONL; a whole large file must not. |
| `max_write_bytes` | 8 MiB | Bound a single write so a runaway model cannot fill the disk in one call. |
| `max_patch_bytes` | 4 MiB | Patch holds the file in memory twice; the cap keeps that bounded. |
| `max_list_entries` / `default_list_entries` | 5000 / 500 | Bound the listing result. |
| `max_git_output_bytes` | 1 MiB | Bound git output, truncated on a character boundary with a marker. |
| `git_timeout` | 10 s | A hung git must not hang the turn. |

## What is enforced, and what is not

Enforced (each with tests in `tests/escaping.rs` and `tests/mutation.rs`):

- absolute paths, `..`, symlink escapes, symlinked-parent escapes are refused with the
  single code `path_outside_workspace`;
- a symlinked workspace root works (the root is canonicalized once);
- `write`/`patch` refuse a symlink at the destination (`path_is_symlink`) and a
  hard-linked destination (`hard_link_refused`);
- the write open uses `O_NOFOLLOW` as defence in depth.

**Not** enforced — stated, not hidden:

- **Hard links cannot be read-protected.** A pre-existing hard link inside the workspace to
  an outside file is readable; only `write`/`patch` refuse it (via `nlink > 1`). The tools
  cannot create links, so the link must already exist.
- **TOCTOU is only partly mitigated.** The final component is protected by `O_NOFOLLOW`; a
  parent directory swapped for a symlink between canonicalization and the open is not
  caught.
- **Sensitive contents are not filtered.** Reading a secret file puts it in the session
  JSONL (ADR 0006 §11). Treat session files as sensitive.
- **`git` is a child process** and can read `~/.gitconfig` and the repository. It is told
  not to take optional locks; tests assert `.git` is unchanged, but no sandbox wraps it.
- Windows: no `O_NOFOLLOW` and no link count; the extra checks are Unix-only.

## Mounting

- In `agent-cli`: `--workspace <dir>` exposes these six tools (no shell). Without the flag
  nothing changes.
- Directly: `CodingTools::new(root)` returns a `ToolProvider`; pass it to
  `Agent::new(model, tools)`.
