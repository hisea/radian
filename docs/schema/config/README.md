# radian 配置 schema v1 fixtures（ADR 0008）

每个文件一个场景。**本表是给人读的；脚本的期望写在 `../check-config-fixtures.sh` 的 `EXPECT` 字典里，脚本不读本表**——改动任何一处必须同步改另一处（同 `docs/schema/fixtures/` 的做法）。读取方 = 支持 `schema_version = 1` 的 v1 实现（ADR 0008 §5）。

**评审补充（合并时追加）**：评审者验证过这条漂移风险是真的——只把本表里的「拒绝」改成「接受」，脚本仍然通过（它读自己的 `EXPECT`）。两处必须手工同步。

| 文件 | 场景 | 读取方期望结果 | 对应 ADR 0008 |
|---|---|---|---|
| `minimal.toml` | 最小合法配置：model 路由（provider/base_url/model/`include_usage`）、一个 MCP server、权限档位、TUI 偏好。key 只从 `auth.json` 来，配置里没有 `api_key`（ADR 0026 D2）。 | **接受**，0 拒绝；全文无明文密钥样字符串。 | §2 / §4 |
| `v1-unknown-fields.toml` | `schema_version = 1`，带未知顶层字段 `future_top_level` 与未知嵌套字段 `model.future_option`。 | **接受**（解释时忽略）；写回必须保真（保留未知字段）或拒绝写回——“忽略但写回时丢弃”被禁止（它会悄悄删掉用户为新版本写的配置）。 | §5.3 / §5.4 |
| `schema-version-newer.toml` | `schema_version = 99`（高于 v1）。 | **只读打开或拒绝打开**，**绝不能以 v1 语义写入**。 | §5.2 |
| `schema-version-lower.toml` | `schema_version = 0`（低于 v1，无 v0→v1 注册迁移路径）。 | **拒绝**；**绝不静默改写用户文件**；迁移只能由显式入口发起。 | §5.3 |
| `tui-theme-colors.toml` | ADR 0029:`tui.theme` 为主题文件名(内建之外的名字指向 `~/.radian/themes/<name>.json`),`[tui.colors]` 逐角色覆盖(颜色、色号、表三种写法,含 pi 角色名)。 | **接受**,0 拒绝;读者原样携带,由 TUI 解析;无明文密钥样字符串。 | §2 / ADR 0029 |
| `tui-display.toml` | ADR 0028 R2/R3/R4/R8:`[tui.display]` 的 `reasoning`/`tool_output`/`tool_preview_lines`/`mermaid`/`highlight`。 | **接受**,0 拒绝;未知值/未知键是警告(展示偏好)。 | §2 / ADR 0028 |
| `tui-tips-banner.toml` | ADR 0008：`tui.tips`/`tui.banner`（布尔，默认都开）关闭启动画面的 Tip 行与字标。 | **接受**，0 拒绝；无明文密钥样字符串。 | §2 |
| `tui-brand.toml` | ADR 0027：`tui.brand` 是品牌文字（≤ 4 显示宽度），footer 的 `brand` 条目与启动画面一行回退共用；不写时用 glyphs 档默认；超出是配置错误。 | **接受**，0 拒绝；无明文密钥样字符串。 | §2 / ADR 0027 |
| `tui-spinner.toml` | ADR 0027：`tui.spinner = "pulse" | "arc" | "quadrant" | "braille"`（默认 pulse；未知值警告回退）。 | **接受**，0 拒绝；无明文密钥样字符串。 | §2 / ADR 0027 |
| `tui-input-style.toml` | ADR 0035 V3：`tui.input_style = "bar" | "lines" | "box"`（默认 `bar`；未知值警告回退）。 | **接受**，0 拒绝；无明文密钥样字符串。 | §2 / ADR 0035 V3 |
| `tui-footer.toml` | ADR 0027 C2：`[tui.footer]` 的 preset/separator/glyphs 与 `[[tui.footer.rows]]` 的 left/right（字符串或表两种条目写法、icon/format/style/priority/thresholds 与条目专属参数）。 | **接受**，0 拒绝；未知条目 id 由 TUI 原样显示并警告，不拒绝文件。 | §2 / ADR 0027 |
| `tui-footer-items.toml` | ADR 0027：`[tui.footer.items.<id>]` 的 symbol/style/symbol_style/format/disabled，`separator_style`（角色或 starship 颜色），`tui.usage_period_secs`。 | **接受**，0 拒绝。 | §2 / ADR 0027 |
| `tui-widgets.toml` | ADR 0027 C3：`[tui.widgets]` 的 `above_input`/`below_input` 在输入框上/下放一行贡献者条目（条目 id）。 | **接受**，0 拒绝。 | §2 / ADR 0027 |
| `providers.toml` | ADR 0026 D10:`[providers.<名字>]` 具名 provider(`kind` + `base_url`),`model.provider = "<名字>"` 选用。ADR 0014 Q2:同名表下 `reasoning_replay` 可覆盖目录的推理回传字段(字段名串,或 `false` 关掉)。 | **接受**，0 拒绝；名字非法/与内建重名是另一个拒绝场景（读取器报 `providers.<name>` 错误）。 | §2 / ADR 0026 D10 / ADR 0014 Q2 |
| `permissions-approval.toml` | ADR 0026 D9:`[permissions] approval = "auto"`(四个已知名字之一)。`full` 档位下这个值被忽略并警告;命令行 `--approval`/`--yes` 永远优先。 | **接受**,0 拒绝;无明文密钥样字符串。 | §2 / ADR 0026 D9 |
| `skills-builtin.toml` | ADR 0025:`[skills] builtin = false` 关掉全部内建 skill,`disabled = ["<name>"]` 关掉单个(用户级/extra_paths 不受影响)。 | **接受**,0 拒绝;未知值/未知键是警告。 | §2 / ADR 0025 |
| `integrations-herdr.toml` | ADR 0034 H6:`[integrations] herdr = true|false`,缺省 `true`。只在 herdr pane 里生效(`HERDR_ENV=1` 且 pane id、socket path 非空);`RADIAN_HERDR=0` 一次性关闭。上报只有状态、会话 id、journal 路径与恢复命令,不含 transcript/usage/model/费用/命令正文。 | **接受**,0 拒绝。 | §2 / ADR 0034 |
| `sandbox-unix-sockets.toml` | ADR 0016:`[sandbox] allow_unix_sockets` 列出沙箱开网时允许连接的 Unix socket(其余出站 Unix socket 全拒;mDNSResponder 固定放行做 DNS)。只认绝对路径(宿主展开 `~`),canonicalize 失败只警告一次并跳过。**只能写在用户级配置**;今天没有项目级配置层。 | **接受**,0 拒绝;无明文密钥样字符串。 | §2 / ADR 0016 |
| `skills-extra-paths.toml` | ADR 0025 片4:`[skills] extra_paths` 列出额外的 skill 根(裁决 2:`~/.claude/skills` 只有列出时才读)。读取器原样保留字符串(不碰文件系统);宿主展开 `~`、相对路径相对**配置文件所在目录**。不存在的目录只警告一次,不报错。 | **接受**,0 拒绝;无明文密钥样字符串。 | §2 / ADR 0025 片4 |
| `api-key-removed.toml` | `model.api_key` 是**合法**的 `SecretRef`（`{ from = "env", name = "EXAMPLE_TOKEN" }`）。 | **拒绝**（ADR 0026 D2）；提示改用 `~/.radian/credentials/auth.json`，不静默忽略。 | §4.2 / ADR 0026 D2 |
| `plaintext-secret.toml` | `api_key` 是明文串 `sk-live-…`。 | **拒绝打开**；报错点名键名/路径（敏感键被赋裸字符串，或明显的密钥样字符串出现在配置里）。 | §4.2 |
| `heuristic-secret.toml` | 明文串出现在**非敏感键**（`tui.status_template` = `sk-heuristic-…`）。 | **拒绝打开**；启发式前缀扫到即拒——再次证明"出现在任何位置"不是只查敏感键。 | §4.2 |
| `plaintext-plain.toml` | 非启发式明文（`api_key = "super-secret"`，无前缀、短串）。 | **拒绝打开**；靠"敏感键不得是裸字符串"这一条规则，不依赖启发式前缀。 | §4.2 规则 1 |

通用约束（脚本逐项校验）：`schema_version` 必须存在且为非布尔整数；接受类 fixture 必须通过明文密钥检测与 SecretRef 形状校验；所有路径/主机是占位符（`/work/example`、`example.com`），无真实密钥。