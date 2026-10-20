# 16. 跨平台（重点是 Windows）

2026-10-18 · Radian 代码版本 hisea/radian@356df1e · 对手版本：Codex `8c07694`、opencode `63c1a27`、pi-mono `482c138`、omp 18.12.1、Claude Code 2.1.302（npm 包）。网页抓取日期都是 2026-10-18。

## 1. 一句话结论

Radian 现在只能在 macOS 和 Linux 上从源码构建，没有 CI，也没有发布产物。五个对手都发布原生 Windows 版本（Codex、Claude Code、opencode、pi 还发 arm64，Codex、Claude Code、opencode 还发 musl）。Radian 的 Windows 改造量比看上去小：agent-core 和三个 provider 没有一处平台代码；大部分 Unix 调用已经包在 `cfg(unix)` 里，并且有一个空的 `cfg(not(unix))` 兜底；ADR 0043 又把缺省档定成了不开沙箱的 `full`。真正要做的是四件事：shell 的选择、用 Job Object 杀进程树、`safe_fs` 的符号链接防护、补一套 Windows 测试。**建议的顺序**：现在只做第 0 步（把 WSL2 写进文档并实测）和第 1 步（建 CI，先让 Windows 交叉编译过），原生 Windows（第 2 步）等到有发布渠道、有外部用户再做，原生 Windows 沙箱（第 3 步）不做，Claude Code 也没做。

## 2. 对比表

| 维度 | Radian | pi | Codex CLI | Claude Code（文档） | omp | opencode |
|---|---|---|---|---|---|---|
| 发布的平台 | 没有发布，只能从源码 `cargo build`（`README.md:66-80`） | Windows x64/arm64 的 zip，加上 npm 包（`.github/workflows/build-binaries.yml:98-99`） | macOS x64/arm64；Linux x64/arm64，gnu 和 musl 都有；Windows x64/arm64 msvc（`rust-release.yml:96-133`、`rust-release-windows.yml:40-75`） | darwin-x64/arm64、linux-x64/arm64、linux-*-musl、win32-x64/arm64（`package.json:23-32`；[setup](https://code.claude.com/docs/en/setup)） | Bun 包（`engines.bun>=1.3.14`，`package.json:578`）；平台相关的部分在 `@oh-my-pi/pi-natives` 里（未核实它有哪些平台） | 12 个目标：linux x64/arm64（含 musl、无 avx2）、darwin、win32 x64/arm64（`packages/cli/script/build.ts:36-54`） |
| 原生 Windows | 不支持；`radian-host` 很可能编译不过（`herdr.rs:32` 无条件 `use std::os::unix::net::UnixStream`，推断） | 支持（`docs/windows.md`） | 支持，推荐原生（[Windows sandbox](https://learn.chatgpt.com/docs/windows/windows-sandbox)）；仓库里的 `docs/install.md:7` 还写着 “Windows 11 via WSL2”，已经过时 | 支持，Windows 10 1809+（[setup](https://code.claude.com/docs/en/setup)） | 支持（CHANGELOG 里 41 条 Windows 修复，`CHANGELOG.md:42-46` 等） | 支持（CI 跑 Windows 单测和 e2e） |
| WSL | 没有文档，也没测过 | 写进了文档（`docs/windows.md:9-13`） | 文档列为备选 | WSL2 支持沙箱，WSL1 不支持（[setup](https://code.claude.com/docs/en/setup)） | 未核实 | 未核实 |
| Windows 上的沙箱 | 没有后端：`platform_backend()` 只认 macOS 和 Linux（`radian-sandbox/src/lib.rs:115-125`）；`workspace`/`safe` 会报 `UnsupportedPlatform`，`full` 不探测后端（`lib.rs:237-246`） | 没有沙箱 | 三套后端：MXC（Win11 24H2+，不需要管理员）、elevated（专用沙箱用户 + 防火墙 + ACL，需要管理员装一次）、unelevated（受限 token + ACL）；`windows-sandbox-rs` + `windows-sandbox-service` 共约 3.65 万行 | **原生 Windows 不支持沙箱**，要用就去 WSL2（[setup](https://code.claude.com/docs/en/setup)） | 没有 OS 沙箱 | 没有 OS 沙箱 |
| shell | 写死 `/bin/sh -c`（`radian-coding/src/shell.rs:66`、`:420`） | 顺序：`shellPath` → Git Bash → PATH 上的 `bash.exe`；可选 `powershell` 工具（`utils/shell.ts:64-135`） | Windows 上先找 PowerShell（`shell_detect.rs:363-366`），找不到退回 `cmd.exe`（`:328-333`）；有 PowerShell 解析器和危险命令表（`command_safety/powershell_parser.rs`、`windows_dangerous_commands.rs`） | 有 Git Bash 就用 Bash 工具，没有就用 PowerShell 工具；`pwsh` 优先，退回 5.1，带 `-ExecutionPolicy Bypass`（[tools-reference](https://code.claude.com/docs/en/tools-reference#powershell-tool)） | 内嵌 brush-core（Rust 写的 bash），不依赖系统 shell（`src/exec/bash-executor.ts:4`） | `pwsh` → `powershell` → Git Bash → `%COMSPEC%`（`packages/core/src/shell/select.ts:95-107`） |
| 杀进程树 | Unix：`setsid` + `killpg` + `waitid(WNOWAIT)`；非 Unix：shell 和 job **什么都不做**（`shell.rs:985-1040`），MCP 用 `taskkill /T`（`agent-extensions/src/process.rs:226-236`） | `taskkill.exe /F /T`，用 System32 的绝对路径（`utils/shell.ts:187-203`） | Job Object + `KILL_ON_JOB_CLOSE`（`utils/pty/src/win/job.rs:53`、`:65`） | 未公开（闭源） | 未核实；CHANGELOG 修过 “取消时误杀无关程序”（`CHANGELOG.md:868`） | `taskkill /T /F`（`packages/util/src/cross-spawn-spawner.ts:351`） |
| 终端 / pty | crossterm 和 ratatui；pty 测试用 portable-pty（`radian-tui/Cargo.toml:83`），只在 macOS 上手测过（`radian-tui/README.md:1209`） | 改过 Windows 和 WSL 的缺省按键（CHANGELOG #8372） | 自带 ConPTY 封装（`utils/pty/src/win/conpty.rs`） | 未公开 | 修过 Windows 上 PTY 的光标查询挂起（`CHANGELOG.md:920`） | node-pty；Windows 上不用自家的 pty 资产（`packages/cli/src/node/target.ts`） |
| CI 测不测 Windows | **完全没有 CI**（仓库里没有 `.github/`） | CI 只跑 Ubuntu（`ci.yml:15`、`:48`）；二进制在 windows-latest 上做冒烟测试（`build-binaries.yml:132-140`） | 在 windows-x64 和 windows-arm64 上跑 nextest 全量测试（`rust-ci-full.yml:468-491`） | 未公开 | 未核实 | Windows 2025 上跑单测、构建和 e2e（`.github/workflows/test.yml:64-66`、`:149`、`:217`） |

## 3. 各家的关键做法

**Codex：投入最大，原生 Windows 加沙箱一起做**
- 代码量：`codex-rs` 里有 798 处 `cfg(windows)` 或 `cfg(not(windows))`，分布在 247 个文件里（用 `git grep -c` 统计）。Windows 沙箱的两个 crate 共 36,538 行 `.rs`（`windows-sandbox-rs` 30,373 行，`windows-sandbox-service` 6,165 行）。
- 三档沙箱写在 `WindowsSandboxLevel { Disabled, RestrictedToken, Elevated }` 里（`protocol/src/config_types.rs:297-302`）。另有一个 MXC 后端，直接调 Microsoft 的 `BaseContainerRunner`，“不改宿主 ACL，不建沙箱用户，不需要提权”（`mxc-sandbox/README.md:1-8`）。官方文档把 MXC 列为推荐，elevated 是首选的旧后端，unelevated 是最后的退路（[文档](https://learn.chatgpt.com/docs/windows/windows-sandbox)）。
- 文档列出的已知限制：unelevated 的网络隔离更弱，也不支持禁读路径；MXC 在前台命令退出后会结束所有子进程，所以脱离前台的 dev server 可能跑不起来；企业设备可能挡住安装步骤（错误 1385）。也就是说，即使投入了 3.6 万行，原生沙箱的覆盖面还是不完整。
- 进程控制：用 Job Object 并设置 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，子进程以挂起状态创建，先加入 job 再恢复运行（`utils/pty/src/win/job.rs:17-65`）。
- shell：Windows 上选 PowerShell，并且排除 WindowsApps 里的 Store 版 PowerShell，因为沙箱账户访问不了它（`shell_detect.rs` 中 `is_inaccessible_windows_apps_powershell_path`）。

**Claude Code：原生 Windows 支持完整，但沙箱只放在 WSL2**
- 原生 Windows 不需要管理员权限；Git for Windows 是可选的，没有它时 shell 工具换成 PowerShell 工具（[setup](https://code.claude.com/docs/en/setup)）。
- 原生 Windows 上的沙箱是 “Not supported”，要沙箱就用 WSL2（同上，表格）。这是对 Radian 最有参考价值的一条：**一个大厂产品也选择了“原生 Windows 不开沙箱”**。
- PowerShell 工具的细节都是踩坑之后补上的：v2.1.214 之前，PowerShell 5.1 的 `>` 会写出 UTF-16LE 文件，非 ASCII 的管道输入会变成 `?`；禁止 Bash 时会顺带关掉 PowerShell，否则规则能被绕过（[tools-reference](https://code.claude.com/docs/en/tools-reference#powershell-tool)）。
- 在 Windows 上，Glob 和 Grep 是缺省工具；在 macOS、Linux 和 WSL 上不是，在那里通过 Bash 里内嵌的 `bfs` 和 `ugrep` 来搜（同上）。Radian 的 `grep` 和 `glob` 本来就是内置工具，这一点不用改。

**pi 和 opencode：Node 系，不做沙箱，选 shell 的逻辑短小清楚**
- pi 依次找 `shellPath`、Git Bash、PATH 上的 `bash.exe`，都找不到就列出找过的路径，并提示安装 Git for Windows（`utils/shell.ts:64-105`）。PowerShell 是可选工具，带 `-NoProfile -NonInteractive -ExecutionPolicy Bypass`（`:122-135`）。`!` 命令始终走 Bash（`docs/windows.md`）。
- opencode 按 `pwsh`、`powershell`、Git Bash、`%COMSPEC%` 的顺序找（`select.ts:95-107`），并且用 tree-sitter-powershell 解析 PowerShell 命令，给权限判断用（`target.ts` 中的 `shellParserWasmAssets`）。
- 这两家杀进程树都靠 `taskkill /T /F`。它沿着父子关系往下走，碰到 leader 已经退出的孤儿进程就够不着了。Radian 的设计文档也承认这一点（`docs/minimal-headless-rust-agent-runtime-design.md:296`）。

**omp：内嵌 shell，绕开 Windows 的 shell 碎片化**
- 用 brush-core 原生绑定直接执行 bash 语法（`src/exec/bash-executor.ts:4`），Windows 上也不读用户的 `$SHELL`（`:424`）。代价是要自己实现 coreutils：CHANGELOG 里有 `du`、`rmdir dir/.`、`dd` 在 Windows 上的行为修复（`CHANGELOG.md:42-44`）。
- 长尾 bug 的规模：omp 从 18.2.10（2026-09-22）到现在约 51 个版本，CHANGELOG 里提到 Windows 41 次；pi 的 coding-agent CHANGELOG 从 0.10.0（2025-11-25）到现在提到 Windows 71 次。修的内容有锁文件 EPERM、Hyper-V 保留端口挡住 OAuth 回调、stdout 管道反压导致死锁、`core.autocrlf` 让补丁打不上、反斜杠被吃掉等（pi `CHANGELOG.md` 中 #10801、#10571；omp `:1046-1049`）。**原生 Windows 的主要成本不在首次移植，而在后面持续修 bug。**

## 4. Radian 现状

### 4.1 Unix 专用代码的数量和分布

在 `crates/*/src` 和 `crates/*/tests` 里用 `grep -rnE` 统计：

| 模式 | src | tests |
|---|---|---|
| `cfg(unix)` / `cfg(not(unix))` | 133 | 63 |
| `cfg(target_os …)` | 19 | 18 |
| `cfg(windows)` | **1**（`agent-extensions/src/process.rs:226`） | 0 |
| `libc::` | 125 | 74 |
| `std::os::unix` | 63 | 77 |
| `setsid` / `killpg` / `waitid` / `pre_exec` | 22 / 9 / 14 / 4 | 13 / 4 / 0 / 0 |
| `posix_openpt` / `openpty` | 7 | 26 |
| `UnixStream` / `UnixListener` | 15 | 4 |
| `/bin/sh` 字面量 | 16 | 16 |

按 crate 统计 src 里的平台相关行：radian-coding 107，radian-host 86，radian-sandbox 38，radian-tui 37，agent-cli 26，radian-session 23，radian-memory 14，agent-extensions 10。**agent-core、radian-runtime、radian-permissions 和三个 provider 都是 0。** 183 个测试文件里，有 9 个整个文件标了 `#![cfg(unix)]` 或 `#![cfg(target_os)]`。其余的很多直接调用 `sh` 或 `printf`，在 Windows 上大概率失败（推断，没在 Windows 上跑过）。

### 4.2 按功能分

| 功能 | 现在的 Unix 实现 | 非 Unix 时的行为 | 出处 |
|---|---|---|---|
| shell 工具和 `!`/`!!` | `/bin/sh -c`，`setsid`，`killpg`，`waitid(WNOWAIT)` | 拿不到 shell；`signal_group`、`own_session`、`stop_leader` 都是空操作，**超时或取消时子进程不会被杀** | `shell.rs:420`、`:780-800`、`:985-1040` |
| 后台 job（ADR 0037） | 同上，加 0700 目录和 `ExitStatusExt::signal` | 有兜底分支，但进程树清理同样是空操作 | `jobs.rs:88-94`、`:763-791`、`:969-975` |
| subagent（ADR 0043） | 先对子进程 `stop_leader` 发 SIGTERM，再 `kill_session` | 空操作，子 radian 会变成孤儿 | `radian-host/src/subagent.rs:421-422` |
| MCP 进程 | `killpg` 加 `waitid` | `taskkill /T /F`，够不着已退出 leader 留下的子进程 | `agent-extensions/src/process.rs:216-236`、`README.md:66-70` |
| 文件工具的符号链接防护（ADR 0039） | 用 `openat(O_NOFOLLOW)` 逐级打开 | 退回按路径打开，**没有符号链接和 junction 防护** | `radian-coding/src/safe_fs.rs:550-553`、`README.md:198-200` |
| 会话和记忆的单写者锁 | `fs2` 的 `flock` | fs2 在 Windows 上用 `LockFileEx`，这是强制锁，可能挡住其他进程读 journal（推断，未核实） | `radian-session/src/writer.rs:444-475` |
| 私有文件权限 | 0600/0700，并检查权限是否过宽 | 不检查，依赖用户目录的缺省 ACL | `radian-host/src/auth.rs:494-511`、`radian-session/src/fs.rs:41-45` |
| `~/.radian` | 只认 `$HOME`（ADR 0020） | 原生 Windows 通常没有 `HOME`，于是 `radian_home()` 返回 `None` | `radian-host/src/home.rs:4-5`、`:29-40` |
| 控制 socket | Unix socket | 返回 “the control port is Unix-only” | `radian-host/src/control.rs:105-115`、`:203` |
| herdr 上报（ADR 0034） | `UnixStream` | **没有 cfg 保护**，在 Windows 上应该编译不过（推断） | `radian-host/src/herdr.rs:32` |
| TUI 信号和 tty | `tokio::signal::unix`，用 `poll(2)` 检测终端是否已关闭 | 都有空兜底；Ctrl-C 和窗口大小变化交给 crossterm | `radian-tui/src/signals.rs:62-115`、`cli/tty.rs:246`、`:326` |
| 沙箱 | seatbelt 和 bwrap | `UnsupportedPlatform`，在 `full` 档不触发 | `radian-sandbox/src/lib.rs:115-125`、`:237-246` |
| 换行 | `patch` 按 LF 视图匹配 CRLF 文件，写回时保持 CRLF | 与平台无关，**已经可用** | `radian-coding/src/patch.rs:61-62`、`:240-242`、`:472-475` |

### 4.3 文档里已经承认的事实

- 设计文档原计划在 Windows 上用 Job Object，以 `TerminateJobObject` 作为 drop 时的兜底；现在只有 `taskkill`，并注明“尚未在 Windows 上测试”（`docs/minimal-headless-rust-agent-runtime-design.md:256`、`:296`、`:345`）。
- ADR 0012 把 Windows 排在 “Phase 7 Job Object/CI”（`docs/adr/0012-permission-sources.md:58`）。
- AGENTS.md 规定：不要把本机测不了的代码交给 pi，例如 Windows 进程控制（`AGENTS.md:52`）。
- crate 的 README 里各自写了 “Not verified here: Windows”（`agent-core/README.md:140`、`radian-host/README.md:348`、`radian-tui/README.md:1209`）。

## 5. Radian 的优势

1. **核心层与平台无关**：agent-core、runtime、permissions 和三个 provider 没有一行平台代码。原生 Windows 的改动集中在 coding、host、tui、session 四个外围 crate，不碰 journal 语义（ADR 0001、0006、0040）。
2. **兜底分支大多已经写好**：src 里有 133 处 `cfg(unix)` 守卫，大多有对应的 `not(unix)` 分支，所以主要工作是“把空操作补成实现”，而不是重构。
3. **缺省是 `full`（ADR 0043 决定 1）**：原生 Windows 不开沙箱，在体验上和 macOS、Linux 的缺省一样。`workspace` 档在 Windows 上明确报错，不会悄悄降级（`docs/sandbox.md` 的硬规则），这与 Claude Code 的做法一致。
4. **单个静态 Rust 二进制**：Codex 已经证明 Rust 工作区可以同时发 msvc、musl 和 arm64。Radian 的依赖（crossterm、ratatui、tokio、reqwest+rustls、fs2、portable-pty）都支持 Windows。不需要像 pi、opencode 那样处理 Node、Bun 和原生 `.node` 资产，也不需要像 omp 那样维护一份内嵌 coreutils。
5. **CRLF 已经处理**，`grep` 和 `glob` 是内置工具，不依赖系统的 `find`/`grep`，Windows 上少了一类问题（对照 Claude Code 在 Windows 上把 Glob/Grep 设为缺省工具）。

## 6. Radian 的劣势和缺口

1. **没有 CI，没有发布**：这是比 Windows 更基本的缺口。六家里只有 Radian 既没有预编译二进制也没有 CI；连 Linux musl 和 arm64 都没验证过，Alpine 用户和容器里的 CI 用不了。
2. **从没在 Windows 上编译过**：`herdr.rs:32` 至少会挡住 `radian-host` 的编译（推断）。本机只装了 `x86_64-unknown-linux-gnu` 目标（`rustup target list --installed`），这一轮没法交叉检查，可能还有别的编译错误。
3. **Windows 上不杀子进程**：shell、job 和 subagent 的取消与超时都是空操作。这不只是体验问题：超时的 `cargo test` 会一直占着 CPU 和文件锁，后续写文件可能失败。
4. **shell 写死 `/bin/sh`**，工具描述里也写着 `/bin/sh -c`（`shell.rs:66`、`:157`），在 Windows 上要改成运行时决定。权限层的 shell 匹配器是按 POSIX 做的保守近似（`radian-permissions/README.md:49`），PowerShell 语法它不认识。
5. **ADR 0039 的防护在 Windows 上不成立**：没有符号链接和 junction 防护，只能写进文档，或者用 `FILE_FLAG_OPEN_REPARSE_POINT` 加句柄相对的打开重新实现（未核实可行性）。
6. **`HOME` 假设**：原生 Windows 上 `~/.radian` 解析不出来，ADR 0020 写的是“没有平台分支”。
7. **WSL 没有文档**：Stack Overflow 2025 调查中，16.8% 的开发者在工作中用 WSL，49.5% 用 Windows，32.9% 用 macOS（[survey](https://survey.stackoverflow.co/2025/technology#1-computer-operating-systems)）。WSL2 是 Linux 内核，Radian 的 bwrap 后端理论上能用；Claude Code 文档写明 WSL2 支持沙箱（[setup](https://code.claude.com/docs/en/setup)），但 Radian 自己没测过（推断）。

## 7. 改进方案（按对 Radian 整体价值的收益/成本排序）

### 第 0 步：把 WSL2 写进文档并实测（约 1 人日）
- **做什么**：在 README 加一节 “Platforms”：macOS 和 Linux 是一级支持，WSL2 按 Linux 对待，原生 Windows 不支持。在 WSL2（Ubuntu 24.04）上跑一遍 Verify 和 `escape_linux`、`compat_linux` 矩阵，结果写进 `docs/sandbox.md`。重点看三件事：bwrap 的 user namespace 能不能用、`/mnt/c` 下的仓库性能（9P 很慢，推断）、herdr 能否运行。
- **收益**：Windows 用户（约占开发者一半）立刻就有一条明确能走的路，而且在 WSL2 里沙箱照样可用，这一点和 Claude Code 一样。可信度也会提高，因为文档说清楚了支持什么、不支持什么。
- **风险**：几乎没有。如果 bwrap 在 WSL2 上探测失败，按现有规则会报错，不会降级。
- **ADR**：不推翻任何 ADR，只更新 `docs/sandbox.md`。
- **验证**：在 WSL2 上，Verify 的 7 条命令和两份沙箱矩阵都通过。

### 第 1 步：建 CI，先让 Windows 编译通过（约 3–5 人日）
- **做什么**：
  1. 加 GitHub Actions：ubuntu 和 macos 跑完整的 Verify；另加 `x86_64-unknown-linux-musl` 和 `aarch64-unknown-linux-gnu` 的构建。
  2. 在 `windows-latest` 上跑 `cargo check --workspace --all-targets` 和 `cargo test -p agent-core -p agent-provider-*`，这几个 crate 与平台无关，应该能直接通过。
  3. 修编译错误：`herdr` 模块加 `cfg(unix)`（或者给它一个空实现），以及 CI 报出来的其他问题。
  4. 顺手发 release 产物（macOS arm64/x64，Linux gnu/musl × x64/arm64）。
- **收益**：这是长期可维护性收益最大的一步。今后一个 PR 往无条件的位置加了 `std::os::unix`，CI 会立刻报出来，平台债不会再悄悄增加。有预编译二进制，用户不装 Rust 也能用，这直接影响用户量。musl 版还能覆盖 Alpine 和容器里的 CI（Claude Code、Codex、opencode 都发 musl）。
- **风险**：macOS 的 seatbelt 矩阵在 GitHub runner 上可能有权限差异（推断）。在 CI 里跑沙箱测试，需要处理嵌套沙箱的情况（ADR 0016）。
- **ADR**：不推翻任何 ADR；ADR 0012 里的 “Phase 7 CI” 可以提前。AGENTS.md 的 Verify 一节加一句 “CI 在 Windows 上做编译检查”。
- **验证**：故意在一个无条件的位置加 `use std::os::unix::net::UnixStream`，Windows job 必须失败。

### 第 2 步：原生 Windows，只支持 `full` 档（约 4–6 人周，之后还要长期维护）
按模块列出要改的地方：

| 模块 | 要改什么 | 估计 | 碰到的 ADR |
|---|---|---|---|
| `radian-coding/shell.rs` | 运行时选 shell：配置项 → Git Bash（`Program Files\Git\bin\bash.exe`）→ PATH 上的 `bash.exe` → `pwsh` → `powershell.exe`；工具描述按选中的 shell 生成；先只做 Bash 语义，PowerShell 作为第二个工具以后再加 | 3–4 人日 | 0033（`!` 用哪个 shell）、0036（工具描述在系统提示词里，每个进程只组装一次，不受影响） |
| 进程控制（shell、job、subagent、MCP） | 新增 `ProcessTree` 抽象：Unix 照旧用 setsid、killpg、waitid；Windows 用 Job Object + `KILL_ON_JOB_CLOSE`，`CREATE_SUSPENDED` 后先加入 job 再恢复运行（照 Codex 的 `job.rs`）；优雅停止用 `CTRL_BREAK_EVENT`（需要 `CREATE_NEW_PROCESS_GROUP`，未核实子 radian 能不能收到）；MCP 不再用 `taskkill` | 6–8 人日 | 0037、0043 决定 1a、0016（`setsid` 用来防 TIOCSTI，Windows 没有这种攻击面，要写明） |
| `radian-coding/safe_fs.rs` | 二选一：在 Windows 上实现拒绝 reparse point 的打开（`FILE_FLAG_OPEN_REPARSE_POINT` 加检查属性，未核实是否足够），或者在文档和启动行写明“Windows 上没有链接防护” | 3–5 人日或 0.5 人日 | 0039（防护变弱需要新 ADR 或修订） |
| `radian-host/home.rs` | 没有 `HOME` 时退回 `USERPROFILE`（或者 `dirs::home_dir`） | 0.5 人日 | 0020（要加一条平台分支，需要修订） |
| `radian-session`、`radian-memory` | 核实 `LockFileEx` 会不会挡住 `radian list`/`tail` 读文件；核实替换正被打开的文件时 rename 是否失败（`atomic_write.rs`、`theme_write.rs`）；fsync 换成 `FlushFileBuffers` 后组提交的耗时（ADR 0040 的 17 ms 基线可能变化，推断） | 3–5 人日 | 0001、0040（语义不能变，只是实现） |
| 权限层 | shell 匹配器只对 Bash 生效；PowerShell 的命令一律 `ask`，直到有解析器为止（opencode 用 tree-sitter-powershell，Codex 自己写解析器） | 1–2 人日 | 0012（只是在现有规则里加一类“永远问”，不推翻） |
| `radian-host` 的 herdr 和控制 socket | Windows 上关掉，并在启动行说明；以后可以用 named pipe（omp 就这么做，`CHANGELOG.md:398`） | 0.5 人日 | 0034（herdr 在 Windows 上能不能运行，未核实） |
| `radian-tui` | crossterm 已经支持 Windows；要核实的是 Windows Terminal、VS Code 终端和 conhost 下的按键（pi 为 Windows 改过缺省按键）、粘贴、alt screen、resize；pty 测试改用 portable-pty 的 ConPTY 后端 | 4–6 人日 | 0009、0023、0027、0035（视觉和按键可能要按平台区别） |
| 路径 | 工具输出和提示词里的 `\`；权限规则里路径的匹配；`@` 提及和 glob 的分隔符；盘符和 UNC 路径 | 2–3 人日 | 0039、0012 |
| 测试 | 把依赖 `/bin/sh` 的测试分开，给 Windows 建一个能通过的子集，在 CI 上跑；把手测清单写进 `radian-tui/README.md` | 5–8 人日 | — |
| 发布 | msvc 的 x64/arm64 zip，加 Authenticode 签名（Claude Code 和 Codex 都签名） | 2 人日加证书成本 | — |

- **用户收益**：不想用 WSL 的 Windows 用户（例如做 .NET、游戏、Windows 原生项目的人）也能用 Radian。和 pi 对齐：pi 原生支持 Windows（`docs/windows.md`），从 pi 迁移过来的 Windows 用户目前没法换到 Radian（用户占比未核实）。
- **风险**：
  1. **owner 和 pi 在本机都测不了**（AGENTS.md:52），每一轮修复都依赖 CI 或远程机器，迭代会很慢。
  2. 长尾 bug：pi 和 omp 的 CHANGELOG 合计 112 条 Windows 条目，说明修 bug 会一直持续，而且不会停。
  3. 安全承诺变弱：没有沙箱，ADR 0039 的防护也不完整，在陌生仓库里使用的风险更高。必须在启动行明确写出来。
  4. 进程控制最难测。设计文档已经说明这类竞态没法确定性复现（`design.md:296`）。
- **验证**：Windows CI 上的 `timeout_kills_and_reaps_process_tree` 改用 Job Object 检查成员（`design.md:416`）；变异检查：去掉 `KILL_ON_JOB_CLOSE`，测试必须失败。

### 第 3 步：原生 Windows 沙箱（不建议做）
- 可选路线有三条：一是 MXC，只支持 Windows 11 24H2 build 26100.9278 及以后，Microsoft 还在陆续放出相关能力；二是 restricted token 加 ACL，网络隔离弱，也不支持禁读；三是 elevated，需要管理员，要建用户、改防火墙。Codex 为这些写了约 3.65 万行，文档里还列着好几条已知限制（第 3 节）。
- 理由：ADR 0043 已经决定 “不再为沙箱加能力”（决定 2）；Claude Code 在原生 Windows 上也不提供沙箱，只引导到 WSL2。需要沙箱的 Windows 用户，用第 0 步的 WSL2 路线就够了。
- 如果以后要做，最省力的是只接 MXC（不需要管理员，Codex 的 crate 是 Apache-2.0，可以参考），而且必须探测失败就拒绝（ADR 0012 决定 2）。要写新 ADR。

## 8. 是否值得现在做

- **第 0 步：值得，马上做。** 成本约 1 天，就能让一半开发者有一条走得通、还带沙箱的路，也不碰任何 ADR。
- **第 1 步：值得，尽快做。** 理由主要不是 Windows，而是 Radian 现在没有 CI，也没有发布产物，这直接限制了用户量和可信度。Windows 编译检查只是顺带加一个 job，作用是防止平台债继续增加。
- **第 2 步：现在不值得。** 前提条件都还不满足：没有发布渠道，也没有外部用户（ADR 0043 是按单人 dogfood 的现实写的），owner 和 pi 在本机都测不了 Windows，第 2 步的 4–6 人周会挤掉评测、免审批沙箱这类收益已经算清楚的工作（见总报告第 0–2 批）。建议的触发条件：第 1 步的发布产物有了外部下载，并且出现了 Windows 原生需求的 issue。到时候先做 shell 选择和 Job Object 这两项，它们占了第 2 步大部分的用户价值，`safe_fs` 和 TUI 的细节放到后面。
- **第 3 步：不做。** 除非将来有 ADR 推翻 0043，把沙箱重新设为缺省。
