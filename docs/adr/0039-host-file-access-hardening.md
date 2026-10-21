# ADR 0039: Host File Access Hardening

Status: accepted

## Background

One shared root cause exists. The **host process (outside the sandbox) touches model-writable spots by path string**. ADR 0037 background jobs let the model change those paths concurrently between host "check" and "use". Tool-dispatch exclusive locks cannot hold already-running background processes.

## Decisions

### 1. Background job output moves out of the workspace

Output files live in the session own directory (`<sessions_dir>/<session-id>.jobs/<id>.out`). The session directory sits on the ADR 0020 sandbox-deny list. The model never sees or writes it. Dir mode is 0700. Opens use `O_CREAT|O_EXCL|O_NOFOLLOW` (Rust: `create_new(true)` plus `custom_flags(O_NOFOLLOW)`). The model sees only job id and the `job` tool, never paths. It reads background job output only through `job`. Truncated foreground command keeps use the same private directory with fd-level create/read. The model takes only opaque handles and reads through `job` (ADR 0037 decisions 3a and 4). Neither exposes paths. The existing workspace `.radian/jobs/` receives no more writes.

### 2. File tools never reopen by path string

Walks start from the workspace-root fd, level by level through `openat(..., O_NOFOLLOW | O_DIRECTORY)`. Any symlink level denies. The last level uses `openat(..., O_NOFOLLOW)` (`O_CREAT` on write, `O_EXCL` for new files). All reads and writes use that fd, never paths. Linux uses `openat2(RESOLVE_BENEATH|RESOLVE_NO_SYMLINKS)` when present, macOS uses stepwise `openat`. Legal in-workspace symlinks (pointing inside the workspace) are **never followed**: tools report one clear error row telling the model to use the target path. Directory-reading `read` with `grep`/`glob` likewise never follows symlinks out of the workspace.

Implementation: `crates/radian-coding/src/safe_fs.rs` `RootDir` (workspace-root `O_DIRECTORY` fd) plus stepwise `openat(dirfd, name, O_NOFOLLOW | O_DIRECTORY | O_CLOEXEC)`, last level `O_NOFOLLOW` (write `O_CREAT`, new files `O_EXCL`). `fstatat(AT_SYMLINK_NOFOLLOW)` and `fdopendir` list directories. Symlink parts read targets through `readlinkat` and open nothing through them. Relative targets normalize lexically against the parent. Absolute targets normalize through `realpath` (names only). Targets landing inside the workspace report as workspace-relative paths. Else they report only "symlink leaving the workspace". No outer paths print. `workspace.rs` `open_read`/`open_dir`/`open_write`/`relative_checked`/`checked_dir_path` all use it. `search.rs` `.gitignore`/`.ignore`/`.git/info/exclude` reads enter through the same fd. Host-side context files and skill bodies read with last-level `O_NOFOLLOW`. `@` mentions open through `RootDir`. macOS returns `ENOTDIR` (not `ELOOP`) for `O_NOFOLLOW|O_DIRECTORY` on links, so one `lstat` runs first, then judges. Non-Unix falls back to std path opens (docs note it).

### 3. Host `git` never runs repo-configured commands

**Every host-process git call** passes one shared helper (`crates/radian-coding/src/host_git.rs`), with `core.fsmonitor=false`, `core.hooksPath=/dev/null`, `core.attributesFile=/dev/null`, and every effective-config `filter.<name>.{clean,smudge,process}` command emptied (`required=false`). This covers prompt and footer git state (`prompt.rs`, `contrib.rs`). `radian-coding/tests/host_git.rs` fails on new bare `git` calls appearing in `crates/*/src`. `GIT_ATTR_SOURCE=<empty tree>` alone is too little (`$GIT_DIR/info/attributes` with `core.attributesFile` still applies), so it disables the commands themselves.

Host `git diff` (full diff and `--stat` both build argv in `crates/radian-coding/src/git.rs` `diff_argv`) also carries `--no-ext-diff --no-textconv`: one model-writable `.gitattributes` row `*.txt diff=<driver>` else makes the host run effective-config `diff.<driver>.command` (outer diff) or `diff.<driver>.textconv` (`GIT_EXTERNAL_DIFF` likewise). Both flags live at argv build, never in the shared helper. The helper also builds `git status`. That command rejects diff-only flags.

Read-only attribute calls (`git check-attr`) never hide `core.attributesFile`: that read learns the true driver, `core.attributesFile` belongs to the user, never model-writable. Hooks and fsmonitor still turn off.

Sandbox git skips this set by default. It runs as the sandbox user. The model controls the same files. ADR 0038 intentionally runs user-installed hooks and filters inside the sandbox. The exception is decision 5: untrusted projects harden `shell` children through env vars all the same.

### 4. Read-only roots: Rust deps, toolchain sources, in-binary docs

`read` (directory reads included) additionally takes three **read-only** roots. Roots are `$CARGO_HOME/registry/src` (default `~/.cargo/registry/src`), each toolchain `lib/rustlib/src` (`$RUSTUP_HOME/toolchains/*/lib/rustlib/src`, default `~/.rustup`), and in-binary docs (`radian://docs/`, `docs/user/` compiled into the binary). `grep` and `glob` take the first two. Virtual roots read only through `read`. `grep`/`glob` refuse and point at `read`. `write`, `patch`, and shell working directories stay unchanged. Virtual roots stay read-only.

- The first two access like the workspace: stepwise `openat(O_NOFOLLOW)` from that root fd, never following symlinks. Roots resolve once at session start (`radian-coding/src/read_roots.rs`).
- The third lives off filesystem: `radian://docs/<page>.md` reads from memory, `radian://docs/` lists pages. No symlink issue exists. `write`/`patch` refuse the `radian://` prefix.
- Disk paths write absolute or with `~`. Relative paths stay workspace paths. Docs write as `radian://docs/`.
- Other outside-workspace paths still refuse, and error hints list readable roots with doc roots.
- Missing, non-directory, or unopenable disk roots skip. `~/.cargo`/`~/.rustup` files outside `$CARGO_HOME/registry/src` and toolchain sources (such as `credentials.toml`) stay unreadable.

### 5. Untrusted projects harden `shell` git through env vars

Untrusted projects (ADR 0031) give every child of the `shell` tool the same decision-3 key table in other spelling. Children include background jobs and user `!` commands. Keys are `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_VALUE_<n>`. Values set `core.fsmonitor=false`, `core.hooksPath=/dev/null`, `core.attributesFile=/dev/null`. They empty commands with `required=false` for each effective filter name. Diff family: effective-config `diff.external` overrides to `true` when set. Each effective diff-driver `command` overrides to `true`. `textconv` overrides to `cat`. One key table only (`crates/radian-coding/src/host_git.rs` `config_pairs` builds both host `-c` and shell `GIT_CONFIG_*` spellings). Driver names look up live repo effective config each time.

Empty values never disable outer diff. Verified on git 2.53.0: empty overrides `diff.external=`, `diff.<name>.command=`, `diff.<name>.textconv=` all fail git with exit 128 (`cannot run : No such file...`). They never fall back to inner diff. So outer diff neutralizes with `true`. Patch hides but commands succeed. `--stat` runs as usual. Model-added `--no-ext-diff` still shows inner diff. textconv neutralizes with `cat`. Content passes through, text diff stays. `diff.external` overrides only when the repo really sets it. Static pushes would squash all patch display. Only really configured driver keys override. A driver with only textconv gains no `command`, or the added key hijacks its diff.

Trusted projects inject nothing: user pre-commit and similar hooks run as usual. Approved outside-sandbox calls (`outside_sandbox`) carry the same env: hardening follows project trust only, never sandbox paths. Users approve source commands, and repo-configured `core.fsmonitor` never stands as an approval object.

Injection lives at launcher level (`SandboxCommand::set_child_env`; the outside-sandbox call uses `std::process::Command::env`). It never lives inside command strings. macOS and transparent backends use process env. Linux `bwrap` uses `--setenv` args (`--clearenv` drops launcher env). Model commands can still override these vars. They guard repo-configured silent command runs, never the model.

## Constraints and Costs

- File tools never follow in-workspace symlinks (README states it).
- Background output and foreground kept output both live in the session directory, one job registry, one id space. The model reads by id only through `job` (ADR 0037 decisions 3a/4). Paths never show, returns stay bounded.
- Host git never runs repo-configured filters: LFS pointer files never unpack. Binary content needs user `git lfs checkout` in the workspace first, or a separate fixed command.
- Fd-level walks run full on Unix only. Non-Unix falls back to std path opens.

## Verification

- 1: pre-place symlinks at old and new spots, start background jobs, outer target files stay unchanged. Output files live in the session directory, modes 0600/0700. Sandbox reads never reach them. Mutations: drop `O_EXCL`/`O_NOFOLLOW`. Move output back to the workspace.
- 2: `tests/safe_fs.rs` six-tools × parent-symlink (outer/inner) matrix, three deterministic swap tests (swap parent dirs to outer-pointing links between lexical check and first `openat`, no sleeps), two message shapes (inner names targets, outer leaks no paths), traversal never follows links and keeps ignore rules. Mutations: one level drops `O_NOFOLLOW`. Last step reopens by path.
- 3: `.gitattributes` pointing at unallowed filters at no staged point makes the host run none (filter commands write marker files, tests assert markers absent). `tests/host_git.rs` scans sources and fails on new bare `git` calls. Mutations: fall back to `git add`. Read filter commands from repo config.
- 4: `tests/read_roots.rs` (`CARGO_HOME`/`RUSTUP_HOME`/`HOME` point at temp trees): registry sources and toolchain `lib/rustlib/src` read, `grep`/`glob` results carry root prefixes, `write`/`patch` to same paths refuse, `$CARGO_HOME/credentials.toml` refuses, outward symlinks under roots refuse, unrelated absolute-path refusals list readable roots with doc roots. `tests/docs.rs`: doc pages read/write with same paging as files, roots list pages, unknown pages list usable pages, `grep`/`glob` refusals point at `read`, `write`/`patch` refuse, total caps, tables match `docs/user/`. Mutations: open write. Open the whole `~/.cargo` directory. Drop doc-table pages. Fail embedded reads. Reword search refusals. Release write refusals.
- 5: untrusted projects pointing repo `.git/config` `core.fsmonitor` at marker-writing scripts run `git status` in `shell` with no markers (foreground, background jobs, and approved outside-sandbox calls each once). Repo `diff.<name>.textconv` (`.gitattributes` names drivers) runs `git diff` in `shell` with no markers and text diff still showing. `diff.external` or `diff.<name>.command` runs `git diff` with no markers and successful commands. Trusted projects run the same `pre-commit` hooks in `shell` `git commit` with markers as usual. Mutations: trusted injects too. Never injects. Drop `core.fsmonitor`, `diff.external`, `diff.<name>.command`, `diff.<name>.textconv` from inject tables (each mutation breaks only its own tests). Outside-sandbox walks restore no inject.
