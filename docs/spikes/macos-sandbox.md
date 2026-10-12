# Phase 0b 可行性验证：macOS 沙箱（sandbox-exec + SBPL）

- 结论：**通过**（8 项标准全部满足；其中"读取未授权环境变量"由 spawner 侧完成，沙箱机制本身做不到——已按任务单要求如实记录，不作为沙箱能力 claim）
- 环境：macOS 27.0.1（26A434，arm64），`/usr/bin/sandbox-exec` 系统自带
- 时间：2026-10-12，工作量远小于 3 天时间盒
- 原型：`spikes/macos-sandbox/`（独立 cargo 项目，不属于根 workspace）；复现方式见下

## 结果总表

| # | 标准 | 结果 | 证据 |
|---|---|---|---|
| 1 | 写工作区外路径（绝对路径）失败 | ✅ | `data/results.txt` `escape_writes` 组，`Operation not permitted` |
| 2 | 用 `..` 绕出工作区写失败 | ✅ | 同上 |
| 3 | 工作区内符号链接 → 工作区外，经链接写失败 | ✅ | 同上（subpath 按解析后路径匹配） |
| 4 | 网络关闭时：对本脚本自开的 127.0.0.1 监听端口 ×2 connect 均失败 | ✅ | `network_off` 组两条 `connect` 用例；未连接任何外部主机 |
| 5 | 读取未授权环境变量 | ✅（spawner 侧） | sandbox-exec **不过滤**环境变量（继承即可见，`environment` 组第 1 条）；`Command::env_clear` + 白名单后未授权变量为 `<unset>`、白名单内可见；MCP 子进程同样受白名单约束（`mcp_stdio` 组第 3 条） |
| 6 | 子进程继承限制（沙箱里再 sh -c 两层） | ✅ | 嵌套 sh 写工作区外仍被拒；嵌套写工作区内仍成功（继承不破坏正常用法）。`sandbox(7)`："New processes inherit the sandbox of their parent" |
| 7 | MCP stdio 在沙箱内可用（initialize 往返 + 一次工具调用） | ✅ | `mcp_stdio` 组：initialize 返回 serverInfo、`echo` 工具结果与参数一致；fixture 用仓库自带 `mcp-fixture`（只读使用） |
| 8 | 沙箱内读写工作区内文件成功 | ✅ | `normal_use` 组（写 + cat 回读内容一致） |

计划 v5 §4.3 额外要求的共享 `.git` 逃逸清单（本任务单表格之外，一并验证）：

| 尝试 | 结果 |
|---|---|
| 写 `.git/hooks/*`、`.git/config`、`.git/info/*`（普通仓库，.git 在工作区内） | ✅ 全部被拒（更深的 deny subpath 覆盖更浅的 allow subpath） |
| `git config core.hooksPath`（写 config 的正门） | ✅ 被拒（could not lock config file） |
| `git add`（写 `.git/index.lock`） | ✅ 被拒 |
| worktree 布局：改 `wt/.git` 指针文件 | ✅ 被拒（literal deny） |
| worktree 布局：写共享 `.git/hooks`、`.git/worktrees/<wt>/gitdir` | ✅ 被拒（共享目录整体 deny） |
| 经 worktree 内符号链接写共享 `.git` | ✅ 被拒 |

对照组：`normal_use` 里 `git --no-optional-locks status` / `git log` / `cargo --version` 在沙箱内正常；worktree 内 `git status` 正常。`.git` 只读不破坏只读类 git 用法。

## 复现

```sh
cd <home>/Code/radian-pi-spikes
cargo build -p agent-extensions --bin mcp-fixture   # 仓库自带 fixture，只读使用
cd spikes/macos-sandbox
cargo run -- run    # 31 例全量用例 + 放行矩阵 → data/
cargo run -- bench   # 启动开销 20 次 → data/startup-overhead.csv
cargo test           # 每个关注点一组断言（9 个测试）
```

证据文件（均在本目录）：`data/results.txt`（逐例结果）、`data/results.json`、`data/profile-main.sb`（实际用的 SBPL 全文）、`data/profile-net-on.sb`、`data/profile-worktree.sb`、`data/allow-matrix.json`、`data/startup-overhead.csv`、`data/bench.txt`。

孤儿进程检查（fixture 与 sleep 均无残留）：

```sh
$ ps -axo pid,comm | grep -E 'mcp-fixture|sleep 600|sandbox-exec' | grep -v grep
（无输出）
```

## 实际使用的 SBPL 配置

`data/profile-main.sb`（路径为运行时 realpath 后代入的生成结果；模板见 `spikes/macos-sandbox/src/lib.rs::write_profile`）：

```lisp
(version 1)
(deny default)
(allow file-read*)
(allow file-test-existence)
(allow process-exec*)
(allow process-fork)
(allow mach-lookup)
(allow sysctl-read)
(allow file-write-data (literal "/dev/null"))
(allow file-write* (subpath "<工作区 realpath>"))
(deny file-write* (subpath "<工作区 realpath>/proj/.git"))
```

要点：

- **default-deny + 选择性 allow**，不用 `(allow default)` 再打补丁。写权限只开工作区子树；更深的 `deny subpath` 可以在工作区内 carve-out（.git 只读就是这么做的，实测有效）。
- 网络关闭 = 不写任何 network allow；打开 = 加一行 `(allow network*)`（对照用例证明两种状态都真实生效）。
- subpath 匹配按**解析后路径**：`$TMPDIR` 是 `/var/folders/...` 的符号链接，profile 里必须写 realpath（`/private/var/...`），否则规则不命中。符号链接逃逸被拦截也说明匹配发生在解析后的目标路径上。

## 必须放行的路径/操作（实测矩阵，`data/allow-matrix.json`）

对 `ls`、`cat`、`git status`、`cargo --version`、`mcp-fixture initialize` 五类命令，逐条移除一个 allow 后重测：

| 移除的 allow | ls | cat | git status | cargo | mcp init |
|---|---|---|---|---|---|
| （不移除） | ok | ok | ok | ok | ok |
| `file-read*` | × | × | × | × | × |
| `file-test-existence` | ok | ok | ok | ok | ok |
| `process-exec*` | × | × | × | × | × |
| `process-fork` | ok | ok | ok | ok | ok |
| `mach-lookup` | ok | ok | ok | ok | ok |
| `sysctl-read` | ok | ok | ok | × | × |

- `file-read*` 与 `process-exec*` 是全部命令的硬依赖。
- Rust 二进制（cargo、fixture）启动需要 `sysctl-read`（查询 CPU 特性）。
- `git` 需要 `/dev/null` 可读写（`file-write-data` literal 放行；不带它时 `fatal: could not open '/dev/null'`）。这是逐路径最小放行的第一个实例；独立 profile 默认连 `/dev/null` 都不放开，系统模板 `/System/Library/Sandbox/Profiles/system.sb` 里则显式放行了它。
- `process-fork` 在这些单命令场景不触发，但嵌套 shell（sh -c）需要它，实际 profile 保留。

## 启动开销（`data/startup-overhead.csv`，20 次，warmup 2 次后计时，含进程 spawn 全程 wall time）

| | median | p95 |
|---|---|---|
| 裸 `/usr/bin/true` | 1.18 ms | 1.33 ms |
| `sandbox-exec -f profile -- /usr/bin/true` | 7.96 ms | 10.31 ms |
| 开销（中位数差） | 6.79 ms | — |

## 风险：`sandbox-exec` 已弃用（本机证据）

- `man sandbox-exec`（本机）：「The sandbox-exec command is DEPRECATED. Developers who wish to sandbox an app should instead adopt the App Sandbox feature described in the App Sandbox Design Guide.」
- `/System/Library/Sandbox/Profiles/system.sb` 文件头：「Apple System Private Interface and are subject to change at any time and without notice… auto-generated and not user editable」——SBPL 语法与操作名是私有接口，随 macOS 版本漂移。
- 本机实测的漂移实例：macOS 27 上 `(version 1)` 方言里 `process-fork*`、`mach-lookup*` 已是 unbound（报 `unbound variable`），须写 `process-fork`、`mach-lookup`；系统自带模板已迁移到 `(version 3)`。合法操作名集合以 `/System/Library/Sandbox/Profiles/` 下模板为准（本 spike 的操作名全部来自该目录实测）。
- 替代方案：man page 指向 App Sandbox（面向应用打包，不适合给任意 CLI 子进程套壳——此句为分析，不是查证结论）。其他替代（如第三方沙箱库、未来新 API）：**未验证**（本任务无外部网络访问，不做无据推断）。
- 缓解：把 profile 生成与操作名集中在 radian-coding 的 sandbox backend 一处，启动时对 `sandbox-exec` 做一次自检（跑一个探针命令验证 deny/allow 仍然生效），失败则按计划把 `workspace-write` 档位的 shell/MCP 诚实禁用。

## 未验证的部分

- 未在 macOS 27 以外的版本上运行（结论带版本号；跨版本兼容风险见上）。
- `cargo` 只测了 `--version`；**没有**在沙箱里跑真实 build（build 需要写 target/、读 `~/.cargo` 注册表缓存、可能要网络拉依赖，属于产品化时要解决的放行清单，不属于本可行性问题）。
- 网络只验证了「关闭」与「全开」两档；(allow network*) 细粒度（按地址/端口放行）未验证。
- XPC/mach 服务只做了全量 `(allow mach-lookup)`；最小化 mach 白名单未做。
- Intel Mac、FileVault/受管控设备等差异未验证。
- `sandbox-exec` 与 App Sandbox 之外的替代机制未查证（无外部网络）。
- 时间注入类逃逸（如通过已打开的 fd 继承可写句柄）未系统测试——`sandbox(7)` 明确「already-open fd 不受限」，这要求 spawner 不向沙箱子进程传递可写 fd（Radian 的 Command 构造天然如此，但没有写专门用例）。

## 对计划的影响

- 证实：`workspace-write` 档位在 macOS 上**可以**用 `sandbox-exec` + 自写 SBPL 实现（shell 与 MCP 子进程都被强制约束），v5 计划 §4.3 的共享 `.git` 只读要求在 SBPL 里可用「更深 deny subpath」落实。
- 证实：环境变量过滤必须由 spawner（`Command::env_clear` + 白名单）承担，架构上归属 radian-coding 的进程启动层，不能依赖沙箱。
- 推翻/修正：无。v5 的假设（先 spike、未通过平台诚实禁用 shell/MCP）不需要调整。
- 新增事实（计划没写、落地时要处理）：① subpath 必须用 realpath；② `sysctl-read`、`/dev/null` 这类"看不见的系统需求"存在，最小放行清单要按命令实测维护；③ profile 语法随 macOS 版本漂移，需要启动自检 + 版本适配层。

## 复核补充（合并时由评审者追加，不是 spike 作者的结论）

评审者用手写的同款 profile 独立探测，结果与上面的结论一致（工作区外写入、`$HOME`/`/tmp` 写入、unix socket bind、`mkfifo`、本机端口连接均被拒；工作区内写入成功），另外发现两点上文未列出的边界：

1. **沙箱内可读整个 `$HOME`。** profile 是 `(allow file-read*)`，所以 `cat ~/.zshrc`、`ls ~/.ssh` 在沙箱里都成功。这是"读全盘、只写工作区"档位的固有语义，但它意味着：被沙箱的 shell 或 MCP 子进程能读到 SSH 密钥、云凭据文件等，环境变量白名单对此无效。上面的标准 5 只覆盖环境变量，**不覆盖文件内容**。是否要对 `~/.ssh`、`~/.aws`、`~/.gnupg`、浏览器配置目录等做 `(deny file-read* ...)`，是 permission 档位设计（v5 的 SandboxMode 措辞）时要定的事；SBPL 里更深的 deny 可以覆盖更浅的 allow（`.git` 只读已证明），技术上可行，但未验证对常见命令的副作用。
2. **`(allow mach-lookup)` 是全开。** `osascript` 在沙箱内可运行。评审者试了经 Finder 在工作区外创建文件夹（Apple Events），未成功（`sandbox_extension_issue_file failed ... Operation not permitted`），但这只是一次探测，**不是**"mach 服务无法被利用"的证明。最小化 mach 白名单仍是未验证项，产品化前应做。
