# radian-coding

Built-in coding tools for radian, bound to one workspace root. The file tools enforce the
workspace boundary themselves; `shell` exists **only when the host supplies a sandbox** and
never runs without one. Git is read-only. See the crate docs (`src/lib.rs`) for the security
contract; this file is the practical summary.

## Tools

| Tool | Concurrency | What it does |
|---|---|---|
| `read` | `Shared` | Read a UTF-8 file, bounded by `max_bytes` (default 64 KiB, hard cap 1 MiB). Every returned line is prefixed with its file line number, `N: ` (that prefix is not file content). `offset` (1-based first line) and `limit` (lines) read a large file in sections. Reports `start_line`/`end_line`/`line_truncated` and, when not finished, `next_offset`; a missing file names up to three close names in its directory; refuses non-UTF-8. A cargo-registry or toolchain source file may also be named as an absolute or `~` path (read-only; see below). |
| `write` | `Exclusive` | Create or overwrite a file with exact content (no newline added). Parent must exist. Never follows a symlink at the destination. |
| `patch` | `Exclusive` | Replace `old` with `new`. A match is located by layer (exact, then typographic normalisation, then line-by-line ignoring trailing whitespace) and must be unique unless `replace_all` is set. A CRLF file is matched on an LF view and stays CRLF. Zero matches refuse the edit and report the closest region with line numbers. |
| `list` | `Shared` | List a directory, sorted, with kind and size; symlinks shown but not followed; capped by `max_entries`. A cargo-registry or toolchain source directory may also be named absolutely or with `~` (read-only). |
| `glob` | `Shared` | Find files whose **workspace-relative path** matches a glob (`src/**/*.rs`); sorted; `.gitignore` respected, `.git` skipped, symlinks never followed; capped by `max_results`. A read-only registry/toolchain base works too, and its results are absolute paths. |
| `grep` | `Shared` | Regex search file contents; `path:line:text` per match, `path-line-text` for `context` lines, `--` between groups; `.gitignore` respected, binary files skipped; capped per file and in total. A read-only registry/toolchain base works too, and its results are absolute paths. |
| `git_status` | `Shared` | `git status --porcelain=v1 --branch --untracked-files=all`. |
| `git_diff` | `Shared` | `git diff [--cached] [-- <path>]`; the path must stay inside the workspace. |
| `shell` | `Exclusive` | `/bin/sh -c <command>` **through a sandbox**, with `cwd` (workspace-relative), `timeout_secs`, `max_output_bytes` and `background`. Present only when the host built the tools with a `Sandbox`. `background: true` returns at once with a job id (no default timeout); the three `job_*` tools below read, list and stop it (ADR 0037). Its output goes to the session's own jobs directory, outside the workspace and unreadable from inside the sandbox (ADR 0039 decision 1). |
| `job_output` | `Shared` | Read what a background `shell` job produced since the last call, plus a `[status: …]` line. `wait: true` blocks until the job ends or `timeout_ms` (default 30 s, max 600 s); a wait timeout does **not** kill the job. The increment is a bounded ring; the full output file is readable only through this tool, by line with `tail_lines`, `from_line`/`max_lines` or `pattern` (capped like `grep`), each naming the file's total line count. |
| `job_list` | `Shared` | List this session's background jobs: id, command first line, state (running / exited n / signal n / killed / timed out), elapsed. |
| `job_kill` | `Shared` | SIGTERM the job's whole process group, then SIGKILL after 2 s. A settled job is a no-op. |
| `ask_user` | `Exclusive` | Ask the user one or more structured questions (single/multi/text) and wait for the answers. Its **own** provider (`AskTools`, not part of `CodingTools`): a host mounts it only when a human is there to answer, and it exists whether or not the host has a workspace (ADR 0024 Q5). The wait races the turn's cancellation and never fabricates an answer. |

`write`/`patch` are `Exclusive` because they mutate files: running them alone keeps each
edit's read-modify-write atomic with respect to the other tools and avoids two writers
racing on one file. The read-only tools are `Shared` so a batch of reads/diffs can overlap.
`ask_user` is `Exclusive` (the default) so a session has at most one pending question: a batch
with two calls asks them one after the other instead of opening two modals.

`read` addresses **lines**: `offset` is the 1-based first line, `limit` caps the line count,
and `max_bytes` still bounds the result. A byte cut that would land mid-line drops the whole
incomplete line (the result's `next_offset` names it), so a caller can loop
`offset = next_offset` and never see half a line; the one exception is a first line longer than
the whole budget, whose UTF-8 prefix is returned with `line_truncated: true` and the next read
continues at the following line. The body ends with a hint for a provider that only reads text
(the adapters never send `structured` to the model): `[truncated: continue with offset=NNN]`,
or `[line N truncated at max_bytes; continue with offset=N+1]` for that exception. Memory is
bounded by about `max_bytes` plus one reader buffer, even for a file with a single enormous
line. This is the fix for dogfood10 D25 (a 73 KB guide read 20 times): read the section you
need, do **not** raise `max_bytes` to reach it.

### Line numbers and a missing file

Every line `read` returns carries its **file** line number as `N: `, so a read at `offset`
3 starts at `3: `, and the continuation hint (`[truncated: …]`) is not numbered. The prefix
is not file content: the description says to strip it before using the text anywhere else,
in particular as `patch`'s `old`. `bytes_read` still counts the file bytes, not the prefix.

A `read` of a missing file keeps the `not_found` code but, when the requested directory
holds names close to the one asked for (case-insensitive edit distance), appends
`Did you mean …?` with up to three of them. Only the requested file's own directory is
listed, after canonicalization, so a suggestion never points outside the workspace.

### `patch` matching layers

`patch` locates `old` in three layers, tried in order, and stops at the first that matches:

1. **exact** — `old` appears byte for byte;
2. **typographic normalisation** — curly quotes, en/em dashes and no-break spaces are mapped
   to ASCII on **both** sides before comparing;
3. **line-by-line** — a block of whole lines agrees once each line's trailing whitespace is
   stripped; a needle ending in `\n` has that newline included in the replaced range.

A layer only *locates* text; the replacement is always `new`, and the bytes replaced are the
file's own. A match at any layer must be unique; more than one is still the `patch_ambiguous`
error, and the file is untouched. `replace_all: true` replaces every occurrence the winning
layer located. A CRLF file is matched on an LF view, offsets map back, and the file (and the
replacement) stays CRLF. When no layer matches, the `patch_not_found` error names the closest
region of the file with line numbers, so the model can correct `old` without another `read`.

### Argument errors

Every tool parses its arguments through `src/args.rs` against its own published
`input_schema`. A bad call returns one `invalid_arguments` failure that lists **every**
problem (a missing required argument, a wrong type, an unknown argument), echoes the
arguments received (bounded to 4 KiB), and ends with `Update the arguments and call the tool
again`. An **optional** argument sent as `""` is treated as absent — a frequent DeepSeek
shape for "use the default"; a **required** `""` is kept, because `write`'s `content` and
`patch`'s `new` may legitimately be empty. `minimum`/`maximum` stay the tools' own checks so
a limit that is exceeded keeps its semantic code (`read_limit_exceeded`, `glob_limit_exceeded`,
…).

## Search (`glob`, `grep`)

`glob` and `grep` are `Shared`, so a batch of searches overlaps instead of queueing behind one
another (the reason they exist: `shell` is `Exclusive`, and the safe tier has no shell at all).
Both walk from a base (`path`, default the workspace root) opened through the workspace root's
directory fd (ADR 0039 decision 2: every component `openat(O_NOFOLLOW | O_DIRECTORY)`), and the
walk itself descends by fd and hands matched files back as open fds; nothing is re-opened by
path. The per-entry rules, which `list`/`read` do not have because they look at one path:

- `.gitignore` (and `.ignore`) files **inside** the tree are respected, with or without a git
  repository; no ignore file **above** the base and no global gitignore is consulted, so the
  same tree gives the same result on every machine;
- the `.git` directory is pruned;
- symlinks are **never** followed — a symlinked file is not a match and a symlinked directory is
  not descended — so a link out of the workspace is never read;
- dot-files other than `.git` are included.

`glob` matches the model's pattern against each workspace-relative path (`*` does not cross a
`/`, `**` does), sorts, and cuts to `max_results` with a `[truncated: showing N of M matches]`
line. `grep` matches each line with a regex (an invalid one is an `invalid_regex` tool error),
skips binary files (a NUL byte in the first 8 KiB), and prints `path:line:text`, context lines
as `path-line-text`, and `--` between non-contiguous groups. A line longer than 8 KiB is matched
on its prefix and reported with a `[truncated]` marker, so one enormous line cannot grow the
result. Matches are capped per file (`default_grep_matches_per_file`) and in total
(`max_matches`), each cut named in the text.

## Read-only roots outside the workspace (ADR 0039)

`read`, `grep`, `glob` and `list` also read two roots outside the workspace, **read-only**:

- the cargo registry sources, `$CARGO_HOME/registry/src` (default `~/.cargo/registry/src`);
- every installed toolchain's `$RUSTUP_HOME/toolchains/*/lib/rustlib/src` (default `~/.rustup`).

A model names one as an **absolute** path or with `~` (a relative path is always
workspace-relative). `CodingTools` resolves the roots from the process environment once, when
the tool set is built (`CodingTools::new` / `with_sandbox*`), canonicalizes each root itself and
holds it as a directory fd; the configured spelling is kept as an alias, so a path written the
way `CARGO_HOME` was set still matches when canonicalization rewrites it (`/tmp` →
`/private/tmp`). A path under a root is opened from that root's fd with the same per-component
`openat(O_NOFOLLOW)` walk the workspace uses, so **a symlink at any level is refused**, and
`grep`/`glob` results are rendered with the root's prefix (absolute) so they can be passed
straight back to `read`. A source that does not exist is simply not readable.

`write`, `patch` and `shell`'s `cwd` never accept these roots: a `write`/`patch` destination is
resolved against the workspace only. Any other absolute path is refused
(`path_outside_workspace`) with a message naming both readable roots. `tests/read_roots.rs` sets
`CARGO_HOME`/`RUSTUP_HOME`/`HOME` to a private tree once per binary and never reads the
developer's real `~/.cargo`/`~/.rustup`; `tests/common`'s `tools()` builds with
`ReadRoots::none()` so the boundary tests stay hermetic.

## Limits (`Limits`, all overridable)

| Field | Default | Why |
|---|---|---|
| `max_read_bytes` / `default_read_bytes` | 1 MiB / 64 KiB | A tool result goes straight into the model context and the session JSONL; a whole large file must not. |
| `max_write_bytes` | 8 MiB | Bound a single write so a runaway model cannot fill the disk in one call. |
| `max_patch_bytes` | 4 MiB | Patch holds the file in memory twice; the cap keeps that bounded. |
| `max_list_entries` / `default_list_entries` | 5000 / 500 | Bound the listing result. |
| `max_glob_results` / `default_glob_results` | 5000 / 500 | Bound one `glob` result set. |
| `max_grep_matches` / `default_grep_matches` | 5000 / 200 | Bound `grep`'s total matches. |
| `default_grep_matches_per_file` | 50 | One file must not fill the result (not model-settable). |
| `max_grep_context` | 10 | Bound the context lines a single match can add. |
| `max_git_output_bytes` | 1 MiB | Bound git output, truncated on a character boundary with a marker. |
| `git_timeout` | 10 s | A hung git must not hang the turn. |
| `default_shell_output_bytes` / `max_shell_output_bytes` | 64 KiB / 1 MiB, **per stream** | Aligned with `read`'s default and hard cap. Past the budget the first quarter and the last three quarters are kept and the middle is replaced by a marker naming the discarded bytes; the tail is larger because a failing build's error is at the end. `max_output_bytes` lets the model raise it, clamped to the hard cap. Discarded bytes are reported per stream in the text and in `structured.*_truncated_bytes`. |
| `default_shell_timeout` / `max_shell_timeout` | 60 s / 600 s | A model-supplied `timeout_secs` is clamped to the maximum, and the clamp is reported. A command that needs longer uses `background: true` (no default timeout). |
| `MAX_RUNNING_JOBS` / `JOB_RING_BYTES` | 4 / 64 KiB | At most four background jobs run at once (a fifth start is a tool error); each holds a process group, a 0600 `.out` file and a bounded ring. The ring only has to cover one `job_output` gap; the file keeps the full history for the host and `job_output` is the model's only read path (ADR 0039 decision 1). |

## What is enforced, and what is not

Enforced (each with tests in `tests/escaping.rs` and `tests/mutation.rs`):

- absolute paths and `..` are refused before any filesystem access (`path_outside_workspace`);
- `read`/`grep`/`glob`/`list` additionally read two **read-only** roots outside the workspace
  (ADR 0039): `$CARGO_HOME/registry/src` and every
  `$RUSTUP_HOME/toolchains/*/lib/rustlib/src`. An absolute or `~` path under one is opened from
  that root's fd with every component `O_NOFOLLOW`; a path under none of them is refused with a
  message naming both. `write`/`patch` and `shell`'s `cwd` never accept them
  (`tests/read_roots.rs`);
- **every path is opened through the workspace root's directory fd** (`safe_fs`, ADR 0039
  decision 2): one `openat(dirfd, name, O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC)` per parent, the
  leaf with `O_NOFOLLOW` (`O_CREAT` for `write`), and the tool reads or writes through that fd.
  No tool re-opens a path string after a check, so a parent directory swapped for a symlink
  while the tool runs is refused, not followed (`tests/safe_fs.rs`: a hook between the lexical
  check and the first `openat` performs the swap; no sleeps);
- **a symlink anywhere in the path is refused, inside or outside the workspace.** A link whose
  target stays inside is `path_is_symlink` and the message names the target as a
  workspace-relative path (``docs/current` is a symlink to `docs/v2`; use that path`), so the
  model retries in one step; a link that leaves the workspace, or whose target cannot be
  resolved, is `path_outside_workspace` and the message never prints the outside path. The
  target is read with `readlinkat` on the same fd and never opened. **Behaviour change
  (2026-10-16): a symlink inside the workspace is no longer followed by the file tools; use the
  target path.**
- a symlinked workspace root works (the root is canonicalized once, then held as an fd);
- `write`/`patch` check the open fd to be a regular file with one hard link **before**
  truncating (`hard_link_refused`); `list`/`glob`/`grep` classify entries with
  `fstatat(AT_SYMLINK_NOFOLLOW)` and descend by fd, so a walk never follows a link out.

**Not** enforced — stated, not hidden:

- **Hard links cannot be read-protected.** A pre-existing hard link inside the workspace to
  an outside file is readable; only `write`/`patch` refuse it (via `nlink > 1`). The tools
  cannot create links, so the link must already exist.
- **Hard links inside the workspace are followed for reads** (the fd walk sees a regular
  file; see above).
- **Sensitive contents are not filtered.** Reading a secret file puts it in the session
  JSONL (ADR 0006 §11). Treat session files as sensitive.
- **`git` is a child process** and can read `~/.gitconfig` and the repository. It is told
  not to take optional locks; tests assert `.git` is unchanged, but no sandbox wraps it.
- Windows: `safe_fs` falls back to std's path-based opens with no symlink defence and no link
  count; the fd walk is Unix-only (tested on macOS; Linux uses the same per-component
  `openat`, `openat2(RESOLVE_BENEATH)` is not used).
- **`shell`'s confinement is exactly the sandbox's** (tests in `tests/shell.rs`). Under
  `workspace` writes are confined to the workspace, the fake sensitive path in the test
  is unreadable and the network is on (ADR 0016); **the macOS backend leaves `mach-lookup`
  open, so the IPC boundary is not tightened and this is not full credential isolation**
  (ADR 0012). `full` runs the command unconfined; it is only reachable because the caller
  named it when building the sandbox, and the tool description says "NOT sandboxed".
  Independently of the mode, the tool kills the whole process group on timeout,
  cancellation and leader exit, and filters the environment to the sandbox whitelist, so a
  model-controlled shell never inherits the host's API keys. Output is read as a stream:
  only a bounded head and a trimmed tail are retained, so a command that prints gigabytes
  cannot grow this process's memory (50 MiB through a 64 KiB budget grows peak RSS by about
  688 KiB, measured).

## Mounting

- In `agent-cli`: `--workspace <dir>` exposes the eight file/git/search tools, plus `shell` when
  the profile built a sandbox (`workspace`/`full`). Without the flag nothing changes.
- Directly: `CodingTools::new(root)` returns a `ToolProvider` (no shell); pass it to
  `Agent::new(model, tools)`.
- With a sandbox: build one with `radian_sandbox::Sandbox::new(mode, root)` (an enforced mode
  probes the backend and fails if it cannot prove enforcement) and pass it to
  `CodingTools::with_sandbox(root, sandbox)`. The shell tool then appears in `tool_specs()`.
- With a human: `AskTools::new(Arc<dyn Prompter>)` is the `ask_user` provider. It is separate
  from `CodingTools` because a host with no `--workspace` has no coding tools but still has a
  human to ask (ADR 0024 Q5). The `Prompter` is implemented by the host (the TUI modal in
  ADR 0024 Q7); `radian-host`'s `QueuedPrompter` puts the question on the shared pending queue.
  The answer is a tool result, never user input: the question is the call's `arguments` and the
  answer its final result in the session journal (ADR 0002 E1).

## 评审补充（合并时由评审者追加，2026-10-12）

评审者自己写了三组逃逸尝试（在 `tests/escaping.rs` 末尾，已保留），**全部被拒绝**：

1. **同级目录名以根名开头**（根 `…/ws`，目标 `…/ws-evil`）：绝对路径与 `../ws-evil/…` 两种写法、`read` 与 `write` 四种组合都被拒，且文件内容未变。这是"用字符串前缀而不是路径组件做包含检查"的经典漏洞；本实现用 `Path::starts_with`（按组件比较），所以不受影响——这条测试把它钉住，以免有人日后改成字符串比较。
2. **经符号链接出去再 `..` 回来**（`sub/link/../sentinel.txt`、`sub/link/.`、`sub/../../outside`）：全部被拒，说明解析发生在包含检查之前。
3. **经符号链接的父目录创建新文件**（目标尚不存在，无法 canonicalize）：被拒，且工作区外确实没有被创建出文件。

已知局限（硬链接读、父目录 TOCTOU 窗口）与 crate 文档所述一致，评审者未另行发现被掩盖的问题。

**一处与本 crate 无关、但复核时撞上的真问题**：`agent-cli` 的 `every_exit_reaps_the_mcp_children` 在机器高负载下（当时 load average 51，三个并行构建）超过了测试里 30 秒的上限而失败；单独重跑 5 次全过（3.7–13 秒）。**不是回归，是这个超时对负载敏感**。修法留给后续切片（放宽上限或改成不依赖墙钟的等待）。
