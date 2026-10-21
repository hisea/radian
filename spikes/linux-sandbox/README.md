# Phase 0b 可行性验证：Linux 沙箱（Landlock / bubblewrap / seccomp）

- **结论：通过（机制选 bubblewrap）**。10 条标准在 `bwrap` 下全部满足，且每条"失败"用例都有"关掉沙箱后成功"的对照。**Landlock 单独是"部分"**：它满足 1–9，但**无法**把可写工作区里的 `.git` 变成只读（第 10 条不满足）——这是机制本身的限制（Landlock 规则是"允许的并集"，没有 deny 规则）。
- 环境：**Arch Linux ARM（omarchy），aarch64，内核 `7.2.6-1-aarch64-ARCH`**，活跃 LSM `capability,yama,landlock`，`bubblewrap 0.12.0`、`gcc 16.1.1`、`Python 3.14.7`、`git 2.55.0`。这是一台**虚拟机**（`ssh <user>@<linux-host>`），不是真实主机。
- 时间：2026-10-12。**VM 里没有安装任何东西**（cargo/rustc 未装；用既有的 `gcc` + shell + `python3`）。原型在 `spikes/linux-sandbox/`，证据在 `spikes/linux-sandbox/data/`。

## 与 macOS 结论的结构对齐

本文件与 `spikes/macos-sandbox/README.md` 逐节对齐，便于两个平台对比决定权限档位。差异表见 §7。

## 1. 结果总表（10 条标准）

证据文件：`data/matrix-bwrap-sandbox.txt`、`data/matrix-landlock-sandbox.txt`、`data/matrix-combo-sandbox.txt`，对照 `*-control.txt`。

| # | 尝试 | bwrap | Landlock（ABI 10） | bwrap+Landlock | 证据（bwrap） |
|---|---|---|---|---|---|
| 1 | 写工作区外绝对路径 | ✅ 失败 | ✅ 失败 | ✅ 失败 | `Read-only file system` |
| 2 | `..` 绕出写 | ✅ 失败 | ✅ 失败 | ✅ 失败 | `../outside/dotdot.txt: Read-only file system` |
| 3 | 工作区内符号链接 → 外，经它写 | ✅ 失败 | ✅ 失败 | ✅ 失败 | `leak.txt: Read-only file system` |
| 4 | 符号链接的**父目录**逃逸 | ✅ 失败 | ✅ 失败 | ✅ 失败 | `link_dir/new.txt: Read-only file system` |
| 5 | 网络关闭时连本机 127.0.0.1 监听端口 | ✅ 失败 | ✅ 失败 | ✅ 失败 | `socket.create_connection` 抛错（新 netns 无路由） |
| 6 | 读取未授权环境变量 | ✅ 隐藏（spawner） | ❌ **做不到**（spawner） | ✅ 隐藏（spawner） | `6a env-secret-hidden` → `<unset>` |
| 7 | 子进程继承限制（沙箱内两层 `sh -c`） | ✅ 失败 | ✅ 失败 | ✅ 失败 | `nested.txt: Read-only file system` |
| 8 | MCP stdio 可用（initialize + 工具调用） | ✅ 成功 | ✅ 成功 | ✅ 成功 | `MCP_OK echo=hello-mcp inside_written=True outside_write_succeeded=False` |
| 9 | 沙箱内读写工作区文件 | ✅ 成功 | ✅ 成功 | ✅ 成功 | `in.txt` 写入并读回 |
| 10 | 共享 `.git` 只读（全部子项） | ✅ 全部失败 | ❌ **全部成功**（限制） | ✅ 全部失败 | 见 §5 |

`SUMMARY` 行（每条 21 个断言）：

```
bwrap-sandbox      ok=21 mismatch=0     bwrap-control      ok=21 mismatch=0
landlock-sandbox   ok=21 mismatch=0*    landlock-control   ok=21 mismatch=0
combo-sandbox      ok=21 mismatch=0     combo-control      ok=21 mismatch=0
```

\* Landlock 那一行的 `ok=21` 是"与**声明的期望**一致"：它的期望表把第 10 条的子项标成"预期成功"（因为机制做不到），所以 21/21 **不代表它满足第 10 条**。`data/matrix-landlock-sandbox.txt` 里 10a–10h 全部是 `actual=success`。

### 1.1 每条"失败"都有"关掉沙箱后成功"的对照

`*-control.txt` 用同一套脚本、同一套 fixture，只把 `SANDBOX=off`（不套 bwrap/Landlock），结果 **21/21 全部按"越界应当成功"通过**：1–5、7、10a–10h 的写/连接全部成功。这排除了"失败只是因为文件不存在或命令写错"的可能。变异 M4（§6）专门演示了这个陷阱：一旦把越界目标从沙箱里藏起来，第 1 条会以 `No such file or directory`"失败"——**那不是沙箱的功劳**。

## 2. 复现

```sh
ssh <user>@<linux-host> 'uname -s'          # 必须是 Linux
ssh <user>@<linux-host> 'mkdir -p /tmp/radian-linux-sandbox'
scp -r spikes/linux-sandbox/{src,scripts} <user>@<linux-host>:/tmp/radian-linux-sandbox/
ssh <user>@<linux-host> 'cd /tmp/radian-linux-sandbox && ./scripts/run_all.sh'
```

`run_all.sh` 会：编译 `ll_probe`/`llsandbox` → 探测 Landlock → 依次跑 bwrap/Landlock/组合的 sandbox 与 control 矩阵 → 跑变异 → 跑启动开销 → 打印孤儿进程检查。所有结果写到 VM 的 `data/`，本目录的 `data/` 是拉回来的同一份。

关键实现（都在 `spikes/linux-sandbox/`）：

- `scripts/sbwrap.sh`：bubblewrap 启动器。`--die-with-parent --new-session --unshare-all`（网络关闭时；打开网络则只 unshare user/ipc/pid/uts/cgroup）；只读挂 `/usr /bin /lib /etc` 与 ROOT，可写挂工作区，再把 `.git`（和 worktree 指针）**只读覆盖**回去；`--clearenv` + 白名单 `--setenv`。
- `src/llsandbox.c`：Landlock 应用器（`create_ruleset` → `add_rule` → `restrict_self` → `execvp`），`--rw/--ro/--deny-net`。
- `scripts/llwrap.sh`：Landlock 启动器。
- `scripts/combo.sh`：bwrap 外层 + Landlock 内层。
- `src/mcp_echo.py` + `scripts/mcp_check.py`：**最小 stdio MCP 服务**（`initialize`/`tools/list`/`echo`/`write_file`）。仓库的 `mcp-fixture` 是 Mac 二进制，VM 里跑不了，所以用它替代——这是替代品，不是仓库 fixture。

### 复现的一个隐含前提（评审时踩到，2026-10-12）

**`src/` 与 `scripts/` 必须放在 `ROOT` 里面。** `sbwrap.sh` 只把 `$ROOT` 只读挂进沙箱（`--ro-bind "$ROOT" "$ROOT"`），所以沙箱内要能执行 `llwrap.sh`、要能读 `src/mcp_echo.py`，这些文件就得在 `ROOT` 之下。上面那四行复现命令恰好满足（都放在 `/tmp/radian-linux-sandbox`），但这是个**未写明的前提**。

评审者第一次重跑时把脚本放在 `/tmp/rv-<pid>`、`ROOT` 另给一个目录，于是得到 `bwrap-sandbox ok=20 mismatch=1`、`combo-sandbox ok=15 mismatch=6`——**全部是 `llwrap.sh`/`mcp_echo.py` "No such file or directory"**，即"应当成功"的那几条（6a/6b、8a、9、10i、10j）因为沙箱里看不见脚本而失败；**所有"应当失败"的逃逸用例（1–5、7、10a–10h）在那次跑法下仍然全部正确失败**，所以安全结论不受影响。按文档的放置方式重跑后，评审者复现了 `bwrap/landlock/combo` 各 `ok=21 mismatch=0`，以及 m1–m6 的变异翻转（`m1 ok=14 mismatch=7`、`m6 ok=11 mismatch=10` 等）。

教训：原型的自举文件与被测目录纠缠在一起时，换个放置位置就会产出看起来像"沙箱失效"的结果。以后这类 spike 应把自己的脚本挂载显式写进启动器，而不是依赖"恰好在 ROOT 里"。

## 3. Landlock 的版本与能力边界（实测）

`data/landlock-probe.txt`（`ll_probe` 逐位试 `landlock_create_ruleset`）：

- **ABI = 10**。`/sys/kernel/security/landlock/abi_version` 在本机不存在，所以用系统调用探测（`LANDLOCK_CREATE_RULESET_VERSION`）。
- **全部 16 个 `access_fs` 位都被接受**：`EXECUTE, WRITE_FILE, READ_FILE, READ_DIR, REMOVE_DIR, REMOVE_FILE, MAKE_CHAR, MAKE_DIR, MAKE_REG, MAKE_SOCK, MAKE_FIFO, MAKE_BLOCK, MAKE_SYM, REFER, TRUNCATE, IOCTL_DEV`。
- **网络位被接受**：`BIND_TCP`、`CONNECT_TCP`（ABI ≥ 4 才有；本机有）。**这是 Landlock 相对 macOS SBPL 的一个明确优势：内核级网络拒绝，不依赖 net namespace。**
- **`scoped` 位被接受**：`ABSTRACT_UNIX_SOCKET`、`SIGNAL`。
- **能力边界（关键）**：Landlock 规则是"允许的并集"，**没有 deny 规则，也没有"更深路径覆盖更浅允许"**。因此"工作区可写、但工作区里的 `.git` 只读"**无法用单个 Landlock ruleset 表达**。实测：`EXPECT_GIT_RO=off` 下 10a–10h 全部成功（`data/matrix-landlock-sandbox.txt`）。macOS SBPL 与 bwrap 都能做到（前者靠更深 deny，后者靠 mount 层的 `--ro-bind`）。
- 另一个实测的集成代价：Landlock **连读也约束**。`git` 把"`~/.config/git/config` 被拒"当成**致命**（不是"不存在"）并 `fatal: unknown error occurred while reading the configuration files`。启动器必须把 `HOME` 指向一个允许的、空的目录（`llwrap.sh` 这么做）。原始错误见 `data/landlock-home-finding.txt`（`fatal: unknown error occurred while reading the configuration files`，rc=128）。这与环境变量一样，是 **spawner 侧**的配合，不是 Landlock 自动做到的。

## 4. 启动开销

`data/bench.txt`（20 次，warmup 后计时；含进程 spawn 全程 wall time）：

| | median | p95 |
|---|---|---|
| 裸 `/bin/true` | 0.54 ms | 1.11 ms |
| `sbwrap.sh` + `/bin/true` | 14.50 ms | 22.89 ms |
| `llwrap.sh` + `/bin/true` | 11.64 ms | 16.53 ms |

- **bwrap 开销（中位数差）≈ 14 ms**；macOS `sandbox-exec` 是 ~6.8 ms，**本机 bwrap 大约慢一倍**（VM + aarch64 + userns/mount 设置，不能直接外推到裸机）。
- **Landlock 那一行的 11.6 ms 主要不是 Landlock 的开销，而是 `bash` 启动器本身**：`llwrap.sh` 是 bash 脚本，裸 `/bin/true` 是 exec；Landlock 只是三个系统调用。**Landlock 自身的开销没有单独测**（未验证）。真实实现里它应该是微秒级（在同一进程内 `restrict_self` 后 exec，无额外进程/命名空间）。

## 5. 第 10 条：共享 `.git` 只读（逐子项）

`data/matrix-bwrap-sandbox.txt`：

| 子项 | bwrap | Landlock |
|---|---|---|
| 写 `.git/hooks/*` | 失败（`Read-only file system`） | **成功** |
| 写 `.git/config` | 失败 | **成功** |
| 写 `.git/info/*` | 失败 | **成功** |
| `git config core.hooksPath` | 失败（`could not lock config file`） | **成功** |
| `git add`（`.git/index.lock`） | 失败 | **成功** |
| worktree 指针文件 `wt/.git` | 失败 | **成功** |
| 共享 `.git/worktrees/<wt>/gitdir` | 失败 | **成功** |
| 经 worktree 内符号链接写共享 `.git` | 失败 | **成功** |
| `git status` / `git log`（只读用法） | 成功 | 成功 |

bwrap 的实现是 mount 层：先把工作区整体 `--bind`（可写），再把 `$WS/.git` 与 `$WS/wt/.git` 用 `--ro-bind` 覆盖回去。Landlock 做不到这个 carve-out。

## 6. 变异检查（每条都让"沙箱拒绝"变成"控制组结果"）

`data/mutations.txt`，全部按预期翻转：

| # | 变异 | 结果 |
|---|---|---|
| M1 | bwrap 去掉 `.git` 只读 bind | 10a/10b/10e/10g/10h 由 fail 变 **success**（10f 仍 fail——它由单独的 `RO_EXTRA` 只读绑定保护，说明是两处 carve-out 各自生效） |
| M2 | bwrap 共享网络命名空间（`NET=on`） | 第 5 条由 fail 变 **success**（`b'pong'`） |
| M3 | bwrap 不清空环境（`CLEARENV=off`） | 第 6a 条由 `<unset>` 变 **`leaked-secret-value`** |
| M4 | bwrap 不让越界目标可见（去掉 ROOT 只读 bind 且 `RO_EXTRA` 不含 outside） | 第 1 条仍"fail"，但原因是 **`No such file or directory`**——证明"失败"可能是假象；对照 `*-control.txt` 与 ROOT 绑定才是真证据 |
| M5 | Landlock 去掉工作区可写规则 | 第 9 条由 success 变 **fail**（`in.txt: Permission denied`） |
| M6 | Landlock 去掉网络拒绝（`DENY_NET=off`） | 第 5 条由 fail 变 **success**（`b'pong'`） |

## 7. 与 macOS 的差异表

| 维度 | macOS（`sandbox-exec`+SBPL） | Linux bwrap | Linux Landlock（ABI 10） |
|---|---|---|---|
| 文件写边界 | SBPL `allow file-write* (subpath WS)` | mount 命名空间 + `--bind/--ro-bind` | `handled_access_fs` + path-beneath 允许规则 |
| `.git` 只读 carve-out | ✅ 更深 `deny subpath` 覆盖更浅 allow | ✅ `--ro-bind` 覆盖 | ❌ **不能**（规则是并集，无 deny） |
| 网络关闭 | ✅ SBPL 不写 network allow | ✅ `--unshare-net`（新 netns 无路由） | ✅ `handled_access_net` 无允许规则（ABI≥4） |
| 读整个 `$HOME` | ❌ **能读**（`(allow file-read*)`，已记为风险） | ✅ 默认**看不到**（只挂 /usr,/bin,/lib,/etc + 工作区，`HOME` 指向空目录） | ✅ 读也被约束（只允许列出的路径）——更强，但会打断 git，需要 `HOME` 配合 |
| 环境变量 | spawner `env_clear`+白名单 | spawner `--clearenv`+`--setenv` | ❌ 机制无关；只能 spawner 做 |
| 子进程继承 | ✅ 新进程继承沙箱 | ✅ 命名空间继承 | ✅ `NO_NEW_PRIVS` 后 fork/exec 继承 |
| MCP stdio | ✅ | ✅ | ✅ |
| 依赖 | `sandbox-exec` **已弃用**、SBPL 私有、随版本漂移 | 非特权 userns（或 setuid bwrap） | 内核 ≥ 5.13（网络需 ≥ 6.7） |
| 启动开销 | ~6.8 ms | ~14 ms（本 VM，含 bash 启动器） | 未单独测（`bash` 启动器 ~11.6 ms 占主导） |

## 8. 未验证的部分

- **其他发行版**：只在 Arch ARM + 内核 7.2.6 上测过；Debian/Ubuntu/Fedora 的内核版本、`bwrap` 版本、LSM 配置未测。
- **容器内**：Docker/Podman 里的嵌套 userns 与 LSM 行为未测（很可能 `--unshare-all` 失败）。
- **无特权 userns 被禁用**：本机 `max_user_namespaces=31623`（可用）。若发行版禁用（或 `kernel.unprivileged_userns_clone=0`），**bwrap 需要 setuid 安装**；setuid bwrap 的行为与安全性**未验证**。Landlock 不受此限（进程内应用，无需 userns）——这是 Landlock 的一个实际优势。
- **真实主机而非 VM**、**x86_64**：未测（结论带版本号与环境限定，参考 ADR 0009 的教训）。
- **Landlock 自身开销**未单独测量（§4）。
- **seccomp**：本切片没有使用，也没有评估（bwrap/Landlock 已满足 10 条）。seccomp 与 Landlock 的组合、以及对 MCP 子进程的额外约束未验证。
- **`(allow network*)` 的细粒度**（按地址/端口）未测；本切片只做"全关"。
- **bwrap 的 `--unshare-pid` 与 MCP 子进程回收**：矩阵里 MCP 正常，但没有测崩溃/孤儿场景下的回收细节（只在 §9 做了一次性 `ps` 检查）。
- **`ABSTRACT_UNIX_SOCKET`/`SIGNAL` scoped 位**被内核接受，但本切片**没有测它们的实际语义**（例如能否阻止经抽象 socket 逃逸 IPC）。

## 9. 孤儿进程

`data/orphans.txt` 为空（`ps -eo pid,ppid,comm,args | grep -E 'radian-linux-sandbox|bwrap|llsandbox|mcp_echo|listener.py'`）。矩阵每次运行后 `kill` 监听进程并 `wait`；`bwrap` 用 `--die-with-parent`。

## 10. 对计划的影响（ADR 0012 第 2 条）

ADR 0012 决定第 2 条：**Linux 后端在本 spike 通过前禁用；请求沙箱但后端不可用时必须明确失败，绝不自动降级为无沙箱。**

本 spike 的结论支持**有条件地解除禁用**：

1. **可以解除**的前提（bwrap 后端）：`bwrap` 可用 **且** 非特权 userns 可用（或使用经过验证的 setuid 安装）；`.git` 与 worktree 元数据用 `--ro-bind` carve-out；`--clearenv` + 白名单；`HOME` 指向允许的空目录。满足这些时 10 条全过。
2. **Landlock 只能作为可选加固层**（例如读约束、网络拒绝），**不能单独作为 Linux 的 `workspace-write` 后端**，因为第 10 条做不到。它适合用在"没有 userns 但内核够新"的环境里做**部分**约束，但要如实说明它不满足 `.git` 只读。
3. 后端不可用（无 bwrap / userns 被禁）时必须**明确失败**，与本 ADR 一致；不要用 Landlock 冒充完整的 `workspace-write`。

**需要 决定**：是否接受 bwrap 作为 Linux `workspace-write` 的唯一后端（以及 setuid bwrap 是否在允许清单内）。

## 11. 已接受的代价

- **结论只对 VM + 内核 7.2.6 + Arch ARM 有效**；真实主机、其他发行版、容器内都是未验证项。
- bwrap 的启动开销（~14 ms）比 macOS（~6.8 ms）高一倍，且需要 userns。
- Landlock 的读约束比 macOS 强，但会打断 `git`（`HOME` 必须配合），且不能 carve out `.git`。
- 本 spike 用 `python3` 的最小 MCP 服务替代仓库 fixture（Mac 二进制），**不是**同一份实现；MCP 协议行为的覆盖有限。
- 组合（bwrap+Landlock）可用且 10 条全过，但本切片没有测"组合在无 userns 环境下"的表现（那时 bwrap 不可用，只剩 Landlock）。
