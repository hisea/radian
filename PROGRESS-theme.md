# PROGRESS theme (ADR 0029) — WIP, 上下文压缩前落盘

工作区 `<home>/Code/radian-pi-theme`，分支 `pi/theme`，base `7b2b911`（main）。
brief: `<scratch>`。
本文件在完成时删除（或最后不提交）；现在为了压缩后能续做而提交。

## 已完成

**只有调研，还没有代码改动。** `git status --short` 在写本文件前是空的；本提交只加本文件。

## 已核实的事实（带文件:行号）

- ADR：`docs/adr/0029-theme-files-pi-compatible.md`（51 行），决定 1–7 与「测试要求」都读过了。
  冲突时以 ADR 为准。
- `crates/radian-tui/src/theme.rs`（现状，约 300 行）：
  - `ThemeName { Dark, Light, Mono }`（`:11`），`parse(&str) -> Option<Self>`（`:18`），
    `as_str`（`:28`），`Default = Dark`。
  - `Theme`（`:37`）：`name: ThemeName` + **22 个 `Style` 角色**（`:41-63`）：
    `reasoning, interrupted, user, tool_call, tool_result, tool_result_error, notice,
    turn_idle, turn_running, turn_cancelled, model, tier, usage, mcp, budget, budget_over,
    note, truncation, approval_border, hint, completion, find`。
  - `style(color: Option<Color>, modifiers) -> Style`（`:65`）。
  - `Theme::dark()`（`:81`）、`light()`（`:118`）、`mono()`（`:155`）；
    `for_name`（`:190`）、`resolve(name: Option<ThemeName>, no_color: bool)`（`:200`，
    `no_color` → `mono`）、`is_mono`（`:208`）、`no_color_env()`（`:213`，
    `NO_COLOR` 非空即真）、`styles() -> [Style; 22]`（`:219`）。
  - `dark` 由 TestBackend 截图测试逐字节钉住（brief 说必须不变）。
- `crates/radian-host/src/config/mod.rs`：
  - `TuiConfig`（`:178`）：`theme: Option<String>`（`:179`）、`keybindings: Vec<(String,
    Vec<String>)>`、`mouse: Option<bool>`、`input_style: Option<String>`；`Config.tui`（`:206`）。
  - `parse_tui`（`:425`）；theme 分支（`:432-439`）：只接受 `dark|light|mono`，否则
    `config.skipped.push("tui.theme")`。
  - 既有测试 `an_unknown_profile_is_refused_and_an_unknown_theme_is_ignored`（`:764`）断言
    `theme = "future"` → `config.tui.theme == None` 且 `skipped` 含 `tui.theme`。
    **这条测试要跟着改**（改成"未知名字只有在主题目录里也没有时才警告回退"）。
- `crates/radian-host/src/home.rs`：`config_dir_for`/`cache_dir_for`/`sessions_dir_for`/
  `memory_dir_for`/`credentials_dir_for`/`diagnostics_dir_for`/`catalog_cache_for`
  （`:38-60`）；公开的 `config_dir()`/`cache_dir()`/`sessions_dir()`/`memory_dir()`/
  `credentials_dir()`/`diagnostics_dir()`（`:68-99`）；`fn private(path)`（`:63`）用
  `radian_session::ensure_private_dir` 建 0700。**要加 `themes_dir_for(root)` 与
  `themes_dir()`**（照 `memory_dir_for` 的写法）。
- pi 主题样例＼�MIT；调色板来自 kanagawa.nvim，MIT）：
  `<home>/.pi/agent/git/github.com/hisea/pi-deck/themes/kanagawa-wave.json`。
  顶层 `$schema`/`name`/`vars`/`colors`/`export`；**51 个 colors**，全是变量名引用（`"fg"`、
  `"line"` …），`vars` 全是 hex；`export` 三个 hex。要**复制**到
  `crates/radian-tui/tests/data/themes/kanagawa-wave.json`，同目录 README 写来源与许可。
- pi 主题 schema：`<home>/.bun/install/global/node_modules/@earendil-works/pi-coding-agent/dist/modes/interactive/theme/theme-schema.json`（361 行）：
  - 顶层 `required = ["name","colors"]`；`properties` = `$schema,name,appearance,vars,colors,export`。
  - `colors.required` = **51 个角色**（列在下面）；`properties` 有 **56** 个（多 5 个可选）；
    `additionalProperties: false`。
  - `$defs.colorValue` = `oneOf`[string, integer 0..255]。
  - 56 个角色：`accent, border, borderAccent, borderMuted, success, error, warning, muted,
    dim, text, thinkingText, selectedBg, scrollbarTrack, scrollbarThumb, searchMatchBg,
    searchMatchText, userMessageBg, userMessageText, customMessageBg, customMessageText,
    customMessageLabel, toolPendingBg, toolSuccessBg, toolErrorBg, toolTitle, toolOutput,
    mdHeading, mdLink, mdLinkUrl, mdCode, mdCodeBlock, mdCodeBlockBorder, mdQuote,
    mdQuoteBorder, mdHr, mdListBullet, toolDiffAdded, toolDiffRemoved, toolDiffContext,
    syntaxComment, syntaxKeyword, syntaxFunction, syntaxVariable, syntaxString, syntaxNumber,
    syntaxType, syntaxOperator, syntaxPunctuation, thinkingOff, thinkingMinimal, thinkingLow,
    thinkingMedium, thinkingHigh, thinkingXhigh, thinkingMax, bashMode`。
  - 5 个可选角色的回退（pi 文档 `themes.md`）：`scrollbarTrack←muted`、`scrollbarThumb←text`、
    `searchMatchBg←selectedBg`、`searchMatchText←text`、`thinkingMax←thinkingXhigh`。
- pi 文档 `<home>/.bun/install/global/node_modules/@earendil-works/pi-coding-agent/docs/themes.md`：
  六种写法 `#rgb`/`#rrggbb`、`oklch(L% C H)`、`okhsl(H S% L%)`、色号 0–255、变量名、`""`；
  变量可链式引用；pi 里缺失变量/循环引用 → **整个主题无效**（我们按 ADR 只让该角色回退）；
  `system` 是保留名、名字不能含 `/`；真彩色不可用时近似 256 色；OKLCH 超出色域要 gamut-map。
- 配置 fixture：`docs/schema/config/*.toml`（8 个）+ `docs/schema/check-config-fixtures.sh`
  （`python3 -I` + `tomllib`，`EXPECT` 字典手工同步）；`minimal.toml` 已有 `[tui]`
  段（`theme = "dark"`, `mouse`, `input_style`）。`docs/schema/config/README.md` 有 fixture 表。
- ADR 0008（`docs/adr/0008-*.md`）提到 `tui.*` 的行：`:22`（TUI 偏好一行）、`:92`（展示类键
  忽略+保留）、`:136`（未知值忽略+警告）、`:140`（`tui.theme`/`keybindings`/`mouse`/`input_style`）。
  **配置表要加 `[tui.colors]`。**

## 下一步（按 ADR 0029 决定 1–7，一步一提交）

1. **主题文件读取 + 变量解析**：新 `crates/radian-tui/src/theme/`（或 `theme.rs` 内加
   `theme_file` 模块）。`serde` 解析 pi JSON（`name`/`appearance`/`vars`/`colors`/`export`）；
   变量链式解析，循环引用/缺失只影响该角色并各警告一次。
2. **颜色换算**：hex、`oklch()`、`okhsl()`、色号、变量名、`""` → `Option<Color>`。
   OKLab/OKHSL 公式自己实现（**不加依赖**），超色域裁剪。`COLORTERM`（`truecolor`/`24bit`）
   检测真彩色，否则近似 256 色。换算与近似都要**纯函数**。
3. **映射表** pi 角色 → Radian 角色（ADR 第 3 条）：写进 `theme.rs` 文档注释 +
   `crates/radian-tui/README.md` 的表。ADR 0027/0028 的角色（`md*`、`toolDiff*`、`syntax*`、
   `thinking*`）**本片只解析并保存**（不绘制），别丢。
4. **容错**（ADR 第 4 条）：缺角色 → 按 `appearance`（缺省 `dark`）回退内建值；未知角色忽略；
   每类问题只警告一次；文件损坏 → 整体回退内建。都不阻止启动。
5. **`[tui.colors]` 覆盖**（ADR 第 5 条）：`TuiConfig` 加字段；键接受 Radian 与 pi 角色名；
   值 = 颜色（只前景）或表 `{fg,bg,bold,italic,dim,underline}`；写法同第 2 条。
   同时改配置 schema、fixture、ADR 0008 配置表。
6. **`NO_COLOR` 优先**（第 7 条）：`resolve` 的现有语义要保住（主题文件与覆盖都不能压过它）。
7. **README**：拷 pi 主题的一条 `cp` 命令；角色表；颜色写法。
8. **测试**：ADR「测试要求」每一条 + 一个 pty 用例（临时 `RADIAN_HOME` 下
   `tui.theme = "kanagawa-wave"`，屏幕上 `user` 角色的前景色 = fixture 里的 RGB）。
9. **变异**（记进 `crates/radian-tui/MUTATIONS.md`）：变量不解析、oklch 通道错位、缺角色不回退
   （变无色）、`[tui.colors]` 不生效、`NO_COLOR` 被压过、内建 `dark` 被改动。
10. **Verify 全套**（fmt/clippy/workspace/journal-selfcheck/两套 fixtures/两套 contract/MSRV 1.88），
    pty ×3，提交前 `git status --short` 为空。

## 与并行 p3 的边界

p3（`pi/0b`）改两个宿主的启动参数、`radian-host` 的配置读取（删 `model.api_key`）和程序改名。
两边都会动 `crates/radian-host/src/config/mod.rs` 但不同段；谁后合并谁解冲突。我这边只加
`tui.colors` 解析与 `tui.theme` 名字放宽，不碰 p3 的段。
