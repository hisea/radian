# 构建缓存与 Verify 时间：Linux 依赖缓存、合并测试二进制

- 状态：第 3、4 项 idea（第 1 项 worktree 脚本与第 2 项 nextest 已落地）
- 涉及 ADR：无；第 4 项见 `verify-test-binaries.md`（新 ADR）
- 来源：`research/2026-10-15-build-timings.md`；本条目里的实测

## 背景

瓶颈在两处：每个 worktree 从零编译，`cargo test` 把测试二进制一个接一个跑。

一次 16 核 warm target 机器的分解：

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

实测结论（第 3 项用）：

- Linux worker 的 ZFS 关着 block cloning，`cp --reflink` 报 "Operation not permitted"；`cp -a` 2.8 GB 要 34 s。
- stable Cargo 支持 `build.build-dir`，但两个 worktree 共用一个 build-dir 时第二个打印 "Blocking waiting for file lock on build directory"：并发构建串行化。一次只跑一个的树可以用；并行 worker 不能用。

实测结论（第 4 项用）：`cargo test --workspace --no-run` 链出 162 个可执行文件（144 个 integration-test、15 个 lib-test、3 个 bin-test）；测试二进制的编译与链接约占全部编译时间的四成，改一行 leaf crate 也要重链它的全部测试二进制。

## 3. Linux 依赖缓存（idea）

- 首选在宿主打开 ZFS block cloning（`zfs_bclone_enabled`），让第 1 项的克隆在 Linux 上也是秒级。
- 开不了就在 Linux worker 装 sccache 本地缓存：依赖每台机器只编一次，workspace crate 照样重编；不支持增量，但依赖本来就不开增量。
- 共享 `build.build-dir` 只适合一次跑一个的 gate 树。

待决：宿主的 OpenZFS 版本是否 ≥ 2.2；sccache 缓存上限。

## 4. 合并集成测试二进制（idea）

见 `verify-test-binaries.md`：218 个 `tests/*.rs` 文件、242 个二进制，省的是编译那 99 s 和冷 worktree 的链接时间，不是运行时间，排在第 2 项之后。
