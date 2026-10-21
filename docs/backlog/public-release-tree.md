# 公开前清理当前树

- 状态：decided
- 涉及 ADR：0013（诊断目录）、0020（`~/.radian/` 布局）；ADR 本身的改写见 `adr-english-ste.md`
- 来源：仓库公开前的检查清单

## 背景

仓库将要公开。当前树里有只属于开发机的内容（内网地址、用户名、本机路径）、一份只对维护者有用的操作说明、误跟踪的编译产物，以及大量“谁在哪天定了什么”的决策记录。公开前先把当前树清干净；历史的改写见 `history-rewrite.md`，它依赖本条和 `adr-english-ste.md`。

## 决定（已定）

1. **AGENTS.md 拆分**。保留 ADR 索引与 Verify 两节，它们是贡献者指南。"delegating to pi" 一节整段移出仓库，放到仓库外的维护者笔记里（例如 `~/.agents/radian-ops.md`），仓库里不留指向它的链接。
2. **脱敏**。内网地址（`192.168.x.x`）、`user@host` 形式的 ssh 目标、`/Users/<name>/`、`/home/<name>/` 一律换成占位符：`<linux-host>`、`<mac-host>`、`<worktree>`、`<home>` 等，同一含义全仓库用同一个占位符。范围是全部被跟踪的文件，包括沙箱逃逸夹具（`crates/radian-sandbox/tests/data/escape-*.json`）、`sandbox-escape.rs`、`spikes/` 下的实验数据、`benchmarks/terminal-bench/`、`docs/spikes/`、`docs/research/` 和 `MUTATIONS.md`。夹具与读夹具的代码一起改，测试照常通过。
3. **删掉误跟踪的文件**。`docs/research/2026-10-16-render-prototype/__pycache__/` 下的 `.pyc` 用 `git rm`；`.gitignore` 已经覆盖它们。
4. **docs/ 只留 `adr/`、`user/`、`backlog/`、`schema/`**。其余移走，所有引用（代码注释、crate README、AGENTS.md、嵌入二进制的手册表）同一提交里改好：

   | 现在 | 移到 |
   |---|---|
   | `docs/providers/*-wire-notes.md` | 各 provider crate 的 `WIRE.md` |
   | `docs/providers/deepseek-reasoning-replay-probe.md` | `crates/agent-provider-openai-compatible/` |
   | `docs/provider-contract.md` | `crates/agent-provider-contract/README.md` |
   | `docs/sandbox.md` | `crates/radian-sandbox/LIMITS.md` |
   | `docs/minimal-headless-rust-agent-runtime-design.md` | 根目录 `ARCHITECTURE.md`，与现行 ADR 不符的地方改掉或删掉 |
   | `docs/perf-baseline.md` | `benchmarks/perf-baseline.md` |
   | `docs/pi-migration.md` | `docs/user/pi-migration.md`，并加进嵌入二进制的手册表 |
   | `docs/research/`、`docs/findings/`、`docs/review/` | 根目录 `research/` |
   | `docs/spikes/<name>.md` | `spikes/<name>/README.md` |
   | `docs/schema/mutation-record*.md` | stays in `docs/schema/`; no move |

5. **research/ 说明**。根目录 README 与 `research/README.md` 说明：这是实验笔记，带日期，以中文为主，记录当时的测量与判断；不是用户文档，也不随代码更新。
6. **不写决策记录**。代码注释、crate README、`MUTATIONS.md`、`docs/backlog/`（含它的 README 与条目模板）、AGENTS.md 里的 "owner"、决定日期、"revision N"/"修订 N" 一类标签全部去掉，只留决定本身。`owner-only` 这类描述文件权限的用法是正常词汇，保留。`research/` 里的这类字样留给历史改写的统一替换处理。以后提交的任何文字都不写这些。
7. **补两个文件**：`CONTRIBUTING.md`（指向 AGENTS.md 的 Verify 一节与 ADR 规则：先读相关 ADR、改决定先改 ADR）；`SECURITY.md`（怎么私下报告沙箱逃逸与其他安全问题，报告里要有什么，不在公开 issue 里贴复现）。
8. **`.gitleaks.toml`**。gitleaks 在当前树与历史里各命中 3 处，都是测试用的假密钥：`crates/agent-cli/tests/cli.rs` 与 `crates/radian-host/tests/pi_migrate.rs` 里的 `sk-live-abcdef1234567890`、`docs/schema/config/plaintext-secret.toml` 的明文密钥夹具（读取方必须拒绝它）。以值放行这组假密钥（`sk-live-abcdef` 加数字后缀的正则），其他命中一律当作问题处理。

## 验收

- `git grep -E '192\.168\.|/Users/[a-z]|/home/[a-z]|[a-z]+@[0-9]'` 在当前树里为空（占位符除外）。
- `git grep -i -w owner` 只剩文件权限一类的用法与 `research/`。
- `git ls-files | grep -E '__pycache__|\.pyc$'` 为空。
- `ls docs` 只有 `adr backlog schema user`。
- `gitleaks dir .` 无命中。
- AGENTS.md 的 Verify 全部通过；Verify 里引用的路径与新位置一致。
