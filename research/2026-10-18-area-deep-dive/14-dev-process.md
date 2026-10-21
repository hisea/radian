# 深挖 14：开发流程与工程健康——Radian 与五个对手

2026-10-18 · Radian `hisea/radian@356df1e`（本机是浅克隆；完整历史从 GitHub 树状克隆到 scratchpad 里统计，共 2190 个提交）· 对手源码：codex `8c07694`（行号以 `git show HEAD:` 为准）、opencode `63c1a27`、pi-mono `482c138`；omp 和 Claude Code 只有 npm 包和文档。发版数据来自 npm registry（`registry.npmjs.org/<包名>` 的 `time` 字段，2026-10-18 抓取）。对手的 GitHub API（stars、Actions 用时）在本会话被代理拒绝（403），所以 CI 用时**一律未核实**。本文没有跑完整的 Verify，只跑了 `cargo metadata --no-deps` 和一些统计命令。

## 1. 一句话结论

Radian 的工程纪律在六家里最严：每个决定都有 ADR，每个新测试都要做变异检查并记账（23 个 `MUTATIONS*.md`，约 1495 条"抓住"），有 provider 契约套件，有 journal 自检。但这套纪律**全部压在一台机器、一条 6–7 分钟的全量 Verify 上**。它没有 CI，没有分层验证，没有发过版。ADR 有 86 万字节，合并重写的规则也没有执行完。按每天约 88 次合并、每次一遍全量估算，光 Verify 每天就要占约 9 小时的机器时间（推断）。最值得做的几件事都很便宜：分层验证脚本、pre-commit 格式检查、把 7 个只放 `#[ignore]` 的 perf 测试移出默认构建、ADR 索引加重写，再加一条只在 main 上跑的 Linux + macOS CI。合并测试二进制收益最大，但风险也最大，要单独立一个 ADR。

## 2. 对比表

| 维度 | Radian | pi | Codex CLI | Claude Code | omp | opencode |
|---|---|---|---|---|---|---|
| 决策记录 | 43 个编号 ADR，7282 行，860 KB，全部中文（`docs/adr/`）；AGENTS.md 要求改代码前先读相关 ADR | 没有 ADR；规则写在 `AGENTS.md`（125 行），变更写在各包的 `CHANGELOG.md`（`AGENTS.md:104-111`） | 没有 ADR 目录；设计写在模块文档和局部 `AGENTS.md` 里（例如 `tui/src/bottom_pane/AGENTS.md`："keep the docs in sync"）；`codex-rs/docs/protocol_v1.md` | 闭源，无从得知 | 只有 npm 包；`CHANGELOG.md` 1676 行 | `specs/v2/` 6 篇共 1265 行，分"现行契约 / 决策记录 / 历史"三类，明确写着"Do not add implementation checklists here"（`specs/v2/README.md`）；另有 18 个 `AGENTS.md` |
| 测试规模 | 3135 个 `#[test]`（集成 1984，内联约 1151）；184 个 `tests/*.rs`；`cargo metadata` 统计出 200 个测试 target | 663 个测试文件；`test/suite/harness.ts` 加 faux provider，不调真实 API（`AGENTS.md:38`） | 157 个 crate 一共只有 282 个 `tests/*.rs`；8 个 crate 用 `tests/all.rs` 把测试聚成一个二进制（`core/tests/all.rs:3`） | 未知 | 未知 | 992 个 `*.test.ts` |
| 快照 / 契约测试 | 手写 golden 81 个（`radian-tui/tests/golden`、`radian-host/tests/golden`），只有一处用 `UPDATE_GOLDEN` 重新生成；provider 契约套件跑 3 个 adapter（`crates/agent-provider-contract/README.md`） | MCP 一致性测试，和 baseline 比较（`ci.yml` 的 `mcp-conformance`） | 1598 个 insta `.snap` | 未知 | 未知 | Storybook 组件测试、Playwright e2e（`AGENTS.md:169`） |
| CI | **没有**：没有 `.github/`，私有仓库 Actions workflow 数为 0（`gh api .../actions/workflows`） | 一个 ubuntu job 跑 build、check、test，外加 MCP 一致性；`cancel-in-progress`（`.github/workflows/ci.yml`） | PR 只跑快检查（Bazel test/clippy、fmt、`cargo shear`）；合并后再跑 nextest 分片矩阵和 release 构建；BuildBuddy 远程缓存（`.github/workflows/README.md`） | 未知 | 未知 | turbo `--affected` 只测受影响的包，Linux 和 Windows 各一遍，每步上限 20 分钟（`test.yml`） |
| flaky 处理 | pty 测试的等待上限统一成 60 s，同时最多跑 3 个（`PTY_SLOTS`，`radian-tui/tests/pty.rs:204-255`）；review 时容易 flaky 的测试要跑 5 遍（`AGENTS.md:88`）；41 个提交标题里出现 "flak" | `test.sh` 隔离 HOME、凭据和临时目录 | nextest `retries = 1`，`slow-timeout = 30s`，用 test-groups 限制并发（`codex-rs/.config/nextest.toml`） | 未知 | 未知 | 测试 skill 记录了"what has made tests slow or flaky in CI"（`AGENTS.md:169`） |
| 构建配置 | `[profile.dev] debug = "line-tables-only"`，依赖不带调试信息（`Cargo.toml`，��；没有 release profile | 不适用（TS） | dev 也用 line-tables-only，另有 `dev-small`、`ci-test`、`profiling` 几个 profile，release 开 thin LTO（`codex-rs/Cargo.toml:623-655`） | 未知 | 不适用 | 不适用 |
| 发版节奏（npm，2026-10-18） | **0 次**；15 个 crate 都是 `0.1.0`，没有 tag，没有 CHANGELOG，只能自己 `cargo build` | `@earendil-works/pi-coding-agent` 1.3.0：近 30 天 15 次，近 90 天 27 次 | `@openai/codex` 0.163.0：正式版近 30 天 16 次，近 90 天 35 次（含预发布，30 天内共 938 个版本） | `@anthropic-ai/claude-code` 2.1.302：近 30 天 24 次，近 90 天 75 次 | `@oh-my-pi/pi-coding-agent` 18.12.1：近 30 天 54 次，近 90 天 133 次 | `opencode-ai` 1.19.3：正式版近 30 天 8 次，近 90 天 36 次 |
| 贡献流程 | 私有仓库，单一 owner；工作由 agent 在 worktree 里完成：合并的分支里 `pi/` 147 个、`oc/` 71 个、`dogfood/` 45 个；2190 个提交中 558 个带 Claude co-author | 新贡献者的 issue 和 PR 默认自动关闭，维护者回复 `lgtm` 才放行；"Using AI to write code is fine. Submitting AI-generated slop … is not"（`CONTRIBUTING.md:15-34`）；husky pre-commit 跑 `npm run check` | "We do not accept external code contributions"（`docs/contributing.md`） | 闭源；二手报道称约 80% 的代码由 Claude Code 自己写（[officechai](https://officechai.com/ai/80-of-claude-codes-code-is-written-by-claude-code-anthropic-lead-engineer/)，二手，未核实） | 未知 | 新功能先开 issue 等设计批准（`CONTRIBUTING.md:82`）；`pr-standards` 机器人检查 PR；`/review` 评论会触发 opencode 自动审 PR（`review.yml`） |
| agent 的验证分层 | 只有一层：全量 Verify 7 条命令，要求全部通过 | agent 改完代码只跑 `npm run check`，"Never run `npm run build` or `npm test` unless requested"，测试按文件跑（`AGENTS.md:32-37`） | `just test` 等于 nextest `--no-fail-fast`；`just fix -p` 只修单个 crate（`justfile:57-88`） | 未知 | 未知 | 根目录禁止跑测试（`package.json` 的 `"test": "echo 'do not run tests from root' && exit 1"`），只在受影响的包里跑 `bun typecheck`（`AGENTS.md:168-174`） |
| 最大的源文件 | 产品代码：`agent-core/src/turn.rs` 2363 行；测试：`radian-tui/tests/pty.rs` **12869 行** | `interactive-mode.ts` 7127 行 | `chat_composer.rs` 12828 行 | — | `agent-session.ts` 14110 行 | 未统计 |

## 3. 每家的关键做法（只写和开发流程有关的）

### pi
- **agent 默认只跑最便宜的那一层。** 改完代码跑 `npm run check`（格式、lint、类型检查），不跑测试；要测试就按文件跑，或者用 `./test.sh` 跑所有非 e2e 测试（`pi-mono/AGENTS.md:32-37`）。全量测试交给 CI。
- **pre-commit 把关。** husky 的 `pre-commit` 先跑 `check-lockfile-commit.mjs`，再跑 `npm run check`（`.husky/pre-commit`），格式问题根本进不了提交。
- **变更日志有固定格式。** 新条目一律写进 `## [Unreleased]`，已经发布的版本节不许改（`AGENTS.md:104-111`）。近 30 天发了 15 个版本。
- **用 issue 准入。** 新贡献者默认被自动关闭，维护者回复 `lgtmi` 或 `lgtm` 才放开（`CONTRIBUTING.md:23-34`）。

### Codex CLI
- **PR 只跑快检查，全量放到合并之后。** PR 上的 `rust-ci.yml` 有意保持很小（fmt、`cargo shear`、argument-comment-lint），完整的 clippy 矩阵、nextest 分片和 release 构建都放在 post-merge（`.github/workflows/README.md`），规则写的是"Keep `rust-ci.yml` fast enough that it usually does not dominate PR latency"。
- **测试聚合成一个二进制。** `core/tests/all.rs` 的注释写着"Single integration test binary that aggregates all test modules"。157 个 crate 一共 282 个集成测试文件；Radian 16 个 crate 就有 184 个。
- **flaky 进配置，不靠人记。** nextest 设 `retries = 1`，"so one transient failure does not fail full-CI outright"；资源敏感的测试用 `test-groups` 限并发（`max-threads = 1/4`），并且注明"Higher concurrency causes integration test timeouts under resource contention on common developer machines"（`codex-rs/.config/nextest.toml`）。
- **没有 ADR，也不收外部 PR**（`docs/contributing.md`）。决策靠团队内部，加上模块文档。

### Claude Code（来源：文档和二手报道）
- 闭源，开发流程不可见。npm 上近 30 天发了 24 个版本（registry 数据），频率很高。
- 二手报道称约 80% 的代码由 Claude Code 自己写（[officechai](https://officechai.com/ai/80-of-claude-codes-code-is-written-by-claude-code-anthropic-lead-engineer/)、[letsdatascience](https://letsdatascience.com/blog/claude-writes-80-percent-of-anthropics-code-brake-pedal)），未核实。

### omp
- 只能看到 npm 包：近 30 天 54 个版本，近 90 天 133 个，是六家里最快的；`CHANGELOG.md` 有 1676 行。CI 和测试都看不到。

### opencode
- **只测受影响的部分。** CI 先用 `turbo ls --affected` 算出受影响的包，再跑 `bun turbo test --affected --concurrency=3`（`.github/workflows/test.yml`）。根目录的 `test` 脚本直接报错退出，逼着 agent 到具体的包里去跑。
- **规格文档按"职责"分类。** `specs/v2/README.md` 把文档分成三类：现行契约（Session、Tools）、决策记录（标明 "Accepted and implemented"）、历史上下文，并且规定"GitHub issues own active work; Git history preserves removed plans"。一共 6 篇、1265 行，对应 Radian 的 43 篇、7282 行。
- **agent 也进了 CI。** 在 PR 下评论 `/review` 会调用 `opencode run` 审查代码（`review.yml:54`）；`/opencode` 评论会触发 GitHub Action（`opencode.yml`）。测试规范写成 skill，放在 `.opencode/skills`（`AGENTS.md:169`）。

## 4. Radian 的优势

1. **决策可追溯，而且有强制力。** 每个约束代码的决定都在 ADR 里，"推翻一个决定必须写新 ADR"（`AGENTS.md:7`）。对手里只有 opencode 的 `specs/v2` 稍微接近（只有 2 篇决策记录）。对一个主要由 agent 写的仓库，这是防止 agent 改着改着悄悄违背设计的最有效手段。
2. **测试质量有证据，不只是数量多。** 新测试必须先在坏代码上失败一次，再做变异检查，结果记账：23 个 `MUTATIONS*.md` 共 6512 行，约 1495 条"抓住"、26 条"没抓住"，并且用 `.gitattributes` 的 union 合并驱动避免冲突。五家对手都没有看得到的类似机制。
3. **契约测试，加上 journal 自检。** provider 契约套件规定了两个"逃生口"，并且明令"Never invent a wire value"（`AGENTS.md:40-42`）；`journal-selfcheck` 让测试里的每次 transcript 写入都和 journal 重放结果对一遍。这是 Rust 运行时这一层的硬保证。
4. **用 agent 开发 agent，而且是有组织的。** 有 worktree 和分支规则、brief 模板、"先写盘再继续"的规则、review 时的变异检查（`AGENTS.md` "Rules for delegating to pi"）。历史里有 147 个 `pi/` 合并和 45 个 `dogfood/` 合并，Radian 用自己开发自己（提交里有 "done by radian exec … reviewed by p1"）。产出量是 2190 个提交，集中在 2026-10-12 到 10-17 这 6 天，平均每天约 365 个。
5. **构建配置已经调过一次。** `debug = "line-tables-only"`、依赖不带调试信息＼�，和 Codex 的 dev profile 做法一致。构建耗时有一份测量报告（`research/2026-10-15-build-timings.md`）。
6. **产品代码的大文件已经拆开。** 修改最频繁的 `radian-tui/src/app.rs`（156 次提交）已经拆成 `app/*.rs`；footer 也按纯移动拆过（1851 行拆到 931 行）。产品代码里最大的文件是 2363 行，Codex、omp、pi 都有超过 7000 行的单个文件。

## 5. Radian 的劣势和缺口

1. **只有一层验证，而且全靠本机。** AGENTS.md 只有"全部 7 条命令都通过"这一层，没有"只验受影响的部分"的快速路径。对照：pi 的 agent 只跑 check，opencode 只测受影响的包，Codex 的 PR 只跑快检查。拿 `cargo metadata` 算扇出：改 `agent-core` 要重链 199 个测试 target，改 `radian-session` 150 个，改 `radian-coding` 129 个，改 `radian-host` 125 个；就算只改叶子 crate `radian-tui`，也有 46 个。build-timings 的结论是"an edit costs roughly one test-binary relink per test file in the fan-out"（§2）。
2. **测试二进制还在变多。** 2026-10-15 测的是 162 个可执行文件；现在 `cargo metadata` 数出 200 个测试 target，`tests/*.rs` 有 184 个。其中 52 个文件只有不超过 3 个测试，7 个 `perf_*.rs` 里的测试**全是 `#[ignore]`**（只做测量，`agent-cli`、`radian-runtime`、`radian-session`、`radian-tui` 各有几个），但每次 Verify 都要编译、链接它们。
3. **没有 CI，Linux 的问题靠人去撞。** 32 个提交标题提到 Linux。例如"the Linux test failures found on a real Linux box (Ubuntu 26.04 LXC)"、"its Linux-only test helpers gated to Linux so macOS clippy passes"，都是合并以后在另一台机器上才发现的。主开发机是 macOS（build-timings §机器），没有任何自动的跨平台检查。
4. **flaky 和负载绑在一起。** pty 的等待上限从 10–20 s 提到 60 s，理由是"the gate runs this suite at load average 70-200 beside several cargo builds"（`pty.rs:209-213`）。并发靠进程内的 `PTY_SLOTS` 互斥锁限制（`pty.rs:250-255`），review 时容易 flaky 的测试要跑 5 遍。41 个提交和 flaky 有关，46 个提交只是修 `cargo fmt` 或 clippy。根因是多个 agent 在同一台机器上并行构建，而且每个 worktree 都有自己的 7 GiB `target/`，依赖要各编一遍（build-timings §机器）。
5. **ADR 体量大，重写规则没有执行完。** AGENTS.md 规定第三次修订就要重写＼�；目前带"修订历史"表的只有 6 篇（0024、0027、0028、0035、0036、0042）。ADR 0018 有 60 KB，还挂着独立的"修订 8""修订 9"两节，没有变更表。0030 已经搁置，却仍有 92 KB，是最大的一篇。全部 ADR 约 50.7 万字符；粗估相当于 25 万 token 左右（推断，CJK 14.5 万字按每字约 1 token，其余按每 3.5 字符 1 token）。一个改压缩逻辑的 agent，光读 0018 就要花约 2–3 万 token。
6. **索引过期，入口也不一致。** `README.md` 的 ADR 索引只写到 0021（`README.md:268`），"Status" 一节还停在 2026-10-13。AGENTS.md 的 ADR 清单漏了 0041（已否决的"去掉终端 mermaid"）。0026 的状态行写着"提议，大部分已裁决"，AGENTS.md 却把它的修订 2 列为现行决定。AGENTS.md 有 11.6 KB，每个会话都会加载，其中 pi 委托规则 5.2 KB，ADR 列表那一段 3.6 KB；大多数会话两样都用不上。
7. **文档语言没有约定。** ADR 43 篇全是中文；crate README 14 篇是英文；AGENTS.md、README、提交信息是英文；设计文档、`sandbox.md`、`perf-baseline.md` 中文占 23–26%（字符比例），属于中英混写；`research/` 中文 10 篇、英文 6 篇；`MUTATIONS.md` 同一个文件里中英文小节交替出现。仓库里找不到任何语言规则（在 AGENTS.md、README、ADR 0036 里 grep "中文/Chinese/English" 都没有结果）。一旦开源，或者引入不读中文的贡献者，"决策"这一层他们读不了。
8. **没有发版，也没有版本概念。** 没有 tag、release、CHANGELOG，crate 一律 `0.1.0`，安装方式只有 `cargo build`（`README.md:70-80`）。对手近 30 天发了 8 到 54 个版本。没有版本，就没有"某个 bug 从哪一版开始"这种问题可问，用户和可信度都无从谈起。
9. **测试代码里的热点。** `radian-tui/tests/pty.rs` 有 12869 行、172 个测试，是全仓库**修改最频繁的文件**（190 次非合并提交）；`agent-cli/tests/cli.rs` 有 8368 行，96 次提交。它们是并行分支合并冲突的主要来源，例如提交"the two tests the merge brought in … use the branch's one pos…"。产品代码里的热点是 `radian-host/src/lib.rs`（120 次）和 `radian-tui/src/main.rs`（163 次）。
10. **小的不一致（推断）。** `radian-runtime` 也定义了 `journal-selfcheck` 这个 feature（`radian-runtime/Cargo.toml:22`），但 Verify 只用它跑 `agent-core`（`AGENTS.md:19`）；AGENTS.md 说 selfcheck"check every transcript write"，但 runtime 层的会话没有在这个模式下跑过。

## 6. 改进方案（按对 Radian 整体价值的收益/成本排序）

省多少时间：凡是"估计"都是从 build-timings 的结构推出来的，**没有实测**。每条做完都应该用 `cargo test --workspace --no-run --timings` 和带计时的 Verify 复测。

### ① 分层验证脚本 `scripts/verify`（收益高，成本低）
- **做什么。** 一个脚本分三层。`fast`：`cargo fmt --check`，加上对改动 crate 跑 `cargo clippy -p <crate> --all-targets`。`affected`：从 `git diff origin/main` 算出改动的 crate，再用 `cargo metadata` 的反向依赖（上面的扇出算法）跑 `cargo test -p <crate 和它的反向依赖> --no-fail-fast`；改了公共 API 时自动包含反向依赖。`full`：就是现在的 7 条命令。脚本把每一步的墙钟时间追加到 `target/verify-times.log`。AGENTS.md 改成"迭代时用 fast/affected，说'done'之前用 full"，**full 的定义不变**。
- **为什么。** pi、opencode、Codex 都把快的那一层交给 agent，把全量交给 gate（§3）。改 `radian-permissions` 或 `radian-memory` 这类 crate 时，只测本 crate，估计从 6–7 分钟降到 1 分钟以内；改叶子 crate `radian-tui` 时，pty 测试本身就是大头，估计只能省一半左右。按每天约 88 次合并、每次合并之前平均有 2–3 轮迭代来算，每天可省机器时间数小时（推断）。
- **风险。** affected 层可能漏掉反向依赖的破坏，或者漏掉只在 feature 组合下才出现的问题；full 仍然是合并前的硬门槛，所以不会漏到 main 上去。
- **ADR。** 不碰。**验证：** 对比同一个改动在 affected 和 full 下的结果；变异检查：故意破坏一个反向依赖，affected 层应该报错。

### ② pre-commit 钩子，加 perf 测试移出默认构建（收益中，成本很低）
- **做什么。** (a) 提供 `.githooks/pre-commit`：`cargo fmt --all -- --check`，失败就拒绝提交；在 worktree 设置步骤里加 `git config core.hooksPath .githooks`。(b) 7 个全是 `#[ignore]` 的 `perf_*.rs` 改为 `[[test]] … required-features = ["perf"]`，测量时用 `--features perf --ignored` 跑。
- **为什么。** 历史上有 46 个提交只是修 fmt 或 clippy，每个都是一轮额外的往返；pi 用 husky 做的就是这件事。(b) 每次构建少链 7 个二进制（200 个里的 3.5%），按 build-timings 每个集成测试 target 平均 2.39 s 估算，冷构建少十几秒的 CPU 时间；`radian-tui` 的扇出也少 2 个（推断）。
- **风险。** (b) 之后，`--all-targets` 的 clippy 就不再检查 perf 代码，需要在 full 层加一条 `cargo clippy --features perf`，或者每周跑一次。
- **ADR。** 不碰。**验证：** 统计 `--no-run` 链接的二进制数；`cargo test --features perf -- --ignored` 能跑通。

### ③ ADR 索引和重写清单（收益高，成本低到中）
- **做什么。** (a) 新建 `docs/adr/INDEX.md`，每个 ADR 一行：编号、状态（已决定、搁置、否决）、**一句中文加一句英文**的现行决定摘要、最新修订号。AGENTS.md 和 README 里那两段长列表都换成指向它的链接。写一个小脚本，检查 INDEX 和目录一一对应（可以放进 `check-fixtures.sh` 这一类）。(b) 按 owner 的重写规则把积压清掉，先重写 0018（60 KB，修订 9）；0030 已搁置，正文挪到 git 历史里，只留"搁置、保留了什么"一页；修正 0026 的状态行。每篇单独一个 docs-only 提交。
- **为什么。** README 的索引停在 0021，AGENTS.md 漏了 0041，说明手写的清单已经跟不上节奏。重写之后，读一篇 ADR 的 token 估计能减半（0018 约省 1 万多 token，推断）；INDEX 也给不读中文的人留了一个入口（见 ⑨）。opencode 的 `specs/v2/README.md` 就是这种"按职责分类、标状态"的索引。
- **风险。** 重写时可能丢掉细节，或者不小心改了决定。规则本身已经要求"no decision changed"，可以让 review 的 agent 拿新旧两版逐条对照决定。
- **ADR。** 执行的是 AGENTS.md 里已有的 owner 规则，不需要新 ADR。**验证：** 索引检查脚本；对重写过的 ADR，抽查 AGENTS.md 列出的"必须知道的修订"在新版里都还在。

### ④ 精简 AGENTS.md（收益中，成本低）
- **做什么。** 把"Rules for delegating to pi"（5.2 KB）挪成一个项目 skill（ADR 0025 的机制），AGENTS.md 只留一行"委托 pi 时加载 skill X"。ADR 那一段长列表（3.6 KB）换成 ③ 的 INDEX 链接，只保留"必须知道的修订"。
- **为什么。** 11.6 KB 大约 2900 token（推断），每个 Radian、pi、oc、Claude 会话都要加载；精简到 4 KB 左右，每个会话省约 2000 token，按每天几百个会话累计相当可观。pi 的 AGENTS.md 只有 125 行，测试和委托细节都放在 skill（`.pi/skills/interactive-testing.md`）里。
- **风险。** skill 是按需加载的，agent 可能该加载的时候没加载。把委托规则的触发条件写进 skill 描述可以缓解。
- **ADR。** 和 ADR 0036（上下文文件）、0025（skills）一致，不需要改。

### ⑤ 只在 main 上跑的 CI：Linux + macOS（收益高，成本中）
- **做什么。** 加 `.github/workflows/verify.yml`，在 push 到 main 时运行（不在每个分支上跑），设 `concurrency: cancel-in-progress`。Linux 和 macOS 各跑一遍 full Verify，用 `Swatinem/rust-cache` 缓存依赖；外加每天一次的定时运行，带 `--features perf` 的 clippy。私有仓库的 Actions 分钟有限，macOS 分钟还要按倍数计价（数额未核实，见 [GitHub Actions billing](https://docs.github.com/en/billing/managing-billing-for-your-products/managing-billing-for-github-actions/about-billing-for-github-actions)），所以也可以在 owner 的 Linux 机器上挂一个 self-hosted runner。
- **为什么。** 32 个和 Linux 有关的提交说明，跨平台问题现在要等到合并后、在另一台机器上才暴露。Codex 的做法正是"PR 快检查，合并后全量"（§3）。一枚绿色的 CI 徽章，也是开源以后可信度的基本配置。
- **风险。** 沙箱和 pty 测试在 CI 容器里可能被外层沙箱拦住（`skip_inside_sandbox!` 已经会输出"not run"标记）。CI 日志要统计这些标记，不能把它们当成通过。
- **ADR。** 不碰（ADR 0013 的"无遥测"只管产品本身，不管 CI）。**验证：** 故意在 main 上推一个只在 Linux 下失败的改动，CI 应该变红。

### ⑥ 跨 worktree 共享依赖缓存（收益中，成本低）
- **做什么。** 在 worktree 的设置步骤里设 `RUSTC_WRAPPER=sccache`，用本地磁盘做缓存。**不要**共用 `CARGO_TARGET_DIR`：多个 agent 并行构建时会抢 Cargo 的构建锁，还会互相导致重编。
- **为什么。** 每个新 worktree 的冷构建要 111 s；依赖在 build-timings 里占 439 s/1021 s 的 attributed 时间（43%），而这一部分正是 sccache 最容易命中的。估计第二个及以后的 worktree，冷构建能降到 60–70 s，同时机器负载也会降下来，从而间接减少 §5.4 里的负载型 flaky（推断）。
- **风险。** sccache 对开了增量编译的 workspace crate 不起作用；缓存目录要定期清理。需要装一个工具，属于 owner 的决定。

### ⑦ 引入 nextest，并把 flaky 规则写进配置（收益中，成本中）
- **做什么。** 加 `.config/nextest.toml`：`retries = 1`，nextest 会把重试后才通过的测试标成 FLAKY；设 `slow-timeout`；给 pty 和 cli 这类测试建 `test-groups`（`max-threads = 3`），代替进程内的 `PTY_SLOTS`；输出 junit。full 层允许用 nextest，review 时把"跑 5 遍"换成对指定 binary 循环 5 次。
- **为什么。** 这是 Codex 已经在用的做法（`nextest.toml`）。flaky 由工具标记、留下记录，不用每次靠 brief 里写"重复几遍"。nextest 一个测试一个进程，`set_var` 互相干扰的问题也随之消失（见 ⑧）。
- **风险。** **`PTY_SLOTS` 是进程内的锁，换成一个测试一个进程之后就失效了**，必须同时配好 test-groups，否则 pty 测试会全部同时启动，flaky 只会更多。`retries` 会掩盖 flaky，所以 review 的规则要写成"FLAKY 数必须为 0，或者每一个都登记在案"。AGENTS.md 里 `--no-fail-fast` 的理由不受影响，nextest 本来就支持这个选项。
- **ADR。** 不碰，改的是 AGENTS.md 的 Verify 一节（由 定）。

### ⑧ 合并集成测试二进制（收益最高，风险最高；要写新 ADR）
- **做什么。** 每个 crate 把 `tests/*.rs` 合进一个 `tests/it/main.rs`（`mod xxx;`），只保留两类单独的二进制：`contract`（AGENTS.md 点名的 `--test contract`），以及 pty、cli 这类重型 e2e。先从 `radian-session`（18 个测试 target）、`agent-core`（16 个）、`radian-coding`（19 个）这几个副作用少的 crate 开始。
- **为什么。** build-timings 把它评为"largest structural lever"：集成测试二进制占 344 s/1021 s。估计 200 个 target 能降到 40 个左右，冷构建省 15–25 s；改一个文件之后的重链时间按比例下降，`radian-tui` 叶子改动从 16.7 s 降到 8 s 左右（推断）。Codex 用 `tests/all.rs` 做的就是这件事。
- **风险（实测到的）。** 14 个测试文件调用了 `env::set_var` 或 `remove_var`，41 个文件里有 `static` 的 `Mutex`、`OnceLock` 或原子量，合进同一个进程以后，环境变量会在测试之间互相干扰（edition 2024 下 `set_var` 本来就是 `unsafe`）。`journal-selfcheck` 的作用范围也会跟着变。build-timings 也写了"This is ADR-worthy"。
- **ADR。** **新 ADR**（测试布局），里面写明哪些测试必须单独成二进制。**验证：** 前后 `--timings` 对比；把合并后的套件在 full 下跑 5 遍；变异抽查确认没有测试因为合并而被静默跳过（数一下合并前后的测试数，必须相等）。

### ⑨ 文档语言约定（收益中，成本低）
- **做什么。** 写一条明确的规则，然后执行。建议：面向用户和贡献者的文档（README、crate README、`crates/radian-sandbox/LIMITS.md`、`provider-contract.md`、AGENTS.md）用英文；ADR 和调研报告可以继续用中文，但 ③ 的 INDEX 里每条带一句英文摘要；同一个文件不混用两种语言，`MUTATIONS.md` 以后统一用一种。
- **为什么。** 现在三类文档各用各的语言，读者无从判断。开源（⑩）之后，"决策"这一层要能被英文读者读到，可信度才算数。
- **风险。** 很小；翻译以后可能出现中英两版不同步，所以 INDEX 里的英文只写一句结论，不翻全文。
- **ADR。** 新 ADR 或者 AGENTS.md 里的一条 owner 规则都可以。

### ⑩ 第一个带标签的发版和 CHANGELOG（对用户和可信度收益高，成本中）
- **做什么。** 根目录建 `CHANGELOG.md`，照 pi 的 `[Unreleased]` 规则（`pi-mono/AGENTS.md:104-111`）；加一个 `[profile.release]`（thin LTO，体积和启动时间要实测）；打 `v0.1.0` tag，提供 macOS arm64 和 Linux x86_64 两个预编译二进制，可以用 ⑤ 的 CI 生成。之后按需发版，不必追对手的频率。
- **为什么。** 现在没有版本、没有安装方式，外部用户无从开始，bug 报告也没有版本可以引用。对手每月发 8 到 54 个版本，"可安装"是最起码的门槛。
- **风险。** 一旦有了版本，就要对用户承诺兼容性：会话 JSONL（ADR 0006）和配置文件（ADR 0008）已经带版本号和迁移规则，这一点 Radian 比较占优。
- **ADR。** ADR 0009 写的是"first release 只做全屏"，正好对应这次发版；发版和兼容策略本身值得一个小 ADR。

### ⑪ 把测试大文件拆成模块（收益中，成本低）
- **做什么。** `pty.rs`（12869 行）和 `cli.rs`（8368 行）按主题拆成 `tests/pty/main.rs` 加若干 `mod`，**仍然是同一个二进制**，用纯移动的方式做，和 footer 那次拆分的先例一样。
- **为什么。** `pty.rs` 是全仓库修改最频繁的文件（190 次），并行分支在这里冲突最多。拆开以后冲突面变小，测试二进制的数量不变。
- **风险。** 共享的 helper（`wait_positive`、`PtySlot`）要放进 `mod common`，不能复制。**验证：** 拆分前后测试名列表完全一致（对比 `cargo test -- --list` 的输出）。

### 不建议做的
- **换成 Bazel 或远程构建（Codex 那样）。** 157 个 crate、多平台、多人团队才需要。Radian 只有 16 个 crate、一个 owner，⑥ 加上 ⑧ 就够了。
- **为了提速删掉变异检查或 journal-selfcheck。** 这些是 Radian 最有辨识度的质量证据（§4.2、§4.3）。真正该削减的是重复链接和全量运行，不是检查本身。
- **在每个分支上都跑 CI。** 以每天约 88 次合并的速度，按分支跑会耗光分钟数，而且和本机的 full Verify 重复。只在 main 上跑加每天定时跑，就能覆盖跨平台这个缺口。
