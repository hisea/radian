# 复合 shell 命令按段判断 allow

- 状态：decided＼�
- 涉及 ADR：0012（allow/ask/deny 语义不变，不改决定）；改 `crates/radian-permissions/README.md` 的 shell 一节
- 来源：`docs/research/2026-10-18-area-deep-dive/7-permissions-sandbox.md` §6 ③；

## 背景

`radian-permissions` 的 `is_simple` 要求整条命令都在安全字符集里，出现任何元字符就不算简单命令，于是走 `ask`。`cargo fmt && cargo test`、`cd crates/x && cargo test`、`rg foo | head` 永远要问，哪怕每一段单独都命中 allow 规则。deny 今天已经会拆开逐段查（`shell.rs` 的分隔符、引号、子 shell 处理）；allow 没有对应的拆分。

在 `full-tier-unrestricted.md`（`full` 全放行）和 `sandbox-auto-allow-shell.md`（沙箱内放行）之后，这条规则实际起作用的地方是：`workspace` 档在沙箱内放行落地之前；`approval = always` 的逐条审阅；用户自己写的 `--allow-shell-command` 前缀规则对复合命令的命中；以及 `workspace` 档越界请求之外一切仍走规则表的路径。它只影响 allow 命中得更多，deny 逻辑完全不动。

## 决定（已定）

- 新增 `shell::segments(command) -> Option<Vec<String>>`：只在**未加引号**的 `&&`、`||`、`;`、`|` 处切分。每一段仍必须满足今天的 `is_simple`；出现任何其他元字符（重定向、`$(…)`、反引号、`&` 后台、换行、通配符等）整串返回 `None`，退回今天的行为（`ask`）。
- 复合命令只有在**每一段**都命中 allow 规则时才 allow；任一段没有规则就 `ask`。deny 的拆分和匹配照旧最先生效。
- 一张"执行其参数"的程序表：`xargs`、`env`、`sudo`、`nohup`、`time`、`timeout`、`watch`、`sh`、`bash`、`zsh`、`eval`、`exec`、`command`、`builtin`、`nice`、`stdbuf`。表里的程序出现在某一段的段首时，这一段不能靠前缀规则 allow（参照 Claude Code 剥包装的清单与 Codex 的禁止前缀清单）。管道传的只是数据，`a | xargs rm` 这类段尾执行由这张表挡住。
- `cd <路径>` 作为一段：只有在目标解析后仍在工作区内、不含 `..` 越界时才算可放行的段，且仅限 `workspace` 档（`.git/hooks` 在沙箱里只读）。`full` 档下 shell 整体 allow，`cd` 段的问题不存在；若 `full` 档配了 `approval = always`，`cd` 段一律不放行，因为 git 会跑目标目录的 hooks，而 `full` 没有那层保护。
- 纯 Rust 小函数，不引入 tree-sitter 或其他依赖。

## 安全论证

每一段满足的条件和今天单条命令一样严；本项只加了一个"每段都要命中"的条件，只可能让 allow 命中得更少，不会让 deny 漏判。`shell_metacharacters_are_never_allowed` 的意图保留为"复合之外的元字符永不 allow"。`crates/radian-permissions/README.md` 里 shell matcher 看不到的东西（`git -c core.fsmonitor=…`、`cargo build` 跑 build.rs）不因本项变化。

## 验收

`a && b` 在 b 没有规则时 `ask`；`ls | xargs rm` `ask`；引号内的 `&&` 不切分；`cargo fmt && cargo test` 在两段都有 allow 规则时 allow；`workspace` 档 `cd x && cargo test` 在 x 在工作区内且两段有规则时 allow，x 越界时 `ask`。变异：改成"任一段命中就 allow"，测试应失败；去掉"执行其参数"表，`ls | xargs rm` 的测试应失败。README 的 shell 一节同步。
