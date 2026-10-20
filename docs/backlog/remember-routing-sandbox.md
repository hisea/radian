# "记住这件事"路由到全局 skill 时，文件工具写不到 `~/.radian/`

- 状态：idea（给 memory-tools fix round 1 的输入；定收口方式）
- 涉及 ADR：0021（D5 写来源）、0036（角色规则）、0039（文件工具的路径边界）、0016（workspace 档写出工作区被拒）
- 来源：Fable 2026-10-20 核对；决定跨项目做法进 `~/.radian/skills/<name>/SKILL.md`、项目内做法进 AGENTS.md

## 背景（已核实）

- `write`/`patch` 只接受工作区相对路径（`crates/radian-coding/src/write.rs` 的 schema 与 `WorkspaceRoot`）；`read` 系列的额外只读根只有 cargo registry 与 rustup 源码（`read_roots.rs`）。模型**用文件工具写不了** `~/.radian/skills/`。
- 唯一能写的路径是 `full` 档下的 `shell`（`cat > ~/.radian/skills/...`）；`workspace` 档写出工作区被沙箱拒绝。
- 所以"写成全局 skill"的规则在沙箱下会撞墙，在 `full` 下会绕过文件工具的保护去写。

## 待决（三选一）

1. 规则写明：`full` 档用 `shell` 写；沙箱开着时把内容交给用户、让用户放。零代码。
2. 给 `write` 加 `~/.radian/skills/` 可写根。和 0039 "文件工具不出工作区"相抵，不建议。
3. 宿主出一条用户命令，模型把内容交给用户确认后写入（同 `/memory add` 思路）。要新 slice。

建议 1 先写进 round 1，project-skills 落地后再看 3。

## 验收

规则文字和 ADR 0021 一致；沙箱会话里"记住一个跨项目做法"不产生被拒的写入、而是把内容交给用户。
