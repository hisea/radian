# `workspace` 档：`write`/`patch` 绕过 0038 的 `.git` 只读清单（bug，待跑一次验证）

- 状态：idea（bug 待验证；验证成立即为 0038 的一致性修复，不需要新决定）
- 涉及 ADR：0038（决定 3：`.git/config`、`.git/config.worktree`、`.git/hooks/` 及子模块同类文件在 `workspace` 档只读）、0039（文件工具是宿主侧 fd 级实现）、0012
- 来源：2026-10-20 对照 Claude Code protected paths 时从代码读出（`profile.rs` 的 `write_under`、`radian-coding` 的 `write.rs`/`patch.rs`/`safe_fs.rs` 没有任何 `.git` 排除）

## 背景

0038 把 `.git` 里能执行代码的部分留成只读，强制点在沙箱（seatbelt/bwrap 的策略）。但 `write`/`patch` 是宿主工具，不经沙箱；`workspace` 与 `full` 两档的规则都是 `(write|patch, PathUnder(<workspace>), Allow)`，`.git` 在工作区内。于是在 `workspace` 档，模型可以用 `write` 写 `.git/hooks/pre-commit`，然后沙箱里的 `git commit`（0038 有意让它跑用户装的 hook）或用户自己下一次 commit 在宿主上执行它。沙箱对 shell 的保护就被文件工具绕过了。

Claude Code 的做法：沙箱层拒 `.git/hooks`、`.git/config` 的写，权限层另有 protected paths 表，文件工具写 `.git` 也要问。Codex 在 `workspace-write` 下整个 `.git` 只读。

`full` 档按 `full-tier-unrestricted.md` 全部放行，本项只管 `workspace`。

## 待决问题

- 先跑一次确认：`workspace` 档下让模型 `write .git/hooks/pre-commit`，看是否成功。
- 修法二选一：(a) 在 `workspace_write()` 加 `(write|patch, PathUnder(<ws>/.git/hooks 等), Deny)`，规则模型已有 `PathUnder` 与 `deny > allow`，几乎零成本，但 `PathUnder` 对单个文件（`.git/config`）的匹配要核一下；(b) 在 `safe_fs` 里按 0038 清单拒绝，和沙箱同一张表、同一个错误文案。倾向 (a) 加一条共享清单常量供沙箱策略与规则同用，清单只维护一处。
- linked worktree：`.git` 是指向外部 gitdir 的文件，文件工具按 0039 从根 fd 出发本来写不到工作区外，核一下这条路径确实被拒。

## 验收

`workspace` 档下 `write .git/hooks/pre-commit`、`patch .git/config` 被拒且错误文案说明原因；`write .git/info/exclude` 仍成功（0038 允许的可写部分）；子模块 `.git/modules/x/hooks/` 同样被拒；逃逸矩阵或 `radian-permissions` 测试里各一条。变异：去掉清单，`write .git/hooks/pre-commit` 的测试应失败。
