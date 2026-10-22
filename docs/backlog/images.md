# 图像输入：`read` 读图、`@` 贴图、剪贴板贴图、`exec --image`

- 状态：idea（设计已有推荐，六个问题待 owner 定；定了即可派发）
- 涉及 ADR：0015（估算规则）、0018（模型目录与压缩占位）、0028（transcript 显示）、0039 §4（读根）、0020（`~/.radian/` 布局，仅当选剪贴板落盘方案）、0032（`radian exec` 参数）、0043（subagent 工具）
- 来源：owner 2026-10-22 提出；对照 pi 1.3.0 已装包（`dist/core/tools/read.js`、`dist/modes/interactive/interactive-mode.js`）核对

## 背景

**已经有的**（不用动）：

- `ContentPart::Image { mime_type, data(base64) }` 在 `radian-core`，user / assistant / tool 消息都能带。
- 三个 provider adapter 都已映射：Anthropic → `image` base64 block；OpenAI-compatible → `image_url` data URL；Responses → `input_image`。不支持的 part 变 `[<mime> image omitted]` 占位文本。
- MCP 工具结果里的 `image` block 已经变成 `ContentPart::Image`（`agent-extensions::session`）。
- journal 原样存 `Message`，base64 进 JSONL；ADR 0006 建议单行 ≤ 16 MiB。
- `--export` 和 `exec` jsonl 的文本投影已把图当 `[image omitted]`。

**缺的**，也就是「图怎么进来、进来之后怎么算、怎么显示」：

1. `read` 工具遇到非 UTF-8 一律 `not_utf8` 拒绝，没有图的分支。
2. TUI `@path` 指到二进制文件时整条消息「unusable attachment」不让发。
3. 没有剪贴板贴图；没有 `radian exec` 带图的入口（arc worker、subagent 截图回传都没法用）。
4. **预算估算会炸**：ADR 0015 C2 把整条消息 JSON 的字节数 ÷ `bytes_per_token`，base64 算进去。一张 1 MB 的 PNG ≈ 1.4 MB base64 ≈ 35 万「token」，远超 200k 窗口，自动压缩立刻触发，之后每一轮都在压缩。真实计费：Anthropic ≈ 宽×高÷750，1568 px 长边封顶约 1,600 token；OpenAI 按 512 px tile 计。这一条不做，其余都不能上线。
5. 模型目录（ADR 0018，models.dev 快照）只留了 `cost` 和 `limit`，没有保留 `modalities.input`，所以 radian 不知道当前模型能不能看图。
6. 压缩：用户原话逐字保留，但 `ContentPart::Image` 没有占位规则；旧的图留在历史里一直占 token。
7. TUI：用户消息的 `display_parts` 只认 `Text`，图 part 直接不显示；工具结果渲染器同样只看文本。

**同行做法**（已核对的标出处）：

| | 进来的路 | 读图工具 | 无视觉模型 | 缩放 |
|---|---|---|---|---|
| pi 1.3.0（已核对本机包） | Ctrl+V：剪贴板图写到 `os.tmpdir()` 临时文件，把路径插进编辑器；文件路径粘贴也插路径 | `read` 支持 jpg/png/gif/webp/bmp，返回 `[text 说明, image]` 两个 block | `read` 加一行 `[Current model does not support images. The image will be omitted from this request.]`，图不发 | `processImage` 按 `model.inputLimits.images.resize` 自动缩放，默认开 |
| Claude Code（按其文档，未重核） | 终端 Ctrl+V 贴图、拖拽文件路径 | `Read` 读 PNG/JPG 直接给模型看 | — | 服务端 |
| Codex（研究笔记 `2-tools.md`） | — | 单独的 `view_image` 工具，不混进 read | — | — |
| opencode（未核对） | `@` 附件可带图 | 待查 | 待查 | 待查 |

## 推荐设计（待定，每条都是一个 owner 决定）

**Q1 入口分两步落地。** v0 = `read` 读图 + TUI `@path` 贴图 + `radian exec --image PATH`（可重复）。v1 = Ctrl+V 剪贴板贴图。理由：v0 三条都只是「文件 → `ContentPart::Image`」，一条管道；剪贴板要读系统剪贴板（`arboard` crate，macOS / Linux / Windows 都有图像读取），是另一个依赖，单独一个 slice。

**Q2 剪贴板贴图不落盘。** pi 写临时文件再插路径，到 radian 这里有读根问题：workspace 档下 `read` 只能读工作区和 ADR 0039 §4 的只读根，`os.tmpdir()` 不在里面。推荐 host 直接拿剪贴板字节，当场做成 `ContentPart::Image` 挂在用户消息上，transcript 显示 `[image: clipboard, W×H, N KB]`。这样 ADR 0020 布局和 0039 读根都不用动。若 owner 偏要和 pi 一样插路径，就得给 `~/.radian/clipboard/` 一个 sandbox-readable 的位置（改 0020）。

**Q3 `read` 返回形状跟 pi 一样：** 一个文本 part（`Read image file [image/png] 1200×800`）加一个 `Image` part。不另开 `view_image` 工具（Codex 的做法多一个工具定义，每轮多付字节，和 0043 §4 的工具数也要再谈）。识别靠魔数（PNG / JPEG / GIF / WebP），不靠扩展名；BMP 不收（Anthropic 不收 BMP）。`read` 的 `offset`/`limit` 对图无意义，传了就报错。读根规则不变：图和文本走同一个 `RootDir`。

**Q4 限额和缩放。** v0 不重编码：单图原始字节 ≤ 5 MiB（Anthropic API 的上限；OpenAI 更宽），像素 ≤ 8000×8000（Anthropic 上限），长边 > 1568 的只在 `read` 的说明文本里提示「服务端会缩放」。读尺寸只解头部（`imagesize` crate，几百行，无 decode）。超限 → `read` 返回错误 `image_too_large`，`@` 返回 unusable。v1 再决定要不要像 pi 一样本地缩到 1568（要 `image` crate 的 decode/encode，编译时间和二进制体积都要量）。

**Q5 估算规则（ADR 0015 C2 加一条）。** `ContentPart::Image` 不按字节算，按像素：`tokens = ceil(min(w, 1568-缩放后的 w) × h ÷ 750)`，没有尺寸时按 1,600 固定。OpenAI 的 tile 规则差别在 2 倍以内，用同一条，`/context` 单独列一行「images」。这条是硬前提，放第一个 slice。

**Q6 模型能力与压缩。**
- 目录快照保留 models.dev 的 `modalities.input`；模型不收图时，`read` 跟 pi 一样加一行说明并**不发**图 part（provider 的 `[image omitted]` 占位是兜底，不是主路径）。目录里没这个模型时视为支持。
- 压缩（ADR 0018）：用户原话里的图变一行 `[image: W×H]`，和附件 `[file: P, N lines]` 一个形状；工具结果里的图随结果一起进摘要。不做「旧图自动换占位」（那是 `context-pruning.md` 的事，等它定规则时把图算进去）。

**Q7 拖拽与 Mac 截屏（owner 2026-10-22 问）。** 两条路机制不同：

- 拖拽（Finder 文件、截屏后的浮动缩略图）：终端只把路径当文本插进输入框（Terminal.app、iTerm2、kitty、Ghostty 都这样，转义写法不同：反斜杠转义空格或整段加引号，常带结尾空格），图片字节不经过终端。composer 规则：光标处一个指向**现有图片文件的绝对路径**（魔数判定）就变成附件，显示 `[image: <文件名>, W×H, N KB]`，Backspace 删掉。路径在工作区外也收：由 host 直接读字节，模型拿到图不拿路径，不经过沙箱，0039 读根不动。并进 v0，和 `@path` 同一条管道。
- 剪贴板截屏（Cmd-Ctrl-Shift-4、预览里 Cmd-C）：Cmd-V 由终端处理，剪贴板是图像时什么都不发，所以 pi 和 Claude Code 都绑 Ctrl-V。radian 自己读系统剪贴板（`arboard`：macOS NSPasteboard，Linux X11/Wayland），键绑定建议 Ctrl-V，读到字节当场做 `Image` part，不落盘（同 Q2）。这就是 Q1 的 v1；owner 要可并进 v0。
- 限制：SSH 到远端跑 TUI 时拖拽插的是本机路径，远端不存在，留作文字并提示；剪贴板读的是远端的，读不到。tmux / herdr 里两条路都透传，正常。exec worker 不受影响，走 `--image PATH`。

**Q8 transcript 里显示图（已定，owner 2026-10-22）：用 `ratatui-image`。** 核对于其仓库 master（12.0.0-rc.3；最新正式版 11.1.0，要求 ratatui ^0.30.1）：

- 协议按它的探测走：kitty 和 Ghostty 用 kitty unicode placeholder（图绑在占位字符上，随滚动、折叠一起走）；iTerm2、WezTerm 用 iTerm2 协议（WezTerm 的 kitty / sixel 实现被作者拉黑）；foot、xterm 等用 sixel；Terminal.app、Alacritty、Konsole、Warp 等退到半块字符缩略图。radian 不自己写协议，也不挑终端。
- feature 必须是 `--no-default-features --features image-defaults,crossterm`：默认的 `chafa-dyn` 动态链接 libchafa，运行机器没有就起不来。
- 它依赖 `image` crate 解码，和 Q4 的本地缩放共用一份依赖，两件事一个 slice。
- 探测要写控制序列到终端并读回答，必须在事件循环读 stdin 之前做；和 `terminal.rs` 现有的 kitty 键盘协议探测合成一次。
- 在 tmux（herdr）里它会自己 spawn `tmux set -p allow-passthrough on` 改当前 pane 的选项，然后穿透查询真实终端。接受这个副作用，手册写明。
- 显示规则（0028 加一节）：图是 block 里一段固定高度的行（高度常量，按字号换算像素；宽按比例，超宽缩到列宽），上面一行仍是 `[image: <来源>, W×H, N KB]` 占位行。折叠态只剩占位行。选择复制、Ctrl-F、`--export` 只看占位行，不看图。journal 不变。
- 顺序：先 Q5 估算，再 read / `@` / 拖拽 / `exec --image` 管道，再这个渲染 slice，最后剪贴板。

## 涉及的输入（手册要写）

- `read` 工具：描述改成「text file, directory, or image」；新错误码 `image_too_large`、`image_offset`。
- `@path`：指向 PNG / JPEG / GIF / WebP 时附图；限额两个常量。
- `radian exec --image <PATH>`：可重复，和 `-p` 一起用；超限退出码按 0032 的参数错误。
- 键：剪贴板贴图的键绑定（v1）。
- 没有新的配置键；限额是常量，手册照常写数值。

## 验收

- `read` 一张 PNG：工具结果两个 part，Anthropic 请求里出现 `image` block；非视觉模型（目录 `modalities.input` 无 `image`）只出现说明文本。
- `@shot.png` 发送后 transcript 显示 `[image: shot.png, W×H, N KB]`，journal 回放一致，`--export` 为 `[image omitted]`。
- `radian exec -p "describe" --image a.png` 的 jsonl 里 user 消息带 `image` part。
- 估算：带一张 1 MB PNG 的会话，`/context` 的 images 行 ≤ 1,700 token，不触发自动压缩；压缩后用户原话里是 `[image: W×H]`。
- 超限：6 MiB 的文件 `read` → `image_too_large`；`@` → unusable 且消息不发。
- 三个 provider contract suite 不动（映射早有）；`docs/user` 的 tools、commands、cli 页各加一节；drift test 读两个限额常量。
