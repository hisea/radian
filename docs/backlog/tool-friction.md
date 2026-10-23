# 工具摩擦：`write` 自动建父目录、`shell` 的 clamp 提示归位

- 状态：decided（可以派发；两项互不依赖，可以一个 worker 做）
- 涉及 ADR：0039（第 2 条：文件工具按 fd 逐级打开，不按路径重开）、0037（第 2 条：前台超时缺省 300 s、上限 600 s；结果文本的标记行）
- 来源：一天内 39 个 worker 会话的 journal 统计（5125 次工具调用，其中 `shell` 1738、`read` 1286、`patch` 1116、`write` 148）

## 背景

两处摩擦每天都在重复，各自只值一轮往返，但每次都让 worker 多想一步、多调一次，而且第二处还会误导看 transcript 的人。

1. **`write` 要求父目录已存在。** 一天里撞了 12 次（新 crate 的 `Cargo.toml`、新模块目录 `src/review/mod.rs`、`.cargo/config.toml`）。错误文本是 `` `crates/radian-arc`: No such file or directory ``，只给出缺失的那一级，模型要自己推断是父目录没建，再用 `shell` 跑一条 `mkdir -p`，再重发 `write`。三轮换一个文件。工具描述里写着"The parent directory must exist"，模型照样撞。
2. **`shell` 的 `[timeout clamped to the configured maximum of 600s]` 拼在 `--- stderr ---` 段之后。** 一天里出现 34 次，全部来自同一个模型给 `timeout_secs` 传 900（schema 已写明 Maximum: 600，它不改）。这行紧跟 stderr 内容、没有分隔，在 TUI 里和在模型眼里都像是命令的 stderr 报了超时。实际这 34 次没有一次超时（1738 次 `shell` 只有 3 次真超时，最长一次 550 s）。看 transcript 的人会把连续两条读成"连着两次 timeout"。真正的超时提示 `[timed out after …]` 在 `exit code:` 之前的标记区，位置不一致。

## 决定（已定）

### 1. `write` 补齐缺失的父目录

- `write` 在工作区内自动创建缺失的父目录，和文件一样按 fd 逐级走：对已存在的一级 `openat(O_NOFOLLOW | O_DIRECTORY)`，缺失的一级 `mkdirat(dirfd, name, 0o755)` 后再 `openat(O_NOFOLLOW | O_DIRECTORY)` 打开它继续。任何已存在的一级是符号链接时拒绝，和今天一样；通过链接建目录和写文件都不发生。不按路径重开（ADR 0039 第 2 条不变）。
- 只在工作区内；工作区外的 `write` 仍然拒绝，不建任何目录。`patch` 不变（它只改已有文件）。
- 失败时的错误文本按今天的形状报缺失或被拒的那一级。
- 工具描述把"The parent directory must exist"换成"Missing parent directories are created"，`crates/radian-coding` 里逐行钉住描述文本的测试同步改。
- ADR 0039 第 2 条补一句：`write` 缺失的父目录由父级 fd 上的 `mkdirat` 创建，再按同一规则打开。docs-only commit。

### 2. `shell` 的两条 clamp 提示移到标记区并写明数字

- `[timeout clamped …]` 和 `[output budget clamped …]` 从 stderr 段之后移到 `exit code:` 之前，与 `[timed out after …]`、`[cancelled …]` 同一处。
- 措辞写明两个数字：`[timeout_secs 900 is above the maximum 600; the command ran with the 600 s limit]`、`[max_output_bytes N is above the maximum M; each stream was capped at M bytes]`。
- structured 里的 `timeout_clamped`、`output_clamped` 不变。`--- stderr ---` 之后只剩 stderr 本身和截断提示。
- ADR 0037 没有写这两行的位置和措辞，不需要改；手册里提到 `timeout_secs` 的页面（`docs/user/subagent-and-jobs.md`）如有示例输出，跟着改。

## 验收

- `write` 到 `a/b/c.txt`（`a` 不存在）创建两级目录和文件；`a` 存在且是指向工作区内或工作区外的符号链接时拒绝，`a` 下没有新建任何东西，拒绝文本的形状与今天 `tests/safe_fs.rs` 矩阵里的两种一致；在词法检查与第一次 `openat` 之间把父目录换成链接的 swap 测试照旧拒绝；工作区外的路径拒绝且不建目录；`patch` 到不存在的文件仍然拒绝。
- 变异：缺失一级改成按路径 `create_dir_all`；对链接一级不拒绝。各有测试失败。
- `shell`：`timeout_secs: 900` 的调用，结果文本在 `exit code:` 之前有一行写明 900 与 600，`--- stderr ---` 之后没有它；`max_output_bytes` 超上限同样；没有 clamp 时两行都不出现；`timed_out` 为真时 `[timed out after …]` 仍在同一区。钉住描述与结果文本的测试同步。
- 两处的工具描述改动都过 `crates/radian-coding` 的 docs drift 测试；`docs/user/` 里凡提到父目录或这两条提示的地方同步。
