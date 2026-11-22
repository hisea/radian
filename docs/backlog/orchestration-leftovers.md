# 清理编排时期的残留

- 状态：decided
- 涉及 ADR：0048、0006、0008
- 来源：2026-11-21 精简调查（arc、hub 已移除，编排改由 operad 负责）

## 背景

arc 和 hub 的代码已经删掉，但仓库里还有几处只为它们存在、或者根本不进二进制的东西。

## 决定（已定）

1. 删掉 arc 评审的 schema fixture：`docs/schema/fixtures/` 里的 `review-verdict-old.json`、`review-verdict-new.json`，`check-fixtures.sh` 里校验它们的那一段，以及 fixtures README 里的两行（都引用 ADR 0048 决定 10）。Verify 不再跑这一段。
2. 删掉 `spikes/` 整个目录。五个原型都不是 workspace 成员，没有构建、测试或脚本引用它们。
3. 把 `research/` 移出仓库（归档到 owner 指定的地方，不在本仓库保留）。仓库里引用 `research/` 路径的文档（backlog 条目、ADR 的来源行）改成不带路径的说明，或者删掉该引用。
4. 测试注释里对 ADR 0048 的引用删掉（ADR 0048 只作历史保留，代码不再引用它）。
5. `radian-tui` 对 `radian-memory` 的依赖在 `src` 里没有使用，删掉；`radian-exec` 只有测试用它，降为 dev-dependency。（`remove-memory.md` 落地时两者都会消失；先落地本条也不冲突。）

不动：各 crate 的 `MUTATIONS.md` 里带 `TK`/`arc` 的旧行是历史记录，保留；`radian-session` 读旧编排 journal 的兼容测试保留（用户的旧会话仍要能打开）。

## 验收

仓库里 `docs/`、`crates/`、`scripts/` 中除 ADR 0048 本身和 `MUTATIONS.md` 外，没有对 arc、hub 或 ADR 0048 的引用；`spikes/`、`research/` 不存在；Verify 全绿，`check-fixtures.sh` 不再提评审 verdict。
