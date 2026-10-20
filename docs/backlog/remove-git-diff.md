# 删掉 `git_diff` 工具

- 状态：decided＼�删）；shell 里的 git 是否加固待决，见末节
- 排期：`shell-readonly-allow` 合入之后（在那之前，`workspace` 档看改动不弹审批只能靠 `git_diff`）
- 涉及 ADR：0043（§4 工具数与别名清单、决定 3 的别名机制）、0039（宿主 git 调用的加固）、0028（TUI 的 `git_diff` 渲染器）、0012（只读内建工具在各档放行）
- 来源：lead 2026-10-20 工具描述审查时 owner 追问"为啥有 `git_diff`"

## 背景

- `git_diff` 当初的理由：只读、各档免审批；走宿主的加固 git 调用（关掉 `core.fsmonitor`、hooks、attributes，屏蔽 `filter.*` 命令），仓库配置里埋的命令借它执行不了；TUI 把结果画成着色 diff。
- 实际用量（本机 163 个会话、约 1.5 万次工具调用，2026-10-20）：`git_diff` 27 次、`git_status` 41 次；同期 shell 里 `git status` 548、`git log` 478、`git diff` 321、`git show` 201。模型看 git 几乎都走 shell，加固那一层实际上被绕过了。
- `shell-readonly-allow` 落地后，`workspace` 档下 `git status`/`git diff` 走 shell 也免审批，"免审批"这条理由不再成立。定义约 600 B，占一个工具位。

## 决定（已定）

- 按 ADR 0043 决定 3 退为**一个版本的别名**：`git_diff`（以及已经是别名的 `git_status`）留在 `tool_specs()` 让 Core 的 preflight 接受旧调用，从每个请求里剥掉，调用得到改名回复「`git_diff` 已移除：用 `shell` 跑 `git diff`/`git status`」，不执行旧功能。下一个版本连别名一起删。
- `<runtime>` 层的工具清单、权限的只读内建工具清单、`explore` 等固定工具集里去掉它；ADR 0043 §4 的工具数减一。
- TUI 的 `git_diff` 渲染器留到别名删除时再删（旧会话的回放还要用）。

## 待决问题：shell 里的 git 要不要加固

这个风险今天就存在，与删不删 `git_diff` 无关：`full` 档下 shell 没有沙箱，在不受信任的仓库里跑 `git diff`/`git status` 会执行仓库配置里的 `core.fsmonitor`、`filter.*.clean`、`diff.*.textconv`、外部 diff 命令。候选：

- (a) 不加固：`full` 档本来就是"像 pi 一样无限制"，信任门（ADR 0031）管不受信任的项目。
- (b) shell 的子进程环境里注入 `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n`/`GIT_CONFIG_VALUE_n`，把宿主加固用的那几项（`core.fsmonitor=false`、`core.hooksPath=/dev/null`…）对所有 shell 里的 git 生效；代价是用户自己的 hooks 在 radian 的 shell 里也不跑（`git commit` 的 pre-commit 失效）。
- (c) 只在项目未受信任时注入 (b) 的配置。

落地前由 定；选 (b)/(c) 时另写验收。

## 验收

- 请求里不再出现 `git_diff`；调用 `git_diff` 得到改名回复；`strip_renamed_tool_aliases` 的测试覆盖它。
- ADR 0043 钉住的工具集合各减一；权限层只读清单不含它，旧名按别名判。
- 变异：`git_diff` 未从请求剥掉 → 测试失败。
- 落地时同分支改 ADR 0043、0039、0012（docs-only commit）并删掉本文件。
