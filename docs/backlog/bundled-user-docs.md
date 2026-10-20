# 用户文档随二进制发布，模型按需读

- 状态：idea（下面的设计是建议，待决问题列在末尾；定了再改 decided）
- 涉及 ADR：0039 §4（`read`/`list` 多一个**二进制内的虚拟只读根**，不在文件系统上）、0036（`<runtime>` 层多一段文档索引）、0013（不出网取文档）；0020、0025 不改
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
- `read` 的读取逻辑跑在任意 `BufRead` 上（`radian-coding/src/read.rs` 的测试直接喂字节切片），`OpenedFile` 只是 `File` + `Stat`；所以一页编进二进制的文本接进 `read`，改的是 `workspace.rs` 的路径解析与 `OpenedFile` 的来源，不是读取逻辑。

## 建议的设计（未定）

**只在二进制内查找，不落盘**＼�二进制都打包了，为什么还要写磁盘）。照 pi 的形状给提示词一张索引，但页面由 `read` 直接从二进制里取；不加工具、不加依赖、不写 `~/.radian/`。

1. **源**：`docs/user/*.md`，面向用户，不出现 ADR 编号与「slice」；每页开头一行描述（frontmatter `description:`），生成索引用。页数建议约 12：quickstart、commands（斜杠命令与 `!`）、configuration、providers-and-login、permissions（档位、信任、`/access`、沙箱事实）、skills、memory、sessions（resume、fork、export）、subagent-and-jobs、themes、mcp、troubleshooting。
2. **进二进制**：`radian-coding`（文件工具所在的 crate）用 `include_str!` 编进去，表由 `build.rs` 扫 `docs/user/` 生成（只用 std，不加 `include_dir`/`rust-embed`）；先例是 skills 的 `BUILTIN` 表与 `CATALOG_SNAPSHOT`。总量设上限（建议 512 KiB），测试断言。页面与二进制永远一致，没有版本戳、没有陈旧文件、卸载不留东西、`HOME` 只读也能用。
3. **虚拟根**：路径写法 `radian://docs/<page>.md`，一眼看出不是文件系统路径。`WorkspaceRoot::resolve_read` 在绝对路径与 `~` 的判断**之前**先认这个前缀，返回 `Resolved::Embedded(page)`；`open_read` 对它返回内存来源（`OpenedFile` 的 `file` 改成 `File | &'static [u8]` 两种来源，`stat` 合成为常规文件与字节数），`read.rs` 之后的 offset/limit/max_bytes 逻辑原样跑。`list radian://docs/` 返回页面清单（就是索引）。`grep`/`glob` 第一步**不**支持虚拟根：拒绝时说明「用 `read`，页面见 `<runtime>` 的索引」；约 12 页、索引写清楚，`read` 够用。要跨页 grep 再做第二步（`grep.rs` 已有单文件分支，循环页面即可）。
4. **边界**：虚拟根只读；`write`/`patch` 对 `radian://` 前缀拒绝。沙箱、`shell`、`!cat` 看不到这些页面（它们不在磁盘上），这是接受的代价；想在终端里看就做 `radian docs [topic]` 子命令（见待决）。ADR 0039 §4 加一条：第三类只读根是二进制内的文档，非文件系统，无 symlink 问题，错误提示里一并列出。ADR 0020 不动。
5. **提示词**：`<runtime>`（0a 层，进程内不变，`--system` 盖不掉）在工具列表之后加一段，形状照 pi：「只在用户问 radian 自身（命令、配置、档位与沙箱、skills、memory、provider）时读；回答前把相关页读完整」一行；索引每页一行 `- <topic>: radian://docs/<page>.md`。预算 ≤ 300 tokens（ADR 0015 估算），测试断言；同一二进制逐字节相同。
6. **入口**：不加斜杠命令，用户直接用自然语言问。`/help` 仍是命令清单，末尾可加一行提示「关于 radian 的问题直接问」。
7. **不做**：联网取文档（0013）；把 ADR 与 crate README 打包进去（那是开发者文档，带编号，200 KB 以上，不是同一套）；`docs` 工具（工具数）；skills 路线（64 KiB 一次返回、吃 1000 token 预算）。

**否决的方案：落盘到 `~/.radian/docs/`**。好处只有一个：四个文件工具零改动（多一个磁盘只读根就够）。代价是启动时比对版本戳并重写目录、ADR 0020 布局与可读清单各加一行、卸载留垃圾、`HOME` 只读时失败、文档可能与二进制不一致。虚拟根多改的只是 `workspace.rs` 一个解析分支与 `OpenedFile` 的来源枚举，换来上面全部省掉；owner 的判断是对的。

## 待决问题

- **语言**：文档用英文还是中文，还是两套？ADR 是中文、提示词是英文、getrad.dev 双语。两套体积翻倍；一套英文、模型按用户语言当场翻译是最省的。建议一套英文；由 定。
- **`radian docs [topic]` 子命令**：页面不在磁盘上，`cat` 看不到；没有模型时想在终端里看就靠它，成本低；要不要做。
- **路径写法**：`radian://docs/<page>.md` 还是别的拼法；要点是不能像一个真实文件路径。
- **谁写 `docs/user/`**：这是这件事的主要成本；写之前先定页面清单与语言。

## 验收

- 干净 `HOME`：启动前后 `~/.radian/` 没有任何与文档有关的文件（`ls -R` 快照相同）。
- `full` 与 `workspace` 两档下 `read radian://docs/commands.md` 返回页面，offset/limit/max_bytes 与普通文件行为一致；`list radian://docs/` 列出全部页面；`read radian://docs/nope.md` 的错误列出可用页面；`grep`/`glob` 以虚拟根为范围时拒绝并指向 `read`；`write`/`patch radian://docs/x` 拒绝。
- 系统提示：`<runtime>` 含文档段，两次构建逐字节相同，≤ 300 tokens；`--system` 下仍在；索引里每条路径都能被 `read` 打开（测试逐条读一遍）。
- pty 测试：问「how do I switch models?」，mock 模型对 `radian://docs/...` 调一次 `read`，工具结果含该页内容（证明提示词里的路径端到端可读）。
- 文档与代码同步测试：TUI `HELP` 里的每个 `/command` 出现在 commands 页；`<runtime>` 工具列表里的每个工具名出现在文档里；`rg 'ADR|slice' docs/user` 为空；内嵌总量 ≤ 512 KiB。
- 落地时同分支 docs-only commit 改 ADR 0039 §4、0036 的层表，并删掉本文件。
