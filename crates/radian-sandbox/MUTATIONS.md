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
