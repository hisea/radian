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
