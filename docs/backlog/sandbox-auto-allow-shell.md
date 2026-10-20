# workspace 档沙箱内自动放行 shell

- 状态：decided＼�
- 涉及 ADR：0012、0016、0043（§3）；0038 的 `.git` 规则不变
- 来源：`docs/research/2026-10-18-area-deep-dive/README.md` 第 8、16 条；`4-architecture.md` §6 ①；

## 背景

workspace 与 full 两档 shell 都是 `ask`（`crates/radian-permissions/src/profile.rs`），开沙箱换不回任何一次提示，dogfood 只能用 `full + --yes`。

影响面比"少点几次确认"大：权限层对**非人类回合**的 `ask` 一律 deny（`crates/radian-permissions/src/hooks.rs`，fail closed）。无头 `radian exec`、subagent 子进程、steer 和后台任务通知触发的回合、herdr 发起的回合，今天碰到 shell 都只能靠 `--yes`。这是"无人值守 worker"定位下最大的摩擦。

## 决定（已定）

三片，按顺序落地，每片自己过 Verify。

**片 0：Linux workspace 档堵 Unix socket（前提）。** 决定 4 曾把"bwrap 不限制 Unix socket"接受为已知限制；本项重开它。workspace 档在 Linux 上用 seccomp 拒绝 `socket(AF_UNIX, …)`（返回 EPERM），抽象 socket 与路径 socket一并堵上。seccomp 看不见路径，所以 `[sandbox] allow_unix_sockets` 在 Linux 上无法按路径放行：Linux 上该键被忽略并在启动行警告一句（只能收窄，fail closed）；macOS 的 seatbelt 路径白名单不变。`docs/sandbox.md` 的已知限制表删掉这一行，逃逸矩阵的 Linux `workspace_denied` 从"已知限制"改为必须通过。

片 0 已在本分支（`dogfood/linux-seccomp`）落地：seccomp 程序见 `crates/radian-sandbox/src/seccomp.rs`，`--seccomp` 只在 `workspace` 档传；逃逸矩阵的 Linux `unix_socket` 组（路径拒绝、抽象拒绝、白名单同样拒绝、DNS 通）全部 PASS，已知限制机制（`KNOWN-LIMIT`）随之删除。片 1、片 2 不在片 0 内，仍待做。

**片 1：沙箱内 shell 由 `ask` 改 `allow`。** 条件三个同时成立：档位是 workspace；这次调用会在沙箱里执行（探针成功的本地后端，或 0016 的 `inherited`）；没有 `deny` 规则命中（`deny` 永远最先）。full 档不变（没有沙箱，shell 仍 `ask`）。命令因沙箱被拒时（写工作区外或 `.git`、读禁读清单、连网被拒、Unix socket 被拒），工具结果里带一个固定标记 `sandbox denied: <类别>`，由沙箱层按 errno 与路径分类；分不清的照常当命令自己失败。模型要越界时显式带 `outside_sandbox: true` 和理由再调一次 shell，这一次走 `ask`；无头会话里 `ask` 仍按拒绝处理。**绝不自动在沙箱外重跑。** subagent 子进程继承父的档位（0043 1a），自然得到同一条规则，不另写代码。MCP 在 workspace 档仍是 `ask`，本项不动。

**片 2：会话授权按前缀。** "本次会话允许"从 (tool, 完整命令串) 放宽到 (tool, 命令前缀)，只对 `crates/radian-permissions/README.md` 定义的"简单命令"生效；复合命令、`eval`、`source`、管道照旧按完整串匹配或 `ask`。前缀授权只存在用户级记录（`~/.radian/access/`），不进项目 `config.toml`：0012 的来源规则是项目只能收窄，放行不能来自项目。禁止清单（永远 `ask` 的前缀，如 `git push`、`rm -rf`）随 ADR 定。

## ADR 改动（docs-only commit，与代码同分支）

0012：workspace 档 shell 的动作改为"沙箱内 allow、越界 ask"，并写明放行发生在沙箱之内、审批仍不能扩大沙箱（P1 不变）；0016：Linux workspace 档 Unix socket 的新规则与 `allow_unix_sockets` 在 Linux 上的处理；0043 §3：沙箱的四类使用场景加一句"沙箱内 shell 不再提示"。

## 验收

片 0：Linux 逃逸矩阵里 workspace 档连路径 socket 与抽象 socket 都被拒；`allow_unix_sockets` 非空时启动行有警告。片 1：workspace 档下 `cargo test` 不出现提示；写 `~/x` 被拒后结果带 `sandbox denied` 标记，带 `outside_sandbox` 再调时走 `ask`；探针失败时不放行（变异）；`inherited` 下放行；full 档仍 `ask`；无头 `radian exec --profile workspace` 不带 `--yes` 能跑沙箱内 shell，越界请求被拒且结论写明；`general` 子会话在父 workspace 非 auto 下能跑 `cargo check`。片 2：允许过 `cargo test -p a` 后 `cargo test -p b` 不再问；`cargo test && rm -rf x` 仍问；项目配置里写放行被忽略并警告（变异：放行能来自项目）。A/B：同一组任务在 workspace 档下统计提示次数和墙钟时间。
