# 构建缓存与 Verify 时间：worktree 秒开、测试并行

- 状态：第 1、2 项 decided（先做）；第 3、4 项 idea
- 涉及 ADR：无（改 AGENTS.md 的 Verify 命令与 CI 工作流）；第 4 项见 `verify-test-binaries.md`（新 ADR）
- 来源：`research/2026-10-15-build-timings.md`；本条背景里的实测

## 背景

两个痛点是同一个病根：每个 worktree 从零编译，`cargo test` 把测试二进制一个接一个跑。

一次 Linux gate（16 核、warm target）的分解：

| 步骤 | 墙钟 |
|---|---|
| build radian + mcp-fixture | 88 s |
| clippy workspace | 41 s |
| `cargo test --workspace` | 407 s |
| 其中：编译测试二进制 | 99 s |
| 其中：242 个二进制串行运行 | 301 s |
| 其中：`radian-tui/tests/pty.rs` | 95 s |
| 其中：`attach.rs` | 54 s |
| 其中：`exec_jobs.rs` | 23 s |
| contracts + schema | 29 s |

doc-test 15 次调用、0 个测试。GitHub CI 一次 run 约 16 min，其中约 8 min 在排队等 macOS runner，Linux job 本身 7 min。

worktree 的代价：Linux worker 上 18 个 worktree 各带 7 到 10 GB 的 target，合计约 100 GB；每开一个新 worktree 冷编译约 110 s。

实测结论：

- macOS：`cp -Rc`（APFS 克隆）一个 5.4 GB 的 warm target 3.4 s；新 worktree 用它跑 `cargo build -p radian-tui` 11 s，只重编 15 个 workspace crate，411 个依赖全部复用。依赖的 fingerprint 不含路径，workspace crate 的 `-C metadata` 含路径，所以 11 s 是地板。
- Linux worker 的 ZFS 关着 block cloning，`cp --reflink` 报 "Operation not permitted"；`cp -a` 2.8 GB 要 34 s。
- stable Cargo 支持 `build.build-dir`，但两个 worktree 共用一个 build-dir 时第二个打印 "Blocking waiting for file lock on build directory"：并发构建串行化。gate 树本来一次只跑一个，可以用；并行 worker 不能用。
- `pty.rs` 用进程内计数闸门 `RADIAN_PTY_PARALLEL`（缺省 3）限流；nextest 一个测试一个进程，这个闸门会失效。

## 1. worktree 秒开（decided，先做）

- 仓库加一个脚本：`git worktree add` 之后从一棵**静止的** warm 树克隆 target。macOS 用 APFS 克隆；Linux 先用 `cp --reflink=auto`，不支持时退回 `cp -a`。不从正在编译的树上克隆。
- Linux gate 脚本按当前 crate 名跑（它仍写着旧名 `agent-core`，两步 0 秒退出 rc=101 而脚本照样打印 DONE，于是最近一次 gate 没跑 journal-selfcheck）；任何一步 rc 非零就退出非零。这个脚本在仓库外，和本项一起修。
- 清掉已完工 worktree 的 target。

验收：新 worktree 第一次 `cargo build -p radian-tui` 在 macOS 上 ≤ 20 s；gate 脚本对改名的 crate 报错而不是 DONE。

## 2. nextest 进 Verify（decided，先做）

- Verify 的 `cargo test --workspace --locked --no-fail-fast` 与 `journal-selfcheck` 那一步改为 `cargo nextest run`；AGENTS.md、CI 工作流、Linux gate 脚本同时改。doc-test 为 0，不单独跑。
- `pty.rs` 的进程内闸门换成 nextest 的 `test-groups` + `max-threads`，并发数按机器核数定（16 核先试 6）；`RADIAN_PTY_PARALLEL` 随之删除。
- 用 nextest 的 per-test 超时（slow-timeout + terminate-after）替代各测试自带的看门狗，挂死的测试不再拖住整个 gate。
- 三条 provider contract 命令与 `--features testing --test contract` 的形状保持。

验收：Linux gate 的测试运行阶段从约 300 s 降到约 100 s（上限是最慢的那个二进制）；一个人为挂死的测试在超时后被杀并计失败；pty 测试在 6 并发下连续三次 gate 不 flake。

## 3. Linux 依赖缓存（idea）

- 首选在宿主打开 ZFS block cloning（`zfs_bclone_enabled`），让第 1 项的克隆在 Linux 上也是秒级。
- 开不了就在 Linux worker 装 sccache 本地缓存：依赖每台机器只编一次，workspace crate 照样重编；不支持增量，但依赖本来就不开增量。
- 共享 `build.build-dir` 只适合一次跑一个的 gate 树。

待决：宿主的 OpenZFS 版本是否 ≥ 2.2；sccache 缓存上限。

## 4. 合并集成测试二进制（idea）

见 `verify-test-binaries.md`：218 个 `tests/*.rs` 文件、242 个二进制，省的是编译那 99 s 和冷 worktree 的链接时间，不是运行时间，排在第 2 项之后。
