# radian-coding

Built-in coding tools for radian, bound to one workspace root. The file tools enforce the
workspace boundary themselves; `shell` exists **only when the host supplies a sandbox** and
never runs without one. There is no git tool: the model runs git through `shell` (hardened
via `GIT_CONFIG_*` in untrusted projects, ADR 0039 decision 5). See the crate docs
(`src/lib.rs`) for the security contract; this file is the practical summary.

## Tools

| Tool | Concurrency | What it does |
|---|---|---|
| `read` | `Shared` | Read a UTF-8 file, a directory, or an image, bounded by `max_bytes` (default 64 KiB, hard cap 1 MiB). Every returned text line is prefixed with its file line number, `N: ` (that prefix is not file content). `offset` (1-based first line) and `limit` (lines) read a large file in sections. Reports `start_line`/`end_line`/`line_truncated` and, when not finished, `next_offset`; a missing file names up to three close names in its directory; refuses non-UTF-8. **An image** (PNG/JPEG/GIF/WebP by magic number, never the extension) returns a text line plus the image part: raw bytes ≤ 5 MiB and each side ≤ 8000 px (`image_too_large`), `offset`/`limit` are `image_offset`, and a model the catalog knows takes no images gets only the text line with the omission sentence. A file under a read-only root (cargo-registry or toolchain sources, or a discovered skill's directory) may also be named as an absolute or `~` path (read-only; see below); in the `full` tier any other absolute or `~` path reads too, except the sandbox's deny set. A directory path lists it instead: sorted entries with kind and size, symlinks shown but not followed; `limit` then caps entries (default 500, hard cap 5000), `offset`/`max_bytes` are ignored and said once. |
| `write` | `Exclusive` | Create or overwrite a file with exact content (no newline added). Parent must exist. Never follows a symlink at the destination. |
| `patch` | `Exclusive` | Replace `old` with `new`, or apply an `edits` array atomically. A match is located by layer (exact, then typographic normalisation, then line-by-line ignoring trailing whitespace) and must be unique unless `replace_all` is set. With `edits`, every edit is located in the original file, ranges must be disjoint, and if any edit fails nothing is written. A CRLF file is matched on an LF view and stays CRLF. Zero matches refuse the edit and report the closest region with line numbers. |
| `glob` | `Shared` | Find files whose **workspace-relative path** matches a glob (`src/**/*.rs`); sorted; `.gitignore`/`.ignore` respected (workspace root down to the search base), `.git` skipped, symlinks never followed; capped by `max_results`. A read-only root base (registry/toolchain sources, a skill's directory) works too, as does any other absolute or `~` path in the `full` tier except the sandbox's deny set, and its results are absolute paths. |
| `grep` | `Shared` | Regex search file contents; `path:line:text` per match, `path-line-text` for `context` lines, `--` between groups; `.gitignore`/`.ignore` respected (workspace root down to the search base), binary files skipped; capped per file, in total matches, and in total output bytes (32 KiB, context lines dropped before matches, each cut named in the text). `output: "files"` lists one line per matching file with its count, `output: "count"` prints only the totals; the default `content` is unchanged. A read-only root base (registry/toolchain sources, a skill's directory) works too, as does any other absolute or `~` path in the `full` tier except the sandbox's deny set, and its results are absolute paths. |
| `shell` | `Exclusive` | `/bin/sh -c <command>` **through a sandbox**, with `cwd` (workspace-relative), `timeout_secs`, `max_output_bytes` and `background`. Present only when the host built the tools with a `Sandbox`. `background: true` returns at once with a job id (no default timeout, and `timeout_secs` is ignored — ADR 0037: bound the command itself or stop it with `job`); `job` below is the only way to read it (ADR 0037 decision 4). Its output goes to the session's own jobs directory, outside the workspace and unreadable from inside the sandbox (ADR 0039 decision 1). The model runs git through `shell`; in a project the user has not trusted, every shell child carries the host git hardening as environment (`GIT_CONFIG_*`, ADR 0039 decision 5). |
| `job` | `Shared` | Read or stop one `shell` job by its job id (`read`, `wait` or `kill`). Plain `read` returns the output since the last call plus a `[status: …]` line; `tail_lines`, `from_line`/`max_lines` or `pattern` page the full file by line. `wait` blocks (default 30 s, max 600 s) and never kills; `kill` SIGTERMs the job's process group, then SIGKILLs after 2 s. A truncated foreground result is a settled entry under a normal job id — the same reader. No list tool: job ids are already in context. |
| `ask_user` | `Exclusive` | Ask the user one or more structured questions (single/multi/text) and wait for the answers. Its **own** provider (`AskTools`, not part of `CodingTools`): a host mounts it only when a human is there to answer, and it exists whether or not the host has a workspace (ADR 0024 Q5). The wait races the turn's cancellation and never fabricates an answer. |

`write`/`patch` are `Exclusive` because they mutate files: running them alone keeps each
edit's read-modify-write atomic with respect to the other tools and avoids two writers
racing on one file. The read-only tools are `Shared` so a batch of reads/diffs can overlap.
`ask_user` is `Exclusive` (the default) so a session has at most one pending question: a batch
with two calls asks them one after the other instead of opening two modals.

`read` on a directory lists it (`limit` caps entries); removed names (`git_diff`, `list`,
`git_status`, `job_output`, `job_list`, `job_kill`, `shell_output`) are unknown tools.

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
error, naming up to 5 occurrence line numbers with their line text (then `and N more`), and the file is untouched. `replace_all: true` replaces every occurrence the winning
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

## Images

One pipeline (`src/image.rs`) turns file or host bytes into `ContentPart::Image`: the kind
comes from the magic number (PNG, JPEG, GIF, WebP; BMP and everything else are not images),
then the limits (raw bytes ≤ `MAX_IMAGE_BYTES` = 5 MiB, each side ≤ `MAX_IMAGE_SIDE` = 8000),
then the base64 part with its display facts in `annotations` (`kind`, `name`, `width`,
`height`, `bytes`). No decode, no re-encode, no local resize; `imagesize` reads the header.
`read`, the TUI's `@path`/dropped path and `exec --image` all call it. `read` returns
`Read image file [<mime>] W×H` plus the part, adds the provider-scaling sentence over
`SCALE_NOTICE_SIDE` (1568 px), and, when the host-supplied `ImageSupport` says the current
model takes no images, returns only the text with the omission sentence. A `read` image travels
to the model only on the Anthropic route (its tool result carries the block); the OpenAI
adapters' tool-result channels are text, so they carry `[<mime> image omitted]` instead. The
TUI's transcript line is `[image: <name>, W×H, N KB]`; `--export` keeps
`[<mime> image omitted]`.

## Search (`glob`, `grep`)

`glob` and `grep` are `Shared`, so a batch of searches overlaps instead of queueing behind one
another (the reason they exist: `shell` is `Exclusive`).
Both walk from a base (`path`, default the workspace root) opened through the workspace root's
directory fd (ADR 0039 decision 2: every component `openat(O_NOFOLLOW | O_DIRECTORY)`), and the
walk itself descends by fd and hands matched files back as open fds; nothing is re-opened by
path. The per-entry rules, which a directory `read` does not have because it looks at one path:

- `.gitignore` (and `.ignore`) files are respected with or without a git repository, from
  the workspace root down through the search base: a nearer directory's rules win, as in
  git, so a repository-root ignore keeps working when the search starts in a subdirectory.
  The global gitignore is not consulted, so the same tree gives the same result on every
  machine;
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
(`max_matches`), each cut named in the text. Total output is bounded by `max_grep_output_bytes`
(32 KiB): past it the tool first drops context lines to keep every match line (with the `--`
group separators readable) and only then cuts whole match lines in order, ending with one note
naming the byte limit, what was dropped, and how to narrow the search (a more specific `pattern`,
`path` or `glob`, or `output: "files"`). `output: "files"` prints one line per matching file with its match count
and `output: "count"` prints only the totals (`N matches in M files`); the per-file, total-match
and total-byte caps apply in every mode, and the default `content` output is unchanged.

## Read-only roots outside the workspace (ADR 0039)

`read`, `grep` and `glob` also read roots outside the workspace, **read-only**:

- the cargo registry sources, `$CARGO_HOME/registry/src` (default `~/.cargo/registry/src`);
- every installed toolchain's `$RUSTUP_HOME/toolchains/*/lib/rustlib/src` (default `~/.rustup`);
- each discovered skill's directory (the directory holding its `SKILL.md`), in both tiers.

In the `full` tier they also read any other absolute or `~` path, except the sandbox's deny
set (the `DEFAULT_DENY_READ` entries under `$HOME`, plus the sessions directory, `memory/`,
`credentials/` and `trust/` under `$RADIAN_HOME`). The deny check runs on the `realpath`ed
path, so a symlink into a denied directory refuses; the file itself opens with `O_NOFOLLOW`
on its last component. A refusal names the denied path and says it is in the sandbox's
deny set.

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
resolved against the workspace only. In the `workspace` tier any other absolute path is refused
(`path_outside_workspace`) with a message naming the readable roots, including the skill
directories. `tests/read_roots.rs` sets
`CARGO_HOME`/`RUSTUP_HOME`/`HOME` to a private tree once per binary and never reads the
developer's real `~/.cargo`/`~/.rustup`; `tests/common`'s `tools()` builds with
`ReadRoots::none()` so the boundary tests stay hermetic.

## Limits (`Limits`, all overridable)

| Field | Default | Why |
|---|---|---|
| `max_read_bytes` / `default_read_bytes` | 1 MiB / 64 KiB | A tool result goes straight into the model context and the session JSONL; a whole large file must not. |
| `max_write_bytes` | 8 MiB | Bound a single write so a runaway model cannot fill the disk in one call. |
| `max_patch_bytes` | 4 MiB | Patch holds the file in memory twice; the cap keeps that bounded. |
| `max_list_entries` / `default_list_entries` | 5000 / 500 | Bound a directory `read` (entry cap). |
| `max_glob_results` / `default_glob_results` | 5000 / 500 | Bound one `glob` result set. |
| `max_grep_matches` / `default_grep_matches` | 5000 / 200 | Bound `grep`'s total matches. |
| `max_grep_output_bytes` | 32 KiB | Bound `grep`'s total output bytes (match and context lines plus notes); past it context lines are dropped before matches, then whole match lines, ending with one note naming the limit, what was dropped, and how to narrow the search. |
| `default_grep_matches_per_file` | 50 | One file must not fill the result (not model-settable). |
| `max_grep_context` | 30 | Bound the context lines a single match can add; a larger request is cut to it (with a `[context cut to N lines]` line), not refused. |
| `default_shell_output_bytes` / `max_shell_output_bytes` | 24 KiB / 1 MiB, **per stream** | Past the budget the middle is replaced by a marker naming the discarded bytes. Success keeps the first quarter and the last three quarters; a failed command (non-zero exit, signal kill, timeout, cancellation) keeps the first eighth and the last seven eighths of the same budget, so the error at the end survives. The exit status is only known at the end, so the capture keeps the maximum of both splits (a `9/8`-budget buffer) and trims to the right one at render time. `max_output_bytes` lets the model raise it, clamped to the hard cap. Discarded bytes are reported per stream in the text and in `structured.*_truncated_bytes`. |
| `default_shell_timeout` / `max_shell_timeout` | 300 s / 600 s | A foreground command never moves to the background on its own: background happens only with an explicit `background: true`. A model-supplied `timeout_secs` is clamped to the maximum, and the clamp is reported. A command that needs longer uses `background: true` (no deadline) or a larger `timeout_secs` (up to 600). |
| `MAX_RUNNING_JOBS` / `JOB_RING_BYTES` | 4 / 64 KiB | At most four background jobs run at once (a fifth start is a tool error); each holds a process group, a 0600 `.out` file and a bounded ring. The ring only has to cover one `job` gap; the file keeps the full history for the host and `job` is the model's only read path (ADR 0039 decision 1). A truncated foreground result registers as a settled entry in the same registry without taking a slot (ADR 0037 decision 3a). |

## What is enforced, and what is not

Enforced (each with tests in `tests/escaping.rs` and `tests/mutation.rs`):

- absolute paths and `..` are refused before any filesystem access (`path_outside_workspace`);
- `read`/`grep`/`glob` additionally read **read-only** roots outside the workspace
  (ADR 0039): `$CARGO_HOME/registry/src`, every
  `$RUSTUP_HOME/toolchains/*/lib/rustlib/src`, and each discovered skill's directory. An absolute or `~` path under one is opened from
  that root's fd with every component `O_NOFOLLOW`; in the `full` tier any other absolute or
  `~` path reads too, except the sandbox's deny set (checked on the `realpath`ed path); in
  the `workspace` tier a path under none of the roots is refused with a
  message naming them (skill directories included). `write`/`patch` and `shell`'s `cwd` never accept them
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
  a symlink inside the workspace is no longer followed by the file tools; use the
  target path.**
- a symlinked workspace root works (the root is canonicalized once, then held as an fd);
- `write`/`patch` check the open fd to be a regular file with one hard link **before**
  truncating (`hard_link_refused`); a directory `read`/`glob`/`grep` classify entries with
  `fstatat(AT_SYMLINK_NOFOLLOW)` and descend by fd, so a walk never follows a link out.

**Not** enforced — stated, not hidden:

- **Hard links cannot be read-protected.** A pre-existing hard link inside the workspace to
  an outside file is readable; only `write`/`patch` refuse it (via `nlink > 1`). The tools
  cannot create links, so the link must already exist.
- **Hard links inside the workspace are followed for reads** (the fd walk sees a regular
  file; see above).
- **Sensitive contents are not filtered.** Reading a secret file puts it in the session
  JSONL (ADR 0006 §11). Treat session files as sensitive.
- **`git` runs through `shell`, not through a tool of its own.** In a project the user has
  not trusted, every shell child carries the host git hardening as environment
  (`GIT_CONFIG_*`: `core.fsmonitor=false`, `core.hooksPath=/dev/null`,
  `core.attributesFile=/dev/null`, emptied filter commands — ADR 0039 decision 5), so
  repository configuration cannot run commands through it; `tests/shell_git_hardening.rs`
  pins the marker-file behaviour for foreground and background jobs. In a trusted
  project nothing is injected, so the user's own hooks run.
- Windows: `safe_fs` falls back to std's path-based opens with no symlink defence and no link
  count; the fd walk is Unix-only (tested on macOS; Linux uses the same per-component
  `openat`, `openat2(RESOLVE_BENEATH)` is not used).
- **`shell`'s confinement is exactly the sandbox's** (tests in `tests/shell.rs`). Under
  `workspace` writes are confined to the workspace, the fake sensitive path in the test
  is unreadable and the network is on (ADR 0016); **the macOS backend leaves `mach-lookup`
  open, so the IPC boundary is not tightened and this is not full credential isolation**
  (ADR 0012). `full` runs the command unconfined; it is only reachable because the caller
  named it when building the sandbox, and the tool description says "NOT sandboxed".
  Independently of the mode, the tool kills the whole **session** (every process the command
  started, including a child that left its process group, like GNU `timeout`) on timeout,
  cancellation and leader exit, and filters the environment to the sandbox whitelist, so a
  model-controlled shell never inherits the host's API keys. Output is read as a stream:
  only a bounded head and a trimmed tail are retained, so a command that prints gigabytes
  cannot grow this process's memory (50 MiB through the default per-stream budget stays far
  under the test's 16 MiB RSS-growth ceiling).

## Mounting

- In `radian-exec`: `--workspace <dir>` exposes the file/search tools (`read`, `write`,
  `patch`, `glob`, `grep`), plus `shell` and `job` when the profile built a sandbox (`workspace`/`full`). Without the flag nothing changes.
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

**一处与本 crate 无关、但复核时撞上的真问题**：`radian-exec` 的 `every_exit_reaps_the_mcp_children` 在机器高负载下（当时 load average 51，三个并行构建）超过了测试里 30 秒的上限而失败；单独重跑 5 次全过（3.7–13 秒）。**不是回归，是这个超时对负载敏感**。修法留给后续切片（放宽上限或改成不依赖墙钟的等待）。
