# 用户文档随二进制发布，模型按需读

- 状态：idea（下面的设计是建议，待决问题列在末尾；定了再改 decided）
- 涉及 ADR：0039 §4（`read`/`grep`/`glob`/`list` 多一个只读根）、0020 §2/§3（`~/.radian/docs/` 进布局与沙箱可读清单）、0036（`<runtime>` 层多一段文档索引）、0013（不出网取文档）；0025 不改
- 来源：「让 radian 文档跟着二进制一起，用户可以问 radian 的功能怎么使用」；`docs/research/2026-10-18-area-deep-dive/15-onboarding-distribution.md` §8（`docs/user/`）、§3.1 与第 4 条（`--help` 与 README 夹 ADR 编号、"slice"）
- 依赖：先有面向用户的文档。今天一页也没有：README 讲 crate 与 ADR，`docs/` 下是设计、ADR、研究；`/help` 的每一行都带 ADR 编号。

## 背景

用户在会话里问「怎么切模型」「workspace 档能不能联网」时，模型手里没有任何 radian 的说明：源码不随二进制走，README 不在用户机器上，`<runtime>` 只有工具一句话用途与档位事实。它只能猜，或者答「我不知道」。

同类工具（2026-10-20 在本机核实）：

- **pi**：npm 包带 `docs/`（48 个 `.md`，约 359 KB）、README 与 `examples/`；系统提示里有一节 `docs`：「只在用户问 pi 自身、SDK、extensions、themes、skills、TUI 时读」，给出 README、docs、examples 三个绝对路径，再给一张主题→文件的索引（extensions → `docs/extensions.md`，themes → `docs/themes.md`，……），并要求「把 .md 读完整、跟着交叉引用读」（`dist/core/system-prompt.js:88-96`）。约 230 tokens，常驻。
- **Claude Code**：文档在 code.claude.com，靠一个专门的子 agent 联网抓取；离线不可用。
- **Codex** 的 npm 包只有 `bin/`、`package.json`、`README.md`；**omp** 的包没有 `docs/` 目录（提示词是否指向文档未核实）；**opencode** 只有 opencode.ai/docs（研究 15 的记录，本机安装包未核实）。

radian 的约束，决定了能走哪条路：

- ADR 0013 默认不出网 → Claude Code 的联网路线不可用。
- ADR 0039 §4：`read`/`grep`/`glob`/`list` 在工作区之外**只**接受两类只读根（cargo registry 源码、工具链 `rustlib/src`），其余路径一律拒绝。所以文档落在磁盘上就必须多一个只读根；机制在 `radian-coding/src/read_roots.rs`，是一张表，加一个根不需要新机制。
- ADR 0025 的 skills 路线不合适：`skill` 工具一次返回整个正文，上限 64 KiB；一套几百 KB 的手册要拆十几个 skill，每个占 skills 节的 1000 token 预算一行，而且不能跨页 grep。
- 新做一个 `docs` 工具会给每个会话多一条工具定义；ADR 0043 刻意压工具数（memory 与 job 合并后 13 个）。
- ADR 0020 §3 的沙箱实现是**禁读清单**（`radian-sandbox/src/policy.rs` 的 `deny_read`），`~/.radian/` 下新目录天然可读，只需 ADR 的清单补一行、矩阵测试补一项。

## 建议的设计（未定）

照 pi 的形状，用 radian 已有的机制承载，不加工具、不加依赖：

1. **源**：`docs/user/*.md`，面向用户，不出现 ADR 编号与「slice」；每页开头一行描述（frontmatter `description:`），生成索引用。页数建议约 12：quickstart、commands（斜杠命令与 `!`）、configuration、providers-and-login、permissions（档位、信任、`/access`、沙箱事实）、skills、memory、sessions（resume、fork、export）、subagent-and-jobs、themes、mcp、troubleshooting。
2. **进二进制**：`radian-host` 用 `include_str!` 编进去，表由 `build.rs` 扫 `docs/user/` 生成（只用 std，不加 `include_dir`/`rust-embed`）；先例是 skills 的 `BUILTIN` 表与 `CATALOG_SNAPSHOT`。总量设上限（建议 512 KiB），测试断言。
3. **落盘**：TUI 与 `radian exec` 启动时比对 `~/.radian/docs/.version` 与二进制里内容的哈希；不同就写进临时目录再整目录 rename 过去，相同就什么都不做（每次启动一次 stat）。落盘而不是做虚拟文件系统，是为了让 `read`/`grep`/`glob` 原样可用。这个目录模型写不到（不在工作区）。
4. **只读根**：ADR 0039 §4 加第三类只读根 `$RADIAN_HOME/docs`（同样从根 fd 逐级 `openat(O_NOFOLLOW)`，symlink 拒绝）；错误提示里一并列出。ADR 0020 §2 布局树加 `docs/`，§3 可读清单加 `docs/`。
5. **提示词**：`<runtime>`（0a 层，进程内不变，`--system` 盖不掉）在工具列表之后加一段，形状照 pi：根路径一行；「只在用户问 radian 自身（命令、配置、档位与沙箱、skills、memory、provider）时读；回答前把相关页读完整」一行；索引每页一行 `- <topic>: <file>`。预算 ≤ 300 tokens（ADR 0015 估算），测试断言；同一组文档逐字节相同。
6. **入口**：不加斜杠命令，用户直接用自然语言问。`/help` 仍是命令清单，末尾可加一行提示「关于 radian 的问题直接问」。
7. **不做**：联网取文档（0013）；把 ADR 与 crate README 打包进去（那是开发者文档，带编号，200 KB 以上，不是同一套）；`docs` 工具；skills 路线。

## 待决问题

- **语言**：文档用英文还是中文，还是两套？ADR 是中文、提示词是英文、getrad.dev 双语。两套体积翻倍；一套英文、模型按用户语言当场翻译是最省的。建议一套英文；由 定。
- **目录名**：`~/.radian/docs/` 还是 `~/.radian/cache/docs/`？`cache/` 已在可读清单里、也符合「可再生」的性质；但 0020 的树是逐项列的，两者改动量一样。建议 `docs/`，一眼能看懂。
- **版本戳**：`CARGO_PKG_VERSION` 在开发构建里每次都是同一个值，会让改了文档却不重写；建议用内嵌内容的哈希，自包含且确定。
- **`radian docs [topic]` 子命令**：没有模型也能在终端里看页面，成本低；要不要做。
- **谁写 `docs/user/`**：这是这件事的主要成本；写之前先定页面清单与语言。

## 验收

- 干净 `HOME`：启动一次 → `~/.radian/docs/` 有全部页面与 `.version`；再启动 → 不重写（mtime 不变）；换一个内容哈希不同的二进制 → 重写。
- `full` 与 `workspace` 两档下 `read ~/.radian/docs/commands.md` 成功，`grep` 以该根为范围成功；`~/.radian/credentials` 仍不可读；`docs/` 下放一个 symlink 被拒绝；沙箱矩阵加 `docs_readable`。
- 系统提示：`<runtime>` 含文档段，两次构建逐字节相同，≤ 300 tokens；`--system` 下仍在。
- pty 测试：问「how do I switch models?」，mock 模型对文档页调一次 `read`，工具结果含该页内容（证明提示词里的路径端到端可读）。
- 文档与代码同步测试：TUI `HELP` 里的每个 `/command` 出现在 commands 页；`<runtime>` 工具列表里的每个工具名出现在文档里；`rg 'ADR|slice' docs/user` 为空。
- 落地时同分支 docs-only commit 改 ADR 0039 §4、0020 §2/§3、0036 的层表，并删掉本文件。
