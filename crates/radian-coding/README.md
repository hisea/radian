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

## 评审补充（合并时由评审者追加，2026-10-12）

评审者自己写了三组逃逸尝试（在 `tests/escaping.rs` 末尾，已保留），**全部被拒绝**：

1. **同级目录名以根名开头**（根 `…/ws`，目标 `…/ws-evil`）：绝对路径与 `../ws-evil/…` 两种写法、`read` 与 `write` 四种组合都被拒，且文件内容未变。这是"用字符串前缀而不是路径组件做包含检查"的经典漏洞；本实现用 `Path::starts_with`（按组件比较），所以不受影响——这条测试把它钉住，以免有人日后改成字符串比较。
2. **经符号链接出去再 `..` 回来**（`sub/link/../sentinel.txt`、`sub/link/.`、`sub/../../outside`）：全部被拒，说明解析发生在包含检查之前。
3. **经符号链接的父目录创建新文件**（目标尚不存在，无法 canonicalize）：被拒，且工作区外确实没有被创建出文件。

已知局限（硬链接读、父目录 TOCTOU 窗口）与 crate 文档所述一致，评审者未另行发现被掩盖的问题。

**一处与本 crate 无关、但复核时撞上的真问题**：`agent-cli` 的 `every_exit_reaps_the_mcp_children` 在机器高负载下（当时 load average 51，三个并行构建）超过了测试里 30 秒的上限而失败；单独重跑 5 次全过（3.7–13 秒）。**不是回归，是这个超时对负载敏感**。修法留给后续切片（放宽上限或改成不依赖墙钟的等待）。
