# `grep`/`glob` 接住「一个 `path` 里写了几个目录」

- 状态：decided＼�做 C + A；不改 schema、不引入 rg 二进制）
- 涉及 ADR：0039（fd 级遍历与读根不变）、0043 §4（工具描述字节）、0037（每条事实只说一次）
- 来源：在 dogfood pane 里看到 `grep` 报 `crates/radian-session crates/radian-host …: No such file or directory`

## 背景（2026-10-20 核实）

- 命令行的 `rg`/`grep -r` 都接受多个路径参数，模型照这个习惯把几个目录用空格拼进 `grep` 的 `path`。本机全部 journal 里 grep+glob 共 1,935 次调用，4 次是这种写法（3 个会话，都是最近的）；返回只有 `No such file or directory (os error 2)`，看不出是 `path` 只收一个路径。
- `grep`/`glob` 已经建在 ripgrep 的库上（`regex`、`ignore` 的 gitignore 匹配、`globset`）；只有遍历是自己的 fd 级实现（`crates/radian-coding/src/search.rs`，ADR 0039 决定 2），所以不调用 `rg` 二进制：那会按路径遍历，丢掉 0039 的保证，也依赖用户装了 rg。
- 解析起点在 `grep.rs` 的 `root.read_base(path)`（目录）与 `root.open_read`（单个文件）；glob 同形。

## 决定（已定）

- **底层支持多个起点（C 的核心）**：一次调用可以有 N 个起点，每个起点照今天的规则各自解析（工作区内相对路径，或只读根下的绝对/`~` 路径；目录或单个文件）、各自带上自己的祖先 ignore 文件，然后把找到的文件合成一个集合再搜索。**每个起点都搜**，不是只搜第一个。同一个文件只出现一次（起点重叠时，例如 `crates` 与 `crates/x`）。`max_matches`、输出字节上限、`files`/`count` 模式都按整次调用算，不是按起点各算一份。结果排序与今天一致（按显示路径）。
- **C：按空白拆分后兜底**：只有在整个 `path` 字符串作为一个路径**不存在**、且它含空白时才拆：按空白拆成几段，每段都按上面的规则解析成功，就当成 N 个起点去搜，结果第一行注明 `[searched N paths: a, b, c]`。整串本身存在（真有带空格的目录名）时照旧当一个路径，不拆。
- **A：拆了也不成立时给提示**：拆开后有任何一段不存在，报错写明哪几段不存在，并提示 `path` takes one directory or file; to search several, call the tool once per path in the same reply（措辞按 ASD-STE100）。不含空白的不存在路径，报错照旧。
- 不改 `path` 的 schema（仍是一个字符串），不改工具描述，提示词字节不变。

## 验收

- `path: "crates/a crates/b"`（两个都存在）：两处的匹配都返回，第一行有 `[searched 2 paths: …]`；`max_matches: 3` 时总数是 3，不是每处 3。
- 起点重叠（`crates` 与 `crates/a`）：同一匹配只出现一次。
- 真实存在的带空格目录 `"my dir"`：当一个路径搜，不拆。
- `"crates/a nope"`：报错点名 `nope` 并带提示；`"nope"`：报错照旧。
- 混合工作区路径与只读根路径时，结果各用各的显示前缀。
- glob 同样的四条。
- 变异：只搜第一段 → 第一条测试失败；上限按起点各算 → `max_matches` 那条失败；去掉去重 → 重叠那条失败；整串存在也拆 → 带空格目录那条失败。
