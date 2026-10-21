# 15. 上手体验与分发

2026-10-18 · Radian hisea/radian@356df1e · 对手版本：pi 1.3.0（npm 包）、omp 18.12.1、Codex `8c07694`（HEAD，2026-10-18）、opencode `63c1a27`（2026-10-17）、Claude Code（只看官方文档，抓取于 2026-10-18）

## 1. 一句话结论

Radian 的骨架已经适合分发：一个只依赖 glibc 的 Rust 二进制，ChatGPT 订阅登录（浏览器和设备码两种）也已经实现。但**一个新用户今天根本装不上，也很难第一次就跑起来**：仓库是私有的，没有发布产物，只能从源码编译；第一次启动按正常顺序走会**连续两次报错退出**；报错里让人去 TUI 里 `/login`，而 TUI 根本打不开。五个对手都是“一条 curl 命令 → 启动 → 在界面里登录 → 开干”，4–5 步。Radian 现在要 7–8 步，其中两步是报错。改进的顺序是：先把 ADR 0008 已经定好的“首次启动”做完，同时补上“没有凭据也进 TUI”这一处；再修用户看得到的文字；然后出发布产物和安装脚本，加 `radian doctor`。这几项都便宜，带来的是能用的人数和可信度，不是速度。

## 2. 对比表

| 维度 | Radian | pi | Codex CLI | Claude Code | omp | opencode |
|---|---|---|---|---|---|---|
| 安装方式 | **只能从源码编译**：`cargo build --release`（README.md「Build」）；没有 CI、没有 release、没有安装脚本（仓库根目录只有 `scripts/measure-tool-concurrency.py`）；仓库私有（`gh api repos/hisea/radian` → `private: true`，0 star） | curl 脚本、PowerShell、npm、Nix（`pi/README.md:25-45`） | curl 脚本、PowerShell、npm、Homebrew cask、GitHub Release 二进制（`codex/README.md:14-55`），另有 winget 打包脚本（`scripts/build_winget_package.py`） | curl 原生安装（推荐）、Homebrew（stable/latest 两个 cask）、WinGet、签名的 apt/dnf/apk 源、npm（[setup](https://code.claude.com/docs/en/setup)） | bun/npm、Homebrew、mise、独立二进制（`omp/src/cli/update-cli.ts:166-182`） | curl、npm/bun/pnpm/yarn、scoop、choco、brew（自有 tap 和官方 formula）、AUR、mise、nix（`opencode/README.md:46-61`） |
| 运行时依赖 | 无（`ldd` 只有 libc/libm/libgcc_s，实测）；`workspace` 档需要系统里有 bwrap 或 sandbox-exec | Node ≥ 22.19（安装脚本能代装，`quickstart.md:15`） | 无（原生二进制） | 无（npm 包也只是拉原生二进制，[setup](https://code.claude.com/docs/en/setup)） | bun ≥ 1.3.14（`omp/package.json:578-580`），或用独立二进制 | 无（安装脚本装二进制；npm 包，未核实细节） |
| 体积 | release 二进制 **40.1 MiB**，strip 后 30.8 MiB，gzip 后 12.5 MiB；加 `lto=fat`+`codegen-units=1`+`strip` 后 **24.6 MiB**，gzip **10.7 MiB**（本机 x86_64 Linux 实测，用环境变量覆盖 profile，未改仓库） | npm 包解压 26 MB，另加依赖和 Node（本机 `du`） | `codex-aarch64-apple-darwin.tar.gz` 94.9 MB；Linux `codex-package-*.tar.zst` 110 MB（[rust-v0.163.0 资源列表](https://github.com/openai/codex/releases/expanded_assets/rust-v0.163.0)，2026-10-18 抓取，含 app-server 等组件） | 未核实 | npm 包解压 56 MB，另加 bun（本机 `du`） | 未核实 |
| 首次启动 | 没有向导。无模型：打印 `no model: pass --model…` 退出码 2；给了模型没凭据：退出码 2（实测，见 §3.1）。项目信任弹窗只在目录里有项目资源时出现（ADR 0031 T3） | 直接进界面，没有模型时提示 `/login` 再 `/model`（`dist/core/auth-guidance.js:4-16`）；首次按终端背景选主题（CHANGELOG 1711） | 引导屏：Welcome → Auth → TrustDirectory（`codex-rs/tui/src/onboarding/onboarding_screen.rs:70-78`） | 首次使用提示登录，走浏览器；设置了 `ANTHROPIC_API_KEY` 时问一次是否用它（[quickstart](https://code.claude.com/docs/en/quickstart)） | 首次启动或向导版本升级时跑 setup 向导，包括 provider 设置（`omp/src/main.ts:669-697`，`src/modes/setup.ts:1`） | 进界面后 `/connect` 选 provider 填 key，`/models` 选模型，可选 `/init` 生成 AGENTS.md（[docs](https://opencode.ai/docs/)） |
| 认证方式 | `auth.json` 里的 API key（`radian login`，存前发一次 `GET /models` 验证，`radian-host/src/login.rs:1-10`）；ChatGPT 订阅 OAuth（浏览器 PKCE + 设备码，`agent-cli/src/login.rs:219-289`，ADR 0026）；**不读环境变量里的 key**（ADR 0026 D2，`agent-cli/src/setup.rs:220-241`） | 订阅 OAuth（Anthropic、ChatGPT、Codex、OpenRouter）、API key、环境变量、云凭据（ADR 0026 背景，`quickstart.md:62-64`） | ChatGPT 登录（浏览器 + 设备码）、API key（`onboarding/auth.rs:87-99,355-410`）；`codex login --with-api-key` 从 stdin 读（`codex-rs/cli/src/login.rs:283`） | Pro/Max/Team/Enterprise 订阅、Console、Bedrock/Vertex/Foundry、企业网关（[quickstart](https://code.claude.com/docs/en/quickstart)） | 多家 OAuth 与 key（ADR 0026 引用了它的 openai-codex OAuth） | `/connect` 存 key 或浏览器 OAuth；`auth.json` 在 `~/.local/share/opencode/`（[providers](https://opencode.ai/docs/providers)）；`auth list/connect/logout/export/import/switch`（`cli/src/commands/commands.ts:145-207`） |
| 自动更新 | 无 | 启动时查 `pi.dev` 最新版并提示 `pi update`，`PI_SKIP_VERSION_CHECK` 可关（`dist/utils/version-check.js:65-77`）；安装/更新遥测默认开（`docs/settings.md:170`） | 启动时查新版本，按安装方式查 Homebrew cask API 或 GitHub latest release（`codex-rs/tui/src/updates.rs:28,60-62`）；`codex update`（`cli/src/main.rs` Subcommand `Update`） | 原生安装后台自动更新，latest/stable 两个通道，`minimumVersion`，`DISABLE_AUTOUPDATER`（[setup](https://code.claude.com/docs/en/setup)） | `omp update`，大版本时改装独立二进制并校验大小与 SHA-256（`update-cli.ts:166-195`） | `upgrade`（别名 `update`），8 种安装方式（`cli/src/services/updater.ts:14`）；配置 `update = auto/notify/disable`（`core/src/config/normalize.ts:73-95`） |
| 诊断命令 | 无 `doctor`；`diagnostics.rs` 只管 panic 日志的目录与清理（`radian-host/src/diagnostics.rs:1-11`）；启动时打印 `[config] <路径>` | 无 doctor（docs 中检索不到） | `codex doctor`：安装、配置、认证、终端、路径、沙箱、网络、更新、git，结果脱敏，可 `--json`，只读（`codex-rs/cli/src/doctor.rs:1-12`，`doctor/` 下 20 多个模块） | `claude doctor`：只读，安装健康、设置文件校验、自动更新结果（[setup](https://code.claude.com/docs/en/setup)） | 只有 `plugin doctor`（`src/cli/plugin-cli.ts`） | `debug config`（配置来源）、`debug paths`（各目录）（`commands.ts:116-135`） |
| 卸载与数据位置 | 无卸载说明；数据全在 `~/.radian/`（ADR 0020），目录 0700、文件 0600 | 文档写了三种卸载方式，并说明数据留在 `~/.pi/agent/`（`quickstart.md:116-136`） | 未核实 | 每种安装方式的卸载命令，以及 `~/.claude`、`~/.claude.json`、项目里 `.claude`/`.mcp.json` 的清理（[setup](https://code.claude.com/docs/en/setup)） | 未核实 | `uninstall` 命令，`--dry-run`，默认保留会话、配置和状态（`commands.ts:76-88`） |
| 文档站与教程 | 无站点；README 面向开发者，讲 crate 和 ADR；没有 quickstart | `pi.dev` + 仓库 `docs/`（42 页，含 quickstart、Windows、Termux） | developers.openai.com / learn.chatgpt.com 的 CLI 页（[cli](https://learn.chatgpt.com/docs/codex/cli)） | code.claude.com：quickstart 7 步、终端入门、安装排障、Academy 课程（[quickstart](https://code.claude.com/docs/en/quickstart)） | 仓库 README 与 docs | opencode.ai/docs：intro 4 步、20 多种语言的 README（仓库根目录 `README.*.md`） |
| 默认配置 | 不存在配置文件时什么都不建（README「Configuration file」；ADR 0008 2026-10-17 补记决定改成首次创建注释模板，**未实现**）；默认档位 `full`（ADR 0043）；无模型默认值 | 无配置也能启动，登录后自动有可用模型 | 默认模型和权限由引导与 `config.toml` 决定（细节未核实） | 登录后即有默认模型 | 向导写入 | 无主模型默认值；小模型默认 Zen 上的 `gpt-5-nano`（[providers](https://opencode.ai/docs/providers)） |

## 3. 每家的关键做法

### 3.1 Radian（实测，2026-10-18）

在空的 `RADIAN_HOME` 里，用 release 二进制跑了一遍首次启动（`script` 提供 pty）：

| 命令 | 结果 |
|---|---|
| `radian` | `[config] none` / `radian: no model: pass --model, or set model.model in the config file`，退出码 2 |
| `radian --provider openai -m gpt-5` | `radian: no credential for 'openai': run 'radian login openai', or '/login openai' in the TUI; refusing to continue …`，退出码 2，**没有进入全屏界面** |
| `radian exec -p hi` | 同一条 no model，退出码 2 |
| `radian login`（没有 tty） | 拒绝，让人手改 `~/.radian/credentials/auth.json`，退出码 2 |

- **先有鸡还是先有蛋**：没有凭据时 TUI 在进屏之前就拒绝，可报错却让人“在 TUI 里 `/login`”。这条路走不通。来源是 `radian-tui/src/cli/run_resolve.rs:55-61`（无模型）以及凭据检查（`agent-cli/src/setup.rs:220-241`，TUI 的文字与它同源）。
- **ADR 0008 的补记（356df1e，只改了文档）** 已经定了：首次启动创建注释模板；TUI 没有模型时打开 `/model` 选择器，没有 key 时接着走 `/login`，选好后写回 `provider` 和 `model`；`radian exec` 仍然退出，但要在消息里写出配置文件的路径和 `--provider <p> --model <m>`（`docs/adr/0008-config-schema-v1.md:49-53`）。代码里还没有：`rg "created a default config"` 没有结果。补记**没有覆盖**“给了 `--model` 但没有凭据”这种情况。
- **面向用户的文字过时，还夹着内部术语**（`radian --help` 实测）：
  - `--provider` 的说明仍写 “`openai-compatible` (the default, unchanged) or `anthropic`”，实际内置了 5 个描述符：opencode-go、chatgpt、anthropic、openai、openai-compatible（`radian-host/src/provider.rs:1038-1106`）。
  - `login` 写的是 “Store an API key … (ADR 0026 D3)”，没提 ChatGPT 登录。
  - `--config` 写 “Read-only: this slice never writes it”。
  - 子命令说明里到处是 ADR 编号；`serve`、`mcp` 是 “Reserved; not implemented”，却列在帮助里。
  - README「Run」一节仍写 “Both hosts read the API key only from an environment variable”，和 ADR 0026 D2 矛盾（代码只读 `auth.json`）。
- **强项**：`radian login` 一个命令就包含了 ChatGPT 的浏览器登录和设备码登录（`agent-cli/src/login.rs:219-289`）。端口 1455 被占时会明确提示改用设备码（`login.rs:414-417`）。存 key 前先验证（`radian-host/src/login.rs:4-10`）。报错不带 key（同文件 :9-10）。
- **体积与依赖**：40.1 MiB，未 strip。`Cargo.toml` 里没有 `[profile.release]`，只调了 `profile.dev`（`Cargo.toml:40-43`）。体积大头推测是 `syntect`（默认语法集）、`rusqlite`（bundled）、`grok-mermaid`、`reqwest`+rustls（推断，没跑 `cargo bloat`）。
- **平台**：沙箱后端只有 macOS 和 Linux（README「Permission tiers」）。Windows 只在 `oauth_chatgpt.rs` 和 `agent-extensions/src/process.rs` 里有 `cfg(windows)`，没有承诺支持。

### 3.2 pi
- 一条 curl 命令，需要时代装 Node，并锁定所有依赖（`quickstart.md:9-21`）。文档开头就讲清楚“模型”和“provider”的区别，然后只让用户做一件事：`/login`（:52-64）。
- 没有模型时，三种情况都有固定提示，并指向本地的 `providers.md`/`models.md`（`dist/core/auth-guidance.js:4-20`）。
- 卸载一节把三种安装方式和“数据不会被删、在哪里”都写了（`quickstart.md:116-136`）。
- 代价：安装/更新遥测**默认开**（`docs/settings.md:170`），每次启动都请求 `pi.dev` 查版本（`version-check.js:65-77`）。Radian 的 ADR 0013 不允许这样做。

### 3.3 Codex CLI
- 安装器默认从 `releases.openai.com` 下载，失败时回落到 GitHub Releases（`README.md:28`）。Homebrew 安装的用户查 cask API，避免提示一个 brew 里还没有的版本（`tui/src/updates.rs:60`）。
- 引导是三屏，有状态（`onboarding_screen.rs:70-98`）。登录屏同时给浏览器、设备码和 API key 三种方式；无头机器走 `headless_chatgpt_login.rs`。
- `codex doctor` 的设计原则值得照搬：只读、不修复、不启动常驻服务、每行脱敏、同一份数据既出人读的摘要也出 `--json`、PATH 里的程序只看不执行（`doctor.rs:1-12`）。
- `login --with-api-key` 从 stdin 读 key，报错里直接给出 `printenv OPENAI_API_KEY | codex login --with-api-key`（`cli/src/login.rs:283`）。CI 用起来不需要 tty，key 也不会出现在命令行上。

### 3.4 Claude Code（全部来自文档）
- 发布工程最完整：两个发布通道，`minimumVersion` 设下限，签名的 manifest 带每个平台二进制的 SHA-256；macOS 公证、Windows 签名；Linux 有签名的 apt/dnf/apk 源（[setup](https://code.claude.com/docs/en/setup)）。
- 自动更新先下载到 `~/.local/share/claude/versions/`，下次启动生效，并保留两个旧版本（同页）。
- `claude doctor` 会显示自动更新是否被关、是谁关的，例如 `disabled (set by env: DISABLE_AUTOUPDATER)`。用户不用猜配置为什么没生效。
- quickstart 只假设用户“有一个订阅”。从安装到第一次改代码是 4 步，每步都给了可以直接复制的提示词（[quickstart](https://code.claude.com/docs/en/quickstart)）。

### 3.5 omp
- 首次启动的向导有版本号：`storedSetupVersion < CURRENT_SETUP_VERSION` 时只补跑新增的场景，`omp setup` 可以强制重跑，非 TTY 时明确拒绝（`main.ts:669-697`，`commands/setup.ts:22-31`）。
- 更新时为“发行方式改变”留了前向兼容：manifest 里的 `omp.dist = "binary"` 让旧版更新器改装独立二进制，并校验大小和摘要（`update-cli.ts:118-195`）。

### 3.6 opencode
- 安装渠道覆盖最广（9 种以上，`README.md:50-61`）。README 有 20 多种语言。
- `uninstall` 默认保留数据，带 `--dry-run`（`commands.ts:76-88`）。`debug paths` 列出所有目录（:120-135）。
- 更新策略只有一个配置键，三个值 `auto/notify/disable`，并兼容旧键 `autoupdate`（`normalize.ts:73-95`）。

## 4. 新用户从零到第一次成功编辑

“决定”指用户必须自己选的地方，不算输入任务本身。

| 工具 | 步骤 | 步数 | 决定 | 中途报错 |
|---|---|---|---|---|
| Claude Code | ① `curl … claude.ai/install.sh \| bash` ② `cd 项目 && claude` ③ 选登录方式，在浏览器里完成 ④ 输入任务 ⑤ 编辑前确认 Yes（默认权限模式会问） | 5 | 2（登录方式；批准编辑）。目录信任提示文档没写，未核实 | 0 |
| Codex | ① `curl … chatgpt.com/codex/install.sh \| sh` ② `codex` ③ Welcome → Sign in with ChatGPT，浏览器 ④ 信任目录（`onboarding_screen.rs:70-78`）⑤ 输入任务 | 5 | 2（登录方式；信任目录）。编辑是否要批准取决于信任后的默认档位，未核实 | 0 |
| pi | ① `curl … pi.dev/install.sh \| sh` ② `pi` ③ `/login`，选 provider，订阅或 key ④ 输入任务（不逐次询问，`quickstart.md:68`） | 4 | 1–2（provider；可选 `/model`） | 0 |
| omp | ① 安装（bun/brew/二进制）② `omp`，setup 向导配置 provider ③ 输入任务 | 3–4 | ≥2（向导里的场景，具体内容未核实，`pi-tui` 包不在本机） | 0 |
| opencode | ① `curl … opencode.ai/install \| bash` ② `opencode` ③ `/connect`，选 provider，粘贴 key（Zen 要先在网页上填账单）④ `/models` ⑤ 输入任务（`/init` 可选） | 5 | 2（provider；模型） | 0 |
| **Radian 今天** | ① 拿到私有仓库的访问权并 `git clone` ② 装 Rust ≥ 1.88 ③ `cargo build --release`（4 核机器实测 3 分 52 秒），自己把二进制放进 PATH ④ `radian` → **no model，退出** ⑤ `radian login` → 编号列表，选 provider，浏览器或 key ⑥ `radian --provider chatgpt -m <要自己知道的模型名>` → **如果第 ⑤ 步漏了，又是 no credential，退出** ⑦ 项目有资源时出现信任弹窗（5 个选项，默认高亮“不信任”）⑧ 输入任务，`full` 档下编辑不问 | 7–8 | 3（provider；模型名，报错里没有候选；信任） | 1–2 |
| Radian（ADR 0008 补记做完，再加 §6 的第 1 条） | ① 下载二进制或 `curl … \| sh` ② `radian` → 自动建配置 → `/model` 选择器 → `/login` → 写回 ③ 输入任务 | 3 | 2（provider/模型；登录方式） | 0 |

## 5. Radian 的优势

1. **分发的底子最轻**：没有运行时依赖，开 LTO 并 strip 后 24.6 MiB，gzip 后 10.7 MiB。Codex 的包大约 95–110 MB（压缩后，含附带组件），pi 和 omp 要先装 Node 或 bun。LTO 让体积降了 39%（40.1 → 24.6 MiB），release 编译从 3 分 52 秒变成 5 分 59 秒（4 核，实测）。
2. **ChatGPT 订阅登录已经实现**，浏览器和设备码都有，端口被占时会给出改用设备码的提示。这和 Codex 的引导持平，比 opencode 只有 key 的入口强。
3. **默认不联网、不收遥测**（ADR 0013）。pi 的安装遥测默认开，Claude Code 和 Codex 都会自己联网查更新。对重视隐私的用户，这是可以写进卖点的一条。
4. **数据只在一个地方**（`~/.radian/`，ADR 0020），权限是 0700/0600，`RADIAN_HOME` 一个变量就能整体换位置。卸载和迁移都比 Claude Code 简单，后者的数据分散在 `~/.claude`、`~/.claude.json`、`~/.local/share/claude` 三处。
5. **报错的形状已经对**：退出码统一（ADR 0032 A3），报错会说下一步做什么，凭据从不出现在输出里。缺的是那条路得真的能走通。

## 6. Radian 的劣势和缺口

1. **装不上**：仓库私有，没有 release、没有安装脚本、没有包管理器，也没有 `cargo install --git` 的说明。这是用户数为零的直接原因。
2. **首次启动沿正常路径会失败两次**，而且报错指向一个打不开的界面（§3.1）。
3. **没有诊断命令**。用户遇到 “sandbox probe failed”（README：探测失败就拒绝运行）、`auth.json` 格式错误、token 过期、bwrap 没装这类问题时，只能读源码。
4. **用户文档等于开发文档**：README 先讲 crate 和 ADR，`--help` 里满是 ADR 编号和 “this slice”，几处说明已经和代码不一致（§3.1）。
5. **没有更新机制**，用户也没有办法知道有新版本。
6. **CI 和无头场景下认证不方便**：`radian login` 没有 tty 就拒绝，也不读环境变量里的 key（ADR 0026 D2）。CI 只能自己拼 `auth.json` 文件。Codex 的 stdin 方式既安全又能写进脚本。
7. **二进制没有做体积优化**：没有 `[profile.release]`（`Cargo.toml`），也没 strip。
8. **没有卸载说明，也没有 `paths` 这类命令**。

## 7. 改进方案（按对 Radian 整体价值的收益/成本排序）

**1. 做完 ADR 0008 补记，并补上“没有凭据也进 TUI”**
- 做什么：
  - 首次启动建注释模板。
  - TUI 没有模型时进 `/model`。
  - **给了 `--model`/`--provider` 但没有凭据时，也进 TUI，直接打开 `/login <provider>`**，不再报错退出。
  - `radian exec` 的报错列出已登录 provider 的候选模型名（用描述符的模型表，`provider.rs:737-990`）。
- 收益：最常见的第一次启动从“两次退出”变成一屏走完（§4 最后两行），也修好了那条指向打不开的界面的报错。
- 成本：低到中。选择器和 `/login` 向导都已经有了，要做的是把启动顺序改成“先进屏，再解析凭据”，再加 pty 测试。
- ADR：前三项在 0008 补记和 0026 D3/D5 的范围内。“没有凭据也进屏”补记没写到，需要给 0008 再加一条补记（不推翻原决定）。
- 验证：pty 测试覆盖空 `RADIAN_HOME` 的四种情况（无配置无模型、有模型无凭据、有凭据无模型、都有）；对已经存在的配置文件，确认一个字节都没改。

**2. 修用户看得到的文字（不改任何决定）**
- 做什么：
  - `--help` 去掉 ADR 编号和 “slice”。
  - 改正 `--provider` 和 `login` 的说明。
  - 隐藏 `serve`/`mcp`（`#[command(hide = true)]`）。
  - README「Run」改成 `auth.json`/`radian login` 的说法；“Configuration file”一节和首次启动的行为同步。
  - 在 README 开头加一个 10 行的 “Getting started”：安装 → `radian` → 登录 → 输入任务，并写清 `full` 是默认档位、怎么换成 `workspace`。
- 收益：提高可信度；README 是第一印象。成本：低，纯文本，外加 help 输出的快照或 grep 测试。ADR：无。
- 验证：`radian --help | rg 'ADR|slice'` 为空；已有的 `cli.rs` 测试保持通过。

**3. 发布产物 + 安装脚本 + 体积配置**
- 做什么：
  - 加 `[profile.release]`，用 `lto`、`codegen-units = 1`、`strip`，不改 `panic`：TUI 用 `catch_unwind` 接住 panic 后才恢复终端、返回 101（`radian-tui/src/cli.rs:616`，`terminal.rs:406`），`panic = "abort"` 会破坏这条路径。实测 LTO+strip 是 24.6 MiB，比现在小 39%。
  - 用 GitHub Actions 出 4 个目标的 tarball，每个配 `sha256`：macOS arm64/x86_64，Linux x86_64/aarch64；Linux 优先 musl 静态链接，需验证 rusqlite bundled 和 rustls 在 musl 下能编。
  - 写 `install.sh`，装到 `~/.local/bin`，可以指定版本，并打印下一步该做什么。
  - 之后再做 Homebrew tap 和 `cargo binstall` 元数据。
- 前提：决定公开仓库，或者至少公开 release。
- 收益：用户量从 0 到可能有。这是本区域收益最大的一项，前提一旦满足就排第一。
- 成本：中（一套 CI，再在 macOS 上做一次签名或公证的评估）。
- ADR：需要一份新的“分发”ADR，写清发布目标、平台、签名、安装位置，以及安装器不联网做别的事（对齐 ADR 0013）。
- 验证：在干净容器里 `curl | sh` 后能跑 `radian --version`；体积进 `benchmarks/perf-baseline.md`；每次发布做一次 smoke test，可以照 Codex 的 `scripts/codex_package/smoke_tests/`。

**4. `radian doctor`（只读，`--json`）**
- 做什么：
  - 本地检查：版本和二进制路径；`~/.radian/` 各目录在不在、权限对不对（ADR 0020）；配置解析结果和被忽略的键（复用启动时的 `[config]` 逻辑）；`auth.json` 里有哪些 provider（只列名字和类型，绝不输出值）；chatgpt token 的过期时间；目录缓存的新旧以及是否在用内置快照（ADR 0026）；沙箱后端探测（复用启动时的 probe）；`$TERM`、真彩色、`HERDR_*`。
  - 网络检查：可选的 `--online`，对每个已登录 provider 跑一次 `login::verify`。
- 收益：能自己排障，issue 报告也有固定格式。最常见的硬失败（沙箱探测失败就拒绝运行）终于有人能看懂原因。
- 成本：中低，大部分代码已经存在。照 `codex doctor` 的五条原则做（`doctor.rs:1-12`）。
- ADR：`--online` 是新的出网项，要写进 ADR 0013 的清单（修订）。离线部分不碰任何 ADR。
- 验证：临时 `RADIAN_HOME` 的快照测试；变异测试：把一个目录改成 0755，doctor 必须报出来；`auth.json` 里放假 key，输出里不能出现这个 key。

**5. 无头认证：`radian login <p> --with-api-key` 从 stdin 读**
- 做什么：照 Codex 的做法（`cli/src/login.rs:283`）。没有 tty 时，只有显式加了这个参数才接受 stdin，只写进 `auth.json`。
- 收益：CI、容器和 herdr 里的批量启动都能用，key 不会出现在命令行上，也不会经过环境变量。
- 成本：低。
- ADR：符合 ADR 0026 D2（key 仍然只从 `auth.json` 来）。不过现在的拒绝理由是“管道里的 key 可见”，放宽它需要给 0026 加一条修订，写明只有显式参数时才接受。
- 验证：`printf key | radian login openai --with-api-key` 之后 `auth.json` 是 0600，stderr 和 journal 里都没有 key。

**6. 卸载与数据说明，以及 `radian paths`**
- 做什么：在 README 和 doctor 里写清卸载方式：删二进制，`~/.radian/` 可以保留，每个子目录存的是什么。`paths` 可以是 doctor 的一个子集。
- 收益：可信度。用户知道自己的会话和记忆在哪、怎么带走。成本：很低。ADR：无，引用 0020 即可。

**7. 版本提示（默认关，或者每次都问）**
- 做什么：
  - `radian update` 只对安装脚本装的版本生效，下载并校验 sha256。
  - 启动时查版本**默认关**，用 `update.check = "notify"` 打开，打开后只请求 GitHub latest release，不带任何标识。
  - Homebrew 用户照 Codex 的做法去查 cask。
- 收益：用户能留在最新版，也就不会撞上已经修好的 bug。
- 成本：中。
- ADR：任何主动出网都要修订 ADR 0013。前提是第 3 条已经有发布产物。
- 验证：默认配置下用网络 mock 断言零请求；打开后只有一次请求。

**8. 文档站或 `docs/user/`（等 1–3 做完再写）**
- 做什么：写 quickstart、providers（每家怎么登录、有哪些模型）、权限档位（面向用户的写法）、排障（列出常见报错，配上 doctor 的输出）。可以先放在仓库的 `docs/user/` 里，以后再接到站点。
- 收益：pi 和 Claude Code 的 quickstart 都只要一屏，这是它们“好上手”的一半原因。
- 成本：中，主要是写作。ADR：无。

**先不做**
- 内嵌免费模型或代理服务（opencode Zen 的路线）：要运营后端，也和 ADR 0013 的“不联网”冲突。
- Windows 原生支持：沙箱和 shell 都要重做，而且 AGENTS.md 说本机测不了的东西不要交出去。
- 每次启动都自动更新（Claude Code 原生安装的做法）：和“默认不联网”冲突。真有需要，等用户量起来后作为可选项再议。
