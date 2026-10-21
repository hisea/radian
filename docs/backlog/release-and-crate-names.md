# 发布：包名改名、release 二进制、Homebrew tap、cargo 安装

- 状态：decided
- 涉及 ADR：0032（一个二进制）；所有提到 `radian-core` / `radian-exec` 的 ADR（改名同步）
- 来源：发布方式讨论（crates.io 与 Homebrew 名字核对）

## 背景

- crates.io 上 `radian`、`radian-core`、`radian-exec` 已被别人占用；其余 14 个包名以及 `radian-core`、`radian-exec` 都空着。`cargo install` 要求二进制包和它依赖的所有 workspace 包都发布在 crates.io 上，所以被占的两个名字必须改。
- homebrew-core 里没有 `radian`；`rad` 是 core 里另一个 formula 的名字。
- private 仓库的 Release 附件下载要认证：brew、预编译二进制、`cargo binstall` 要等仓库公开后才能给外人用。

## 决定（已定）

1. **改名**：`radian-core` → `radian-core`，`radian-exec` → `radian-exec`。包名、目录名（`crates/radian-core`、`crates/radian-exec`）、库名（`radian_core`、`radian_exec`）一起改，代码里的 `use` 路径、文档、ADR、`AGENTS.md`、`README.md`、`README.zh-CN.md` 同步。`research/` 和 `spikes/` 是历史记录，不改。其他 `agent-*` 包不改名。ADR 改名放在自己的 docs-only commit 里。
2. **可发布的元数据**：每个 workspace 包都有 `description`、`license = "MIT OR Apache-2.0"`、`repository`、`rust-version`（可以继承 workspace），path 依赖带 `version`；不发布的包（测试夹具一类）标 `publish = false`。本条只做到 `cargo publish --dry-run` 能过，**不真正发布**。
3. **release workflow**（`.github/workflows/release.yml`）：推 `v*` tag 时触发，编译 `aarch64-apple-darwin`、`x86_64-apple-darwin`、`x86_64-unknown-linux-musl`、`aarch64-unknown-linux-musl` 四个目标的 release 二进制（`radian` 和 `rad`），每个目标打成 `radian-<version>-<target>.tar.gz`，附 `.sha256`，上传到这个 tag 的 GitHub Release。
4. **Homebrew tap**：tap 仓库 `hisea/homebrew-tap`，formula `radian`，只装 macOS 二进制，只链接 `radian`（不链接 `rad`，避免和 core 的 `rad` 冲突）。release workflow 在上传完后更新 tap 里 formula 的版本、URL 和 sha256；写 tap 用一个只对 tap 仓库有写权限的 token（secret 名 `TAP_TOKEN`）。tap 仓库和 token 由维护者建，不在本条代码里。
5. **cargo 安装**：README 两个语言版本加安装段：Homebrew、Release 下载、`cargo install --git https://github.com/hisea/radian radian-tui`；Linux 上 `workspace` 档位需要 `bubblewrap`。crates.io 发布后的 `cargo install` / `cargo binstall` 写法等发布时再加。`radian-tui` 包加 `[package.metadata.binstall]`，指向 release 附件的命名。

## 待决问题（不在本条）

- 什么时候发布到 crates.io，以及二进制包对外叫 `radian-tui` 还是别的名字。
- 第一个 tag 的版本号和时间（和仓库公开一起定）。

## 验收

- `git grep -n 'radian-core\|radian_core\|radian-exec\|radian_exec' -- ':!research' ':!spikes'` 为空。
- `AGENTS.md` 的 Verify 全部通过；三个 provider contract 套件通过。
- `cargo publish --workspace --dry-run --locked`（或按依赖顺序逐个 dry-run）通过，不发布。
- `release.yml` 用 `actionlint` 检查通过；在本机能按 workflow 里的命令编出至少 `aarch64-apple-darwin` 的包，tar 里有 `radian` 和 `rad`，sha256 文件对得上。
- formula 模板在本机用 `brew install --formula ./radian.rb`（指向本地编出的 tar）能装上并运行 `radian --version`。
