# 内建一组只读 shell 命令免问

- 状态：decided＼�
- 涉及 ADR：0012（修订档位规则：内建只读 allow 规则，`deny` 仍最先）、0039（读边界的论证）；与 `sandbox-auto-allow-shell.md`、`full-tier-unrestricted.md` 的 0012 修订合成一份
- 来源：`docs/research/2026-10-18-area-deep-dive/7-permissions-sandbox.md` §6 ⑥；

## 背景

今天两档 shell 整体 `ask`，连 `git status` 都要问，除非每次手写 `--allow-shell-command`。Claude Code 在所有模式下都有一组内建只读命令不问（`ls cat grep find wc diff stat`、只读 git 等）。

在 `full-tier-unrestricted.md`（`full` 全放行）和 `sandbox-auto-allow-shell.md`（沙箱内放行）之后，这条规则实际起作用的地方是：`workspace` 档在沙箱内放行落地之前；`approval = always` 的逐条审阅（只读命令不值得审）；以及无头、非人来源回合里 `ask` 按拒绝处理的路径，只读命令不再被拒。规则放在两档的规则表里，`full` 下被整体 allow 覆盖。

## 决定（已定）

- 内建一张只读命令表，作为 allow 规则放在用户规则之后、档位整体 shell 规则之前；`deny` 规则照旧最先。表的初始内容：
  - `git status`、`git diff`、`git log`、`git show`、`git branch`（无参数或 `--list`）、`git rev-parse`、`git remote -v`、`git stash list`
  - `ls`、`pwd`、`wc`、`cat`、`head`、`tail`、`stat`、`file`、`du`、`tree`
  - `rg`、`grep`、`find`
  - `cargo metadata`、`cargo tree`（不执行 build.rs 的只读查询；`cargo build/check/test` 不在表内）
- 命中条件，三条同时成立：
  1. 命令是今天定义的简单命令（或 `shell-compound-allow.md` 落地后每一段都满足本条）。
  2. 所有路径参数解析后都在工作区内：不含 `..` 越界、不以 `/` 或 `~` 开头、不是符号链接出工作区（按 0039 的 fd 级边界同一口径）。`cat ~/.ssh/id_rsa` 仍然要问。
  3. 不带可执行或可写的选项：`find -exec`/`-execdir`/`-delete`/`-ok`/`-fprint*`、`git -c`/`-C`/`--exec-path`/`--git-dir`/`--work-tree`/`--output`、`grep`/`rg` 的 `--pre`/`--pre-glob`、`tail -f`/`--follow`（长驻），以及任何 `-o`/`--output`/`--out-file` 类写文件选项。出现即不算只读，退回 `ask`。表里每个程序配一张自己的禁止选项清单。
- 失败方式是多问一次（over-asking），与 `radian-permissions/README.md` 的"over-asking is the intended failure mode"一致。
- 只读表是宿主内建，不从项目配置扩展（0012 来源规则：项目只能收窄）。用户想加自己的只读命令仍用 `--allow-shell-command`。

## 安全论证

这些命令不写入、只读工作区，而内建的 `read`/`grep`/`glob` 工具本来就能读到同样的内容（0039 的边界），所以模型能看到的东西没有变宽。路径限制保证读不到工作区外。可执行选项按"出现即不算只读"处理，漏掉的后果是多问一次，不是放行。`git log -p`/`git show` 可能触发 `.gitattributes` 指定的 `diff.<driver>.command`/`textconv`：本项的 git 子命令一律自动追加 `--no-ext-diff --no-textconv`，与 ADR 0039 决定 3 的宿主 git 调用口径一致；未经 `host_git.rs` 的裸 git 调用由 `tests/host_git.rs` 的源码扫描挡住，本项的实现要么走那个 helper，要么在 Verify 里给它加例外并说明原因。

## 验收

`workspace` 档（沙箱内放行落地前）与 `approval = always` 下 `git status`、`rg foo crates/`、`ls -la` 不出现提示，journal 里 `PermissionResolved.approver = policy`；`cat ~/.ssh/id_rsa`、`cat ../x`、`find . -exec rm {} \;`、`git -c core.pager=x log`、`tail -f log` 都 `ask`；`deny` 规则命中时仍 deny。变异：去掉路径检查，`cat ../x` 的测试应失败；去掉选项检查，`find -exec` 的测试应失败；去掉 `--no-ext-diff`，`host_git.rs` 的 textconv 标记文件测试应失败。0012 的档位规则同步修订。
