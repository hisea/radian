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
| `tui-tips-banner.toml` | ADR 0008：`tui.tips`/`tui.banner`（布尔，默认都开）关闭启动画面的 Tip 行与字标。 | **接受**，0 拒绝；无明文密钥样字符串。 | §2 |
| `api-key-removed.toml` | `model.api_key` 是**合法**的 `SecretRef`（`{ from = "env", name = "EXAMPLE_TOKEN" }`）。 | **拒绝**（ADR 0026 D2）；提示改用 `~/.radian/credentials/auth.json`，不静默忽略。 | §4.2 / ADR 0026 D2 |
| `plaintext-secret.toml` | `api_key` 是明文串 `sk-live-…`。 | **拒绝打开**；报错点名键名/路径（敏感键被赋裸字符串，或明显的密钥样字符串出现在配置里）。 | §4.2 |
| `heuristic-secret.toml` | 明文串出现在**非敏感键**（`tui.status_template` = `sk-heuristic-…`）。 | **拒绝打开**；启发式前缀扫到即拒——再次证明"出现在任何位置"不是只查敏感键。 | §4.2 |
| `plaintext-plain.toml` | 非启发式明文（`api_key = "super-secret"`，无前缀、短串）。 | **拒绝打开**；靠"敏感键不得是裸字符串"这一条规则，不依赖启发式前缀。 | §4.2 规则 1 |

通用约束（脚本逐项校验）：`schema_version` 必须存在且为非布尔整数；接受类 fixture 必须通过明文密钥检测与 SecretRef 形状校验；所有路径/主机是占位符（`/work/example`、`example.com`），无真实密钥。