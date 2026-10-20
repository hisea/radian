# 工具描述审查：改掉与代码不符的，删掉重复的

- 状态：decided＼�只做"对不上 + 删重复"，不做整体压缩）
- 排期：pi B 线，`remove-safe-tier` 合入之后、`full-tier-unrestricted` 之前（第 7、8 条与 `remove-safe-tier` 改同一段 `shell` 描述）
- 涉及 ADR：0039（`grep`/`glob` 的 ignore 规则若写在其中则同步）、0021 D6（`memory_search` 的排序）、0024（`ask_user` 的 `header` 宽度）、0025（skill 名缺省为目录名）、0037（`shell` 超时与自动转后台）
- 来源：lead 2026-10-20 在 gate13 的代码上逐个核对工具描述与实现（`ask-user-pass` 之后的那次审查）

## 背景

工具定义每次请求约 15 KB。逐条和实现对过之后，有 10 处描述和代码不符，4 处同一件事在描述和字段说明里各写一遍。体积不是重点（定义在缓存前缀里，删重复净省约 1 KB）；重点是模型照着错的事实做事。

## 决定（已定）

对不上的（改描述，第 1 条连行为一起改）：

1. `grep`、`glob`：描述说 "`.gitignore` is respected"，实际只读搜索根**以内**的 `.gitignore`/`.ignore`，不读上层目录的，也不读全局 gitignore；在子目录里搜时，仓库根 `.gitignore` 排除的东西（例如子 crate 的 `target/`）会被搜出来。**改行为**：从仓库根（与 `.radian-worktrees` 剪枝同一个根）到搜索根之间各级目录的 `.gitignore`/`.ignore` 也生效，优先级照 git（近的覆盖远的）；全局 gitignore 仍不读。描述写明 `.ignore` 也生效。
2. `skill`：`name` 字段说"frontmatter 的 `name`"，实际 frontmatter 没写 `name` 时用目录名。
3. `memory_search`：描述说"newest/most relevant first"，实际顺序是 scope → 可信度 → 匹配分 → 最新 → id；关键词匹配不分大小写，描述补上。
4. `git_diff`：描述说 `stat` 返回"the short form"，字段说明说 `--porcelain=v1 --branch`；代码是后者，描述改成一致。
5. `ask_user`：schema 的 `header.maxLength: 16` 按字符数，解析器按显示宽度（中文一个字算 2），9 个汉字能过 schema、被解析器拒绝。去掉 `header` 的 `maxLength`，宽度只由描述说明、由解析器检查。
6. `shell`、`job`："kills the whole session" 指进程会话（进程组），模型容易读成 agent 会话；改成 "the whole process group"。
7. `shell`：只说 "Clamped to the configured maximum"，模型看不到任何数字。写明：不传 `timeout_secs` 时，前台跑到自动转后台阈值就转后台、不会被杀；传了就从启动起算到点杀掉，有上限；每个输出流有缺省预算。数字从 `Limits` 和配置取，不在描述里写死成与配置不符的值。
8. `shell`：`full` 档那段里的 ADR 编号和 `--profile workspace` 是给人看的，删掉。
9. `shell`："they … run in parallel"——`grep`/`glob` 已并行，`read` 还没进阻塞线程池（`file-tools-spawn-blocking.md` 剩下的部分）。本项不改这句；`read` 那部分落地时同分支确认这句成立。若本项先于它落地，这句改成只说 `grep`/`glob`。
10. `read`：`limit` 字段说明只说行数；对目录它是条目数上限（缺省 500），字段说明补上。

删重复的（同一件事只留一处，约束留在 schema，用法留在描述）：

11. `patch`：`edits` 的语义（在原文里定位、范围不得重叠、一处失败全部不写）描述和字段说明各写一遍；字段里把 `old`/`new` 叫 "legacy form"，与描述把它当主用法矛盾，去掉 "legacy"。
12. `grep`：`output` 三个取值在描述里写了两遍，字段说明里又一遍。
13. `job`：`timeout_ms` 的缺省值与上限在描述和字段说明各一遍。
14. `subagent`：`explore`/`general` 的区别在描述和 `agent` 字段各一遍。

不做：系统提示词 `<runtime>` 层的工具清单（与定义重复约 920 B）保留，它是 ADR 0036 D1 定的；整体压缩描述（再省 2–3 KB）不做，等有评测数据再说。

## 验收

- 每条一个测试钉住新文字（关键句 `contains`）或新行为；删重复的条目用测试断言该句只出现在一处。
- 第 1 条：fixture 仓库根 `.gitignore` 写 `target/`，在子目录搜 `grep`/`glob`，`target/` 下的文件不出现；变异：去掉上层 ignore 的读取 → 测试失败。子目录自己的 `.gitignore` 里 `!` 反选能覆盖上层规则。
- 第 5 条：一个 9 个汉字的 `header` 不再被 schema 拒绝、由解析器拒绝并给出 `invalid_arguments`；`agent_core` 仍接受该 spec。
- 实测改动前后每个工具的序列化定义字节数，写进报告；`read`/`ask_user` 已有的体积上限测试按实测值更新。
- 落地时同分支改涉及的 ADR（docs-only commit，只改与新文字或第 1 条行为相关的句子）并删掉本文件。
