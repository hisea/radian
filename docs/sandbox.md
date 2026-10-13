# Sandbox backends（Phase 3）

`crates/radian-sandbox` 把两个 spike 变成产品代码：macOS 的 `sandbox-exec` + SBPL，和 Linux 的
`bubblewrap`。它只提供一件事——**受约束地启动一个子进程**；审批（`ApprovalPolicy`）与规则集
（`ToolRuleSet`）不在本 crate，也**没有**接进 `agent-cli`。

**消费者（2026-10-12，Phase 3 shell 切片）：** `radian-coding` 的 `shell` 工具是第一个消费者
（`CodingTools::with_sandbox`）；没有 `Sandbox` 时它根本不注册。三点值得记在这里：① `FullAccess`
下后端是透明的（不探测、不包裹），但 shell 工具仍自己按 `allow_env` 清环境——**full 是
"文件系统/网络不设限"，不是"把宿主环境交给模型控制的 shell"**；**full 不放开环境变量白名单**
＼�。② 进程组清理：shell 工具用 `setsid` 让子进程自成会话与进程组
（没有控制终端），超时/取消/leader 退出都杀整组（ADR 0016 取代了原先的 `process_group(0)`）。
③ 输出截断也是 shell 工具自己的策略：每流独立预算、保留头尾（尾部更大，因为失败命令的错误在
末尾）、边读边丢，沙箱不参与。

**ADR 0016（2026-10-13）：** `workspace` 档位改为**开网**（出站与本机回环 connect/bind/listen），
并放开 **pty**（`/dev/ptmx` 与 `/dev/ttys[0-9]+` 的写与 `file-ioctl`，后者**只**对这两类路径）；
`safe` 仍关网、无设备。shell 工具用 `setsid` 让受约束命令没有控制终端，配合内核的
controlling-terminal 检查堵住 `TIOCSTI`（矩阵新增 `tiocsti_outside_terminal`）。

## 硬规则：请求沙箱但后端不可用 = 明确失败

`Sandbox::new(mode, workspace)` 与 `Sandbox::from_policy` 对 `Safe` / `WorkspaceWrite` **先探测
后端**（跑一个真子进程，证明 deny 与 workspace allow 都生效），探测失败就返回 `SandboxError`。
**没有任何路径**把受约束档位变成无沙箱运行；`FullAccess` 只有调用方点名才会用到，且不探测。
（ADR 0012，决定第 2 条。）测试：`an_unavailable_backend_is_an_error_and_never_a_downgrade`、
`a_failed_probe_is_an_error_not_a_downgrade`、`full_access_is_transparent_and_unprobed`。

探测为什么必须是"跑探针"而不是"二进制存在"：`sandbox-exec` 已弃用，SBPL 操作名是私有接口并会随
macOS 版本漂移（spike 记录过 `process-fork*` 变 unbound）。探针在一个临时 workspace 里同时验证
"工作区外写被拒"和"工作区内写成功"，两者缺一即失败。

## 后端与策略

- `SandboxMode::{Safe, WorkspaceWrite, FullAccess}` 是产品概念；SBPL / bwrap 的参数只是实现。
- `SandboxPolicy`：canonical workspace、`network`、`read_only`（git 元数据自动发现：`.git` 目录、
  worktree 指针文件及其共享 common dir）、`deny_read`（默认 `~/.ssh`、`~/.aws`、`~/.gnupg`、
  `~/.netrc`、浏览器配置等）、`allow_env` 白名单。
- **ADR 0020（2026-10-13）**：宿主在 enforced 档位把**实际的会话目录**（`--session-dir`
  可能已改位）与 `~/.radian/memory` 加进 `deny_read`，并在建沙箱前把它们创建为 0700（Linux 的
  bwrap 只能遮住**已存在**的路径）；`~/.radian/config` 与 `~/.radian/cache` 保持可读。
- **macOS**：`(deny default)` + 选择性 allow；只写工作区；更深的 `deny` 覆盖 git 元数据与敏感读；
  默认不加 `(allow network*)`；`/dev/null` 放行；`TMPDIR` 指向工作区内 `.radian-sandbox/tmp`。
- **Linux**：`--unshare-all`（网络关闭）或部分隔离（开启）；宿主根 `--ro-bind / /` + 工作区
  `--bind`；git 元数据 `--ro-bind` 覆盖；敏感目录 `--tmpfs`（目录）或 `/dev/null`（文件）；
  `--clearenv` + 白名单；`HOME`/`TMPDIR` 在工作区内 `.radian-sandbox/{home,tmp}`。

## 逃逸清单（两平台各 39 例，全部带"关掉沙箱后成功"的对照）

`radian-sandbox` 的 `harness`（`testing` feature）跑同一套矩阵：每条"应当失败"的用例都在
`WorkspaceWrite` 下跑一次、在 `FullAccess` 对照下跑同一命令。`Case::passed()` 要求
`沙箱失败 && 对照成功`，否则不算通过。

| 组 | 用例 | macOS | Linux |
|---|---|---|---|
| escape_write | 绝对路径写工作区外 / `..` / 经符号链接 / 临时区 | 拒 | 拒 |
| symlink | 符号链接父目录逃逸 | 拒 | 拒 |
| network_off | 连本机 127.0.0.1 监听端口 | 拒 | 拒 |
| environment | 白名单外的变量对子进程不可见；白名单内的可见 | ✅ | ✅ |
| inheritance | 两层 `sh -c` 仍受约束；工作区内嵌套装写成功 | ✅ | ✅ |
| normal_use | 工作区内写回读；工作区外只读 | ✅ | ✅ |
| git_readonly | 写 `.git/hooks`、`.git/config`、`.git/info`、`git config core.hooksPath`、`git add`、worktree 指针、共享 gitdir、经符号链接写共享 `.git` 全部被拒；`git status`/`git log` 可用 | 8 拒 + 2 可 | 8 拒 + 2 可 |
| sensitive_read | 假 `~/.ssh/id_ed25519` 打开失败（只断言 exit，不读内容）；普通文件可读 | ✅ | ✅ |
| radian_home | ADR 0020：`~/.radian/sessions/<file>` 与 `~/.radian/memory/<file>` 读失败；`config/<file>` 与 `cache/<file>` 可读；`--session-dir` 指向的目录也读失败 | 3 拒 + 2 可 | 3 拒 + 2 可 |
| mcp_stdio | 最小 stdio MCP（initialize / tools/list / tools/call echo）可用 | ✅ | ✅ |
| signals | 给自己的子进程、孙进程发信号可以；给沙箱外同用户进程发信号被拒 | 2 可 + 1 拒 | 2 可 + 1 拒 |

证据：`crates/radian-sandbox/tests/data/escape-macos.{txt,json}`、
`escape-linux.{txt,json}`（后者在 VM 上生成后拉回）。每份证据含一次 `ps` 孤儿检查（两平台均为
"no orphan processes"；矩阵运行结束后没有残留 `bwrap`/`mcp_echo.py`/`sandbox-exec`）。

**信号（2026-10-13，dogfooding D8）**：原先 SBPL 没有任何 `signal` 规则，`(deny default)` 让沙箱内
进程连自己的子进程都杀不掉（`cargo test` 里会 kill 子进程的测试因此 `EPERM`）。在 macOS 27 上用
`sandbox-exec` 实测了五种写法（自己的子进程 / 孙进程 / 同 profile 的另一个沙箱实例 / 沙箱外同组进程）：
`(target self)` 全拒；`(target children)` 拒孙进程；`(target pgrp)` 与不带过滤的 `(allow signal)`
**能杀沙箱外同进程组的进程**，不可用；`(target same-sandbox)` 子孙可、另一实例拒、外部拒——采用它。
Linux 的 bwrap 带 `--unshare-pid`，沙箱内看不到外部 pid，本来就满足（VM 上同一矩阵 34/34）。
矩阵的 `signals` 组钉住这三条；去掉规则、换成 `children`、换成不带过滤的 `signal` 三个变异各被抓住。

**Linux 与 macOS 的一个差异（已写进代码注释）**：bwrap 无法为**尚不存在**的敏感路径建挂载点
（根是只读的），因此只 mask 已存在的 deny 路径；SBPL 可以给未来才会创建的路径写规则。默认 deny
列表里不存在的项在 Linux 上暂不 mask。

## 敏感目录禁读的兼容性轮次（macOS，`workspace` + 默认 deny 列表）

`crates/radian-sandbox/tests/compat_macos.rs` 逐条跑并断言：

| 命令 | 结果 | 说明 |
|---|---|---|
| `git status` / `git log` / `git config --get` | ✅ | 需要读 `~/.gitconfig`，在 `file-read*` 下可读 |
| `ssh -V` | ✅ | 只打印版本，不读 `~/.ssh` |
| `cargo --version` | ✅ | |
| **`cargo build`（无依赖项目，`--offline`）** | ✅ | spike 未验证项：可在沙箱内 build；`target/` 在工作区内，`~/.cargo`/`~/.rustup` 可读，`TMPDIR` 指向工作区 |
| `git add` / `git commit` | ❌ | **设计使然**：`.git` 有意只读（ADR 0012）。需要提交时应在沙箱外或 `full` 下做；不为此放宽规则 |

**没有为兼容性放宽任何规则**：唯一新增的放行是 `/dev/null`（git 需要）与 `TMPDIR` 指向工作区内
（两平台一致），都属于"工作区可写"的既有语义。不接受的做法——比如为了 `git commit` 而让 `.git`
可写——没有采用。

## 启动开销

| 平台 | 裸 `true`（中位） | 沙箱内 `true`（中位） | 开销 |
|---|---|---|---|
| macOS 27.0.1 / arm64 | 0.98 ms | 6.89 ms | **~5.9 ms** |
| Linux VM（Arch ARM，内核 7.2.6，bwrap 0.12.0） | 0.86 ms | 8.84 ms | **~8.0 ms** |

（20 次采样，warmup 2 次。Linux 数字经 ssh 在 VM 上测得；macOS 数字本机测得。）

## 表述约束（硬要求，ADR 0012 决定第 4 条）

macOS 后端**全开 `mach-lookup`**，`mach-lookup` 最小化排到 Phase 7。因此 **IPC 边界尚未收紧**，
任何面向用户的文案、文档或 UI **不得**把"敏感目录禁读"表述成完整的凭据隔离保证。允许的说法只有：

> 工作区外不可写、敏感目录默认不可读，IPC 边界尚未收紧。

另注：环境变量过滤属于**启动层**（本 crate 的 `command`），不是沙箱机制本身；`sandbox-exec` 与
bwrap 都不过滤环境。

## 复现 Linux 运行（VM）

```sh
# 第一步必须确认是 Linux；返回 Darwin 立即停止（那是 owner 的 Mac）。
ssh <user>@<host> 'uname -s'          # Linux

SYS=$(rustc --print sysroot)
RUSTFLAGS="-C link-self-contained=yes" \
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER="$SYS/lib/rustlib/aarch64-apple-darwin/bin/rust-lld" \
  cargo build -p radian-sandbox --bin sandbox-escape --features testing \
    --target aarch64-unknown-linux-musl
scp target/aarch64-unknown-linux-musl/debug/sandbox-escape <user>@<host>:/tmp/
ssh <user>@<host> '/tmp/sandbox-escape --out /tmp/radian-sandbox-evidence; echo EXIT=$?'
scp <user>@<host>:/tmp/radian-sandbox-evidence/escape-linux.* crates/radian-sandbox/tests/data/
```

VM 上只写 `/tmp` 下自建目录，不碰 `~/.ssh` 或任何真实凭据。矩阵的敏感读用例使用自建的假 key，
并且只重定向到 `/dev/null`、只看退出码，从不读内容。

## Linux 独立复核（2026-10-12，评审者本人在 VM 上重跑）

**环境**：`ssh <user>@<host>`，Arch Linux ARM（aarch64，内核 7.2.6，bwrap 0.12.0，git 2.55.0）。
第一条命令确认 `uname -s` = Linux。为了跑**真正的测试套件**（而不是交叉编译的驱动），在 VM 里装了
Rust：下载 `rustup-init`（static.rust-lang.org，aarch64）后用
`RUSTUP_HOME=/tmp/radian-rust/rustup CARGO_HOME=/tmp/radian-rust/cargo ./rustup-init -y --profile
minimal --default-toolchain 1.88.0 --no-modify-path` 安装，全部落在 `/tmp`，**未动系统文件、未动
`~/.ssh` 或任何凭据**。仓库用 tar 同步到 `/tmp/radian-linux/src`；因 `/tmp` 是 3.9G tmpfs，构建加
`CARGO_PROFILE_DEV_DEBUG=0` 只为缩小目标目录。

**1. 逃逸矩阵（独立重跑）**：`cargo test -p radian-sandbox` 的 `linux_escape_matrix` 通过；另用
`sandbox-escape --out` 独立跑一次，**26/26 PASS**，并与仓库里的 `escape-linux.{txt,json}` 逐条比对
（表的 group/case/expect/sandbox/control/verdict 列与 JSON 的
`sandbox_succeeded`/`control_succeeded`/`passed`）**零差异**。每条"应当失败"都带关沙箱对照；矩阵
结束后 VM `ps` 无 `bwrap`/`mcp_echo.py`/`sandbox-exec` 孤儿。

**2. shell 工具（Linux 首跑）**：`cargo test -p radian-coding --test shell` **12/12 通过**：

- 超时与取消杀整个进程组：`a_timeout_kills_the_whole_process_group`、
  `cancellation_kills_the_whole_process_group`；跑完 VM `ps` 无 `sleep 600` 孤儿。
- 输出截断保留头尾：`the_end_of_a_long_output_is_kept`（尾部 `ERROR:` 行存活）、
  `each_stream_has_its_own_budget`、`memory_does_not_grow_with_the_output`（50 MiB 进 64 KiB 预算）。
- 环境变量不外泄：`the_host_environment_does_not_reach_the_shell`。
- `workspace-write` 边界：`workspace_write_confines_the_shell`（工作区内写成功、工作区外写失败、
  denied 读失败、连本机端口失败；每项带关沙箱对照）。
- 后端不可用不降级：把 `bwrap` 从 PATH 拿掉（可还原，不改系统文件），
  `sandbox-escape` 退出 2 且打印 `run is refused (never unsandboxed)`。

**3. 带依赖的 `cargo build`**：新增 `crates/radian-sandbox/tests/compat_linux.rs`（Linux-only，
镜像 `compat_macos.rs`），用一个依赖 `itoa` 的真项目 `cargo build --offline`，**成功**，产物在
工作区 `target/debug/` 内；`git status/log/config`、`ssh -V`、`cargo --version` 可用，
`git add`/`git commit` 按设计被 `.git` 只读拒绝。3/3 次通过。证据 `tests/data/compat-linux.txt`。
（VM 的 cargo/rustup 在 `/tmp` 下，测试把 `CARGO_HOME`/`RUSTUP_HOME` 加入 `allow_env`；CI 里它们在
`~/.cargo`/`~/.rustup`，本就在默认 allow 的 `HOME` 之下。）

**4. 发现并修复**：`radian-coding/tests/git.rs` 的
`git_status_and_diff_report_the_worktree_and_leave_dot_git_untouched` 在 Linux 上约 **1/3 概率失败**，
差分定位到 `.git/objects/maintenance.lock` 出现。根因是 setup 的 `git commit` 触发了 git 的**后台
auto-maintenance**（git 2.55 会跑；macOS 的 2.53 不跑），与本切片的只读 git 工具无关。修复：测试仓库
`git config maintenance.auto false` + `gc.auto 0`。变异（去掉这两行）在 VM 上 20 次跑出 **5 次失败**；
带上修复 20 次 **0 失败**。**未改动任何产品规则**。

**5. `agent-cli` 档位接线（Linux 首跑）**：先 `cargo build -p agent-extensions --bin mcp-fixture`
（`cargo test -p agent-cli` 单独跑不会自动构建它，测试里有明确的报错提示），再
`cargo test -p agent-cli` **49/49 通过**，连跑 3 次稳定。含
`a_failed_sandbox_probe_refuses_instead_of_degrading`（探针失败即拒绝，模型一次都没跑）、
`the_startup_text_does_not_claim_credential_isolation`、默认 `workspace-write`、无沙箱不注册 `shell`。

**启动开销**（VM 本机重测，20 次）：裸 `true` 中位 0.93 ms，沙箱内 8.73 ms，**~7.8 ms**（与旧值
~8.0 ms 一致）。

**2026-10-13 ADR 0020 追加**：矩阵新增 `radian_home` 组 5 条（sessions/memory 拒，config/cache
可，`--session-dir` 移动目录拒）。同一 VM 上重跑 **39/39 PASS**（macOS 本机同样 39/39）。

## 未验证 / 已知边界

- **只在 macOS 27.0.1（arm64）与一台 aarch64 Arch ARM VM（内核 7.2.6、bwrap 0.12.0、git 2.55.0）
  上跑过**；
  其他 macOS 版本、发行版、x86_64、容器内、setuid bwrap、无特权 userns 被禁用的发行版均未验证。
- **没有测** `network = true` 的细粒度（只做全开/全关）；`mach-lookup` 最小化未做；时间注入类
  逃生未系统测试（`already-open fd` 不受限，spawner 不向子进程传可写 fd，但没有专门用例）。
- `Safe` 档位在 Linux 上 `HOME`/`TMPDIR` 位于工作区但工作区只读，需要临时目录的命令会失败——
  这是 `Safe` 的诚实语义，未专门验证。
- 兼容性轮次只覆盖列出的命令；任意第三方构建系统可能还需要别的放行。

## 评审补充（合并时由评审者追加，2026-10-12）

1. **复现步骤里的文件名错了，已改。** 二进制叫 `sandbox-escape`（`Cargo.toml` 的 `[[bin]] name`），`scp … /tmp/` 后它在 `/tmp/sandbox-escape`，而原文下一行运行的是 `/tmp/radian-sandbox-escape`——照原文复现会直接失败。评审者按原文执行时撞上，已改成 `/tmp/sandbox-escape`。
2. **"真实 `cargo build` 能跑"的覆盖范围比测试本身更宽，但要说准。** 仓库里的兼容性测试验的是一个**零依赖**的 hello-world（`--offline`）。评审者另外在本机（macOS）沙箱里做了一次**带依赖的真实构建**：一个依赖 `serde_json` 的项目（共 11 个包，依赖从本机 cargo 缓存读取、`--offline`、网络关闭、`Cargo.lock` 在沙箱外预先生成），`cargo build` **成功**，用时约 8 秒。该探针依赖本机缓存状态，因此没有提交成测试。**未验证**：需要联网拉依赖的构建（`workspace-write` 默认关网，这类构建按设计会失败）、带 build script 写工作区外路径的 crate、其他构建系统。
3. **Linux 结果本轮评审未能独立重跑。**（**后续已补**：2026-10-12 评审者在 VM 上独立重跑，26/26，零差异；见文末“Linux 独立复核”节。） 驱动可以按文档交叉编译（得到静态链接的 aarch64 ELF），但评审时 VM 已不可达（`ssh: … No route to host`）。所以 Linux 的 26/26 目前**以本文作者提交的证据为准**（`tests/data/escape-linux.{txt,json}`），评审者核对了证据文件格式与"每条失败都带关沙箱对照"的列，但没有在 VM 上再跑一次。上一轮 Linux spike 的 26 条中的同类用例评审者是独立复现过的（见 `docs/spikes/linux-sandbox.md`）。
4. **本切片在 owner 的 Mac 上装了 rustup target `aarch64-unknown-linux-musl`**（用于交叉编译 VM 上跑的驱动）。这是一次开发环境改动，不影响仓库，但值得知道。
5. **`git add`/`git commit` 在 `workspace-write` 下失败是设计结果**（`.git` 只读覆盖，ADR 0012），不是兼容性缺陷。含义：在该档位下 agent 不能自己提交；提交是宿主或用户在沙箱外做的事——这与计划 Phase 6c "apply/merge/push 保持独立审批步骤" 一致。
