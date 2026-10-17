# 变异记录：sandbox 后端（Phase 3）

日期：2026-10-12。变异在一次性检出 `/tmp/sandbox-mut`（`git worktree add`，HEAD = 本提交）里做，
每条改完恢复源码、提交前 `git status --short` 为空。测试一律 `--no-fail-fast`。

macOS 变异跑 `cargo test -p radian-sandbox --test escape_macos`（或 `--test backend`）；
Linux 变异把 `sandbox-escape` 交叉编译成 aarch64-musl、scp 到 VM（`ssh <user>@<host>`，
先确认 `uname -s` = Linux）后运行，看退出码与失败用例数。

## macOS

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| MM1 | 去掉工作区写边界（`(allow file-write* (subpath ws))` → `(allow file-write*)`） | escape 用例由"被拒"变"成功" → escape_macos FAILED | `escape_macos`（escape_write 四例） |
| MM2 | 去掉 git 元数据的 `deny file-write*` carve-out | git 写用例变成功 → FAILED | `escape_macos`（git_readonly） |
| MM3 | 去掉敏感读 `deny file-read*` | 假 key 可读 → FAILED | `escape_macos`（sensitive_read） |
| MM4 | 无条件 `(allow network*)` | 本机端口可连 → FAILED | `escape_macos`（network_off） |
| MM5 | `apply_env` 不再 `env_clear` | 白名单外变量泄漏 → FAILED | `escape_macos`（environment） |
| MM6 | `from_backend` 跳过 `probe()`（伪造 Ok） | 失败探测不再报错 → `--test backend` FAILED | `a_failed_probe_is_an_error_not_a_downgrade` |
| MM7 | 探测失败时降级为无沙箱（返回 `NO_SANDBOX`） | 不可用后端返回了可用 Sandbox → `--test backend` FAILED | `an_unavailable_backend_is_an_error_and_never_a_downgrade`、`full_access_is_transparent_and_unprobed` |

## Linux

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| LM1 | 宿主根改为可写（`--ro-bind / /` → `--bind / /`） | **探测先抓住**：工作区外写成功 → probe 判泄漏，driver EXIT=2（拒绝运行） | probe（不是矩阵） |
| LM2 | 去掉 git 元数据的 `--ro-bind` carve-out | 6 个 git 写用例变成功 → EXIT=1 | `escape-linux`（git_readonly） |
| LM3 | 网络关闭改为部分隔离（不 `--unshare-all`） | 1 例（本机端口连接）变成功 → EXIT=1 | `escape-linux`（network_off） |
| LM4 | 去掉 `--clearenv` | 1 例（白名单外变量）泄漏 → EXIT=1 | `escape-linux`（environment） |
| LM5 | 不 mask 敏感路径 | 1 例（假 key 可读）变成功 → EXIT=1 | `escape-linux`（sensitive_read） |

结论：macOS 7 个、Linux 5 个变异全部被抓住。LM1 不是被某条矩阵用例抓住，而是被**后端自检**抓住——
这正是自检存在的理由（规则静默失效时，构建阶段就拒绝）。没有等价变异或测试漏洞。

## ADR 0016：workspace 开网与 pty（2026-10-13）

一次性检出 `/tmp/tiers-mut`（base `9c060d2`），每个变异用 `git checkout --` 恢复，
`git status --short` 为空。macOS 跑 `cargo test -p radian-sandbox --test escape_macos`；Linux
变异交叉编译 `sandbox-escape` 到 VM 后运行（先确认 `uname -s` = Linux）。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| MM8 | `workspace` 关网（`network: false`） | ✅ escape_macos FAILED（`network_workspace` 的 3 例由 ok 变 denied） | `escape_macos`（network_workspace） |
| MM9 | `file-ioctl` 不带过滤（`(allow file-ioctl (literal …) (regex …))` → `(allow file-ioctl)`） | ✅ `--lib` FAILED | `seatbelt::tests::the_pty_ioctl_allow_is_filtered_to_the_pty_devices`；**逃逸矩阵抓不住**（见下） |
| LM6 | Linux 去掉 `--new-session`（bwrap 的 setsid） | ❌ **未抓住**：`tiocsti_outside_terminal` 仍 PASS | 无 |

**MM9 的说明（matrix cannot distinguish）**：`(allow file-ioctl)` 不带过滤时，`openpty` 与
`tiocsti_outside_terminal` 都照常 PASS（实测，macOS）。矩阵区分不了过滤与不过滤；用 seatbelt
单测把规则的**形状**钉住（断言 profile 含带 `(literal …) (regex …)` 的 file-ioctl、不含裸的
`(allow file-ioctl)`）。ADV 0016 要求过滤，单测是这条要求的执行点。

**LM6 的说明**：去掉 bwrap 的 `--new-session` 后，Linux 的 `tiocsti_outside_terminal` 仍然只看到
sandbox `denied`、control `ok`。原因：bwrap 的 `--dev /dev` 在沙箱内挂载自己的 devpts，测试传进去
的**沙箱外** pts 路径在沙箱里指向另一个终端（或不存在），因此这条用例在 Linux 上并没有真的把
“继承的控制终端”交给被沙箱化的命令；`--new-session` 不是这条用例能观测到的防线。macOS 同理：
seatbelt 即使 `(allow file-ioctl)`（过滤或不过滤）也拒 `TIOCSTI`（实测，`/dev/tty` 可打开、ioctl
返回 EPERM），所以 `tiocsti_outside_terminal` 通过但**不敏感于 setsid**。setsid 的变异由
`radian-coding` 的进程组用例抓住（见该 crate 的 MUTATIONS）。

## ADR 0020 `radian_home` 矩阵用例（2026-10-13）

一次性 worktree `/tmp/home-mut`（从 `62cdcf3` 创建，用完删除），`--no-fail-fast`；变异后从副本还原。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M2 | harness 不把 `~/.radian/memory` 加进 `deny_read` | ✅ 抓住（39 cases, 1 failed） | `escape_macos` 的 `radian_home/memory_unreadable` |
| M3 | harness 把 `~/.radian/config` 误加进 `deny_read` | ✅ 抓住（39 cases, 1 failed） | `escape_macos` 的 `radian_home/config_readable` |
| M4 | 宿主在遮住前不先创建目录（去掉 `ensure_private_dir`） | ❌ **未抓住**，见下 | — |

**M4 未被抓住，也不是可确定复现的缺陷。** `bwrap.rs` 在 **每次 `Sandbox::command()` 时**才遍历
`policy.deny_read` 并跳过不存在的路径；而 shell 命令的 bwrap 参数是在 shell **执行时**才构造的，
那时 `open_journal` 已经创建了会话目录（`memory/` 由 `radian_host::memory_dir()` 在宿主启动时创建）。
所以即使去掉 `ensure_private_dir`，被读的路径在 bwrap 构造参数时已经存在，仍会被 `--tmpfs` 遮住。
要确定复现"目录不存在时未先创建"需要一个在 bwrap 构造参数之后、子进程读之前创建该文件的并发写者，
测试无法提供（沙箱内也不能在工作区外写）。**结论**：宿主仍然创建两个目录（`sessions/` 与
`memory/`，0700），这是对"运行时自己不会创建 `memory/`"的必要保证；`memory_unreadable` 用例
（M2）证明遮住确实生效。**未验证**：如果将来 `memory/` 由别处延迟创建，且在某个 shell 命令的
bwrap 参数构造之前它仍不存在，那个窗口不会被遮住。

## sbxnest：工作区在禁读目录内

Base: `cd89c9d` · 一次性检出 `/tmp/sbxnest-mut2`（detached，跑完删除）· `--no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| N1 | `from_policy` 不做 `workspace inside deny_read` 检查 | `backend::a_workspace_inside_a_denied_directory_is_refused` | ✅ failed |

1/1 抓住。

## unix-socket：出站 Unix socket 白名单（ADR 0016）

Base: `2d7cb90` · 一次性检出 `/tmp/radian-us-mut`（detached，跑完删除）· 每条改 `seatbelt.rs` 的
`profile_text` 后跑 `escape_macos`。

| # | 变异 | 抓住它的用例 | 结果 |
|---|---|---|---|
| M1 | 去掉 `(deny network-outbound (remote unix-socket))` | `unix_socket workspace_denied` 由 denied 变 **ok** | ✅ 1 例 FAILED |
| M2 | 去掉 mDNSResponder 放行 | `unix_socket dns_resolves` 由 ok 变 **denied** | ✅ 1 例 FAILED |
| M3 | 不放行白名单 | `unix_socket whitelisted_allowed` 由 ok 变 **denied** | ✅ 1 例 FAILED |
| M4 | 放行写在 deny **之前**（SBPL 后写优先，deny 反而赢） | `whitelisted_allowed` 与 `dns_resolves` 都变 denied | ✅ 2 例 FAILED |

4/4 抓住。

## startup-speed：git 元数据发现跳过缓存目录与 node_modules（2026-10-15）

工作区 `<home>/Code/radian-dogfood-ss`，分支 `dogfood/startup-speed`；每个变异改完从副本
还原并用 `cmp` 确认。跑 `cargo test -p radian-sandbox --no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SP-1 | 去掉 `CACHEDIR.TAG` 跳过（`is_skipped_dir` 不查标记） | `policy::tests::the_walk_never_enters_a_cache_tagged_directory`、`backend::a_repository_inside_a_skipped_directory_is_not_collected` | ✅ |
| SP-2 | 去掉 `node_modules` 跳过（`SKIP_DIR_NAMES` 置空） | `policy::tests::the_walk_never_enters_node_modules_but_collects_its_own_root_git`、`backend::a_repository_inside_a_skipped_directory_is_not_collected` | ✅ |
| SP-3 | 把跳过检查放到遍历入口（连用户点名的工作区根也跳过） | `policy::tests::the_workspace_root_is_walked_even_when_it_is_cache_tagged` | ✅ |
| SP-4 | 恢复跟随符号链接（`file_type()` 换回 `metadata()`） | `backend::a_symlinked_directory_is_not_traversed` | ✅ |
| SP-5 | 跳过目录根自己的 `.git` 不再收集（即只 `continue`，这就是被评审指出的绕过） | `backend::a_skipped_directory_that_is_itself_a_repository_still_has_its_dot_git_collected` | ✅ |

5/5 抓住。SP-5 是一个真实的安全回归：不可信仓库在自己的根放 `CACHEDIR.TAG`（或叫
`node_modules`）即可让 `.git` 保持可写，模型能写 `.git/hooks`（用户下次在那里跑 git 会执行）。
修法是 `is_skipped_dir` 命中的目录只收集它**自己根下**的 `.git`（复用 `add_git_entry`，不往下钻），
子树仍然不搜（`a_repository_inside_a_skipped_directory_is_not_collected` 与
`the_walk_never_enters_a_cache_tagged_directory` 继续钉住）。

## ADR 0038 切片①：`git_access` 与 `.git` 只留执行入口只读（macOS，2026-10-15）

工作区 `<home>/Code/radian/.radian-worktrees/git-writable`，分支 `oc/git-writable`。每个变异
只改 `crates/radian-sandbox/src/seatbelt.rs`，跑完从副本还原并逐字节比较确认一致（脚本与日志在
`.radian/progress/logs/mutations-round2.txt`）。跑 `cargo test -p radian-sandbox --no-fail-fast
--test git_writable`（macOS-only 行为测试，每条命令带 `FullAccess` 对照）；GW-5 额外跑
`--test escape_macos`。GW-6..8 是评审回合（发现下述真实漏洞后）补的。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| GW-1 | 去掉 `hooks/` 的 deny（`(subpath <git>/hooks)` 那一段） | `git_writable::writable_except_exec_allows_git_and_denies_the_exec_entries` 的 `repo_hook_new_file`/`repo_hook_overwrite`/`submodule_hook`/`worktree_shared_hook` | ✅ 1 test FAILED |
| GW-2 | 去掉 `config` 的 deny | 同上，`repo_config`/`submodule_config`/`worktree_shared_config` | ✅ |
| GW-3 | 去掉 `config.worktree` 的 deny | 同上，`repo_config_worktree_new`（per-worktree 那条仍被 `worktrees/*/config.worktree` 的 regex 护住） | ✅ |
| GW-4 | 忽略 `git_access`：`WritableExceptExec` 也走整块 `ReadOnly` deny（宿主忘了接线的形态） | 同上，`git_add`/`git_commit`/`git_checkout_branch`/`git_stash`/`git_branch_delete` 这些 must_succeed 用例 | ✅ |
| GW-5 | `ReadOnly` 臂改成不产生任何 deny（整块 `.git` 变可写） | `git_writable::readonly_keeps_the_whole_git_read_only`；另跑 `escape_macos` 的 `git_readonly` 组 | ✅ 两个测试二进制各 FAILED |
| GW-6 | 非目录条目回退成"不加任何规则"（即评审发现的漏洞：worktree 的 `.git` 指针文件变可写） | `git_writable::writable_except_exec_protects_worktree_links_and_nested_submodules` 的 `worktree_pointer_file` | ✅ |
| GW-7 | 去掉 `worktrees/<name>/{gitdir,commondir,config.worktree}` 的 deny | 同上，`worktree_gitdir`/`worktree_commondir`（per-worktree 的 `config.worktree` 仍被 `git_exec_denies` 对该 gitdir 的 literal 护住） | ✅ |
| GW-8 | 子模块 regex 退回一层（`(/modules/[^/]+)+` → `/modules/[^/]+`） | 同上，`nested_submodule_config`/`nested_submodule_hook`（那个两级 gitdir 没有工作树、不被发现，只有这条多级 regex 护得住） | ✅ |

8/8 抓住。GW-4 是"策略参数被忽略"的形态：`.git` 退回整块只读，原生 `git commit` 失败，must_succeed
用例抓住。GW-5 同时被新行为测试与共享逃逸矩阵的 `git_readonly` 组抓住——后者证明任务 agent 的整块
只读规则没有被放松。

**GW-6 是一个真实的安全漏洞（评审回合发现，已修）**：最初 `git_exec_denies` 对**非目录**（worktree 的
`.git` 指针文件）直接 `return`，于是那类是**整块可写**的——模型可以把它改指向自己握有 hook 的 gitdir，
用户在沙箱外下次跑 git 就执行。修法：非目录条目保留**整块** `deny file-write*`，和 `ReadOnly` 一样。
GW-6 证明新测试抓住这个回归。**GW-8 的测试要小心**：若嵌套子模块的工作树还在工作区里，发现会把它
的 gitdir 加进只读清单，`literal .../config` 就替 regex 挡住写——测试会**空过**。所以测试里的两级子模块
故意**没有工作树**（`nest/.git/modules/a/modules/b` 在，`nest/a`、`nest/a/b` 已删），只有多级 regex
护得住；否则 GW-8 抓不住（本次第一版测试就是这样空过的）。

## ADR 0016：`confined_by_outer_sandbox` 与 `inherited` 后端（2026-10-16）

worktree `.radian-worktrees/inherited-sandbox`，分支 `dogfood/inherited-sandbox`。每个变异只改一个
文件，跑完把保存的原文写回并 `cmp` 确认逐字节一致（副本在 `.radian/mut/`）。只跑应当抓住它的测试
二进制。本 crate 的变异如下；`radian-tui` 的 `build_sandbox`/PTY、`radian-host` 的 `describe`、
`agent-cli` 的 `build_sandbox` 各有自己的表（同一轮）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| IS-1 | `confined_by_outer_sandbox` 恒返回 `false`（`result > 0` → `false`） | `escape_macos`（acceptance 1：`sandbox-exec -p '(version 1)(allow default)'` 下的 `sandbox-confined` 必须打印 `true`）；另跑 `radian-tui` 的 `pty_an_outer_sandbox_is_inherited_instead_of_refused`（内层探测失败即拒绝，启动不了） | ✅ 两个二进制各 FAILED |
| IS-2 | 恒返回 `true` | 不是某个测试的断言：沙箱**外**也打出了 `radian-test: skipped inside a sandbox:` 标记（acceptance 5 的第二条「沙箱外一行都没有」）；`grep -c 'radian-test:'` 检查失败 | ✅（见下） |
| IS-3 | `inherited` 后端的 `command()` 去掉 `apply_env`（子进程继承整个宿主环境） | `backend::the_inherited_backend_keeps_the_environment_whitelist_and_no_local_rules`（`/usr/bin/env` 的输出多出 `CARGO_*`、`RUSTUP_*` 等） | ✅ |
| IS-4 | `confined_by_outer_sandbox` 的 `result > 0` → `result >= 0`（**0 也算被约束**；评审回合发现的真实漏洞） | `escape_macos::macos_confined_probe_is_false_outside_and_true_under_sandbox_exec`：沙箱外**直接**跑 `sandbox-confined` 助手，断言其 stdout 为 `false`，变异下打印 `true` → FAILED。旧矩阵测试**抓不住**，见下 | ✅ 1 test FAILED |

4/4 抓住（IS-1..IS-3 + 评审回合的 IS-4）。**IS-2 的说明**：函数恒真时，`escape_macos` 与四个清单里的 PTY 测试都会走「跳过」分支而
**空过**，所以它由 acceptance 5 的标记计数检查抓住——沙箱外运行同一批测试并 `grep -c 'radian-test:'`
应为 0，变异后清单里的测试在沙箱外也各打一行（本次抽查了其中两个二进制，各一行）。这正是 acceptance 5
要求「沙箱外一行都没有」的原因。

**IS-4（评审回合 2026-10-17，真实安全边界漏洞，已修）**：把 `sandbox_check_self` 的 `result > 0` 写成
`result >= 0` 后，`sandbox_check(getpid(), NULL, 0)` 在沙箱外返回的 0 也被判成「已约束」。原来的
acceptance 1 断言写在 `macos_escape_matrix` 里，而它**先**在测试进程调 `confined_by_outer_sandbox()`，
读到（变异造成的）`true` 就打印 `radian-test: skipped inside a sandbox: macos_escape_matrix` 后返回——
cargo 记它通过，于是矩阵里那段 acceptance 1（包括「`sandbox-exec` 下的 `sandbox-confined` 必须打印
`true`」）在变异下**一次也没执行**。旧测试因此对「0 被当成已约束」完全不敏感：沙箱外误判会选中不设限的
`inherited` 后端，是一条安全边界漏洞。

修法：新增 `macos_confined_probe_is_false_outside_and_true_under_sandbox_exec`，**不**在父进程调
`confined_by_outer_sandbox()`，答案只从 `sandbox-confined` 助手自己的 stdout 读：同一助手在
`/usr/bin/sandbox-exec -p '(version 1)(allow default)'` 下必须打印 `true`，**直接**跑必须打印 `false`。
「已在沙箱内」改用**结构性**信号判定，不走被测函数：内核拒绝在 `(deny default)` 沙箱里再套一层
`sandbox-exec`（`sandbox-exec: sandbox_apply: Operation not permitted`，本机实测，退出码 71；与 ADR
0016 修订 2 背景表一致），命中时打印同样的固定标记后返回。`macos_escape_matrix` 的跳过行为保持不变
（它另有目的：矩阵本来就要在沙箱外持有资源）。变异与还原：只改 `crates/radian-sandbox/src/lib.rs`，
跑完把保存的原文（`.radian/mut/`）写回并 `cmp` 逐字节一致（SHA256 恢复前后相同）。

## ADR 0043 切片 ⓪B：linked worktree 的 git 目录可写（macOS，2026-10-17）

worktree `<home>/Code/radian/.radian-worktrees/s0b-gitdir`，分支 `dogfood/s0b-gitdir`。每个变异
只改 `crates/radian-sandbox/src/seatbelt.rs`，跑完从副本（`.radian/mut/seatbelt.rs.orig`）还原并
`cmp` 逐字节一致（SHA256 前后相同）。跑
`cargo test -p radian-sandbox --locked --no-fail-fast --test git_writable`（macOS-only；每条命令带
`FullAccess` 对照）。测试 1/2 = 新加的
`linked_worktree_outside_the_workspace_{allows_native_git,denies_exec_and_non_git_paths}`（workspace
**就是** linked worktree，host 仓库与 common dir 在它之外）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| LWT-1 | 去掉新加的 allow（`WritableExceptExec` 下不发 `(allow file-write* (subpath <git dir>))`） | 测试 1，`worktree_merge_ff_only`（原生 git 在 worktree 里写 common dir 的 refs / per-worktree 的 index 被拒） | ✅ 1 test FAILED |
| LWT-2 | allow 发成 common dir 的**父目录**（放宽到 host 仓库根） | 测试 2，`host_working_tree`（host 工作树里的文件本在工作区外、非 git 元数据，必须拒） | ✅ 1 test FAILED |
| LWT-3 | 把 allow 挪到 deny **之后** | 测试 2，`worktree_gitdir`（嵌套的 per-worktree allow 反压 common dir 的 `worktrees/<name>/gitdir` deny）；另有两个既有测试也失败 | ✅ 3 tests FAILED |
| LWT-4 | 只为**工作区内**的 git 目录发 `hooks/` deny（common dir 在工作区外就跳过） | 测试 2，`common_hook` | ✅ 1 test FAILED |

4/4 抓住。LWT-3 是顺序形态的漏洞：`read_only` 按路径排序，common dir 恒在 per-worktree gitdir
**之前**，所以"每个条目 allow 紧跟自己的 deny"这种单遍写法会让后者的 allow 反压前者的
`worktrees/[^/]+/gitdir` deny——实现必须**先发全部 allow、再发全部 deny**。LWT-4 与 GW-1 同类
（`hooks/` deny 缺失），但只有当 common dir 在工作区之外时才漏；新测试的 `common_hook` 正是这个
形态（既有测试的 common dir 在工作区内，抓不住它）。


## ADR 0034：`HERDR_*` 在缺省白名单里（2026-10-17，ADR 0043 slice ①）

变异：从 `policy.rs` 的 `DEFAULT_ALLOW_ENV` 删掉六个 `HERDR_*` 名字；先
`cargo build -p radian-tui --bin radian`，`cp` 备份 / 还原并 `cmp` 一致。命令：
`timeout 300 cargo test -p agent-cli --locked --no-fail-fast --test cli the_shell_environment_carries_herdr_and_allow_env`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| HE-1 | 白名单不含 `HERDR_ENV/HERDR_PANE_ID/HERDR_TAB_ID/HERDR_WORKSPACE_ID/HERDR_SOCKET_PATH/HERDR_BIN_PATH` | `the_shell_environment_carries_herdr_and_allow_env`（`left: "\|foo-value\|"`，`pane-9` 为空） | ✅ 1 failed |
