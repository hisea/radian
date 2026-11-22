<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/readme/banner-dark.png">
    <img src=".github/readme/banner-light.png" alt="radian" width="480">
  </picture>
</p>

<p align="center"><a href="README.md">English</a> | <b>简体中文</b></p>

<p align="center">
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-5b3fd0" alt="license: MIT / Apache-2.0"></a>
  <img src="https://img.shields.io/badge/rust-1.99.1%2B-5b3fd0" alt="rust 1.99.1+">
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux-5b3fd0" alt="platform: macOS | Linux">
  <a href="https://github.com/hisea/radian/releases"><img src="https://img.shields.io/github/v/release/hisea/radian?color=5b3fd0" alt="release"></a>
</p>

# radian

radian 是一个快速、高效的编码 agent，只有一个 Rust 二进制。
计划、任务编排、门禁和评审现在放在 [operad](https://operad.io) 里，它通过 `radian exec` 驱动 radian。

[radian](#radian编码-agent) · [operad](#operad计划与编排) · [安装](#安装) · [文档](#文档)

## radian：编码 agent

- `radian` 打开全屏终端界面，`rad` 是它的别名。
- `radian exec` 是无头模式。`radian exec -p "<任务>"` 跑一个任务后退出；`--output jsonl` 把每一步输出成一行 JSON，退出码说明这一轮是怎么结束的。
- 每个会话是一个 JSONL 日志文件。两种模式都能恢复会话、在轮次边界分叉会话，也可以用 `radian attach` 只读查看。
- 模型接口：OpenAI 兼容的 Chat Completions 和 Anthropic Messages API。工具：内置的文件、搜索、shell 和 job 工具，MCP stdio 服务器，skills，记忆，以及一次性的 `subagent`。
- 你已有的东西直接能用：`AGENTS.md` 和 `CLAUDE.md`、`~/.agents/skills` 里的 skills、`mcpServers` 格式的 MCP 配置、Claude 和 Codex 风格的 hooks，以及 pi 的主题。
- 在 [herdr](https://github.com/ogulcancelik/herdr) 里，radian 自己上报状态（working、idle、blocked）。

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset=".github/readme/terminal-dark.png">
    <img src=".github/readme/terminal-light.png" alt="radian TUI 120 列：一个思考块、一次 shell 调用、一次 read、一次带 diff 的 patch、job 通知、输入框和底栏" width="820">
  </picture>
  <br><sub>radian 在 120 列下的画面，由 TUI 原型脚本渲染。</sub>
</p>

```sh
radian login                          # 存一个模型 key
radian                                # 全屏界面
radian exec -p "summarize Cargo.toml" # 无头，跑一个任务
```

**权限。** 默认档位是 `full`：不开沙箱，内置工具也不弹询问。
`--profile workspace` 打开沙箱（macOS seatbelt 或 Linux bubblewrap），写入限制在工作区内。
项目可以为自己保存 `workspace`，而且项目的设置只能收窄权限。
`full` 不防御来自陌生仓库的提示注入：要边界就用 `workspace`、容器或专用的系统用户。
详见 [`docs/user/permissions.md`](docs/user/permissions.md)。

## operad：计划与编排

计划、任务编排、门禁和评审现在放在 [operad](https://operad.io) 里，它是一个单独的程序。
operad 通过 `radian exec` 驱动 radian：每个任务启动一个无头 worker，跑门禁，只合并通过的结果。
radian 本身仍是编码 agent：终端界面、`radian exec` 和它们用到的工具。

## 安装

用 Homebrew、release 压缩包或 Cargo 从源码安装当前版本。

```sh
brew install hisea/tap/radian
```

这个 tap 只链接 `radian`：homebrew-core 里已经有一个 `rad` formula，所以这里不链接自带的 `rad` 别名。macOS 的包经过签名和 notarization。

或者从[发布页](https://github.com/hisea/radian/releases)下载 `radian-<version>-<target>.tar.gz`，用同名的 `.sha256` 校验后，解开里面的 `radian`（和 `rad`）：

```sh
shasum -a 256 -c radian-<version>-<target>.tar.gz.sha256
tar -xzf radian-<version>-<target>.tar.gz
./radian --version
```

或者从源码构建（会同时装上 `radian` 和 `rad`）：

```sh
cargo install --git https://github.com/hisea/radian radian-tui
```

在 Linux 上，`workspace` 档位用 `bubblewrap` 跑沙箱；先把它装好（Debian/Ubuntu 上比如 `sudo apt install bubblewrap`）。

## 文档

新用户从 [`docs/user/quickstart.md`](docs/user/quickstart.md) 开始；手册的每一页都列在 [`docs/user/index.md`](docs/user/index.md)，也可以在 [getrad.dev/docs](https://getrad.dev/docs/) 看（用户手册是英文）。

| 内容 | 位置 |
|---|---|
| crate、构建、状态、配置和权限参考 | [`docs/internals.zh-CN.md`](docs/internals.zh-CN.md) |
| 设计 | [`ARCHITECTURE.md`](ARCHITECTURE.md) |
| 约束代码的决策 | [`docs/adr/`](docs/adr/) |
| 贡献、安全 | [`CONTRIBUTING.md`](CONTRIBUTING.md)、[`SECURITY.md`](SECURITY.md) |
| Agent 指令（改代码前先读） | [`AGENTS.md`](AGENTS.md) |

## 许可证

按你的选择，使用 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 之一授权。第三方代码及其署名列在 [NOTICE](NOTICE)。
