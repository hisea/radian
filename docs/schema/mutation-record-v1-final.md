# 本次收尾的变异检查记录（0006 定稿 + config schema v1）

日期：2026-10-12 · 基线：`3e82250`（工作树 `/tmp/schema2-mut`，一次性检出，已删除）
方法：每个变异改完（脚本或 fixture），运行对应校验脚本期望**非 0 退出**；改完立即 `git checkout --` 还原。收尾时 `git status --short` 为空。

| # | 变异 | 脚本 | 结果 | 被什么抓住 |
|---|---|---|---|---|
| M1 | `check-fixtures.sh`：`TranscriptReplaced` 分支里删掉 `index_next = 0`（index 不归零） | `check-fixtures.sh` | ✅ 失败 | `replace-resets-index.jsonl`：`line 6: MessageAppended.index 0 != transcript 长度 2` |
| M2 | `replace-resets-index.jsonl`：`TranscriptReplaced.len` 2→3（与真实后续消息数不符） | `check-fixtures.sh` | ✅ 失败 | 文件末尾 `TranscriptReplaced.len 3 != 其后的 MessageAppended 数 2` |
| M3 | `check-fixtures.sh`：删除 `messages_since_replacement += 1`（follow 计数不递增） | `check-fixtures.sh` | ✅ 失败 | `TranscriptReplaced.len 2 != 其后的 MessageAppended 数 0` |
| M4 | `check-config-fixtures.sh`：`PLAINTEXT_PREFIXES` 去掉 `"sk-"` | `check-config-fixtures.sh` | ✅ 失败 | `heuristic-secret.toml`：非敏感键的 `sk-…` 漏网 → “期望拒绝但未检出明文” |
| M5 | `check-config-fixtures.sh`：删除整个「低版本」分支（fall through 到 accept） | `check-config-fixtures.sh` | ✅ 失败 | `schema-version-lower.toml`：`schema_version 0 != v1`（版本校验兜住） |
| M6 | `check-config-fixtures.sh`：`SECRET_FROM` 去掉 `"env"` | `check-config-fixtures.sh` | ✅ 失败 | `minimal.toml`：`SecretRef.from` 必须 `env\|keychain\|file` |
| M7 | `check-config-fixtures.sh`：`SENSITIVE_KEYS` 去掉 `"api_key"` | `check-config-fixtures.sh` | ✅ 失败 | `plaintext-plain.toml`：非启发式明文 `api_key = "super-secret"` 靠名字规则拒绝 → 规则失效后漏网 |
| M8 | `minimal.toml`：`api_key` 改成明文（合法 fixture 被破坏） | `check-config-fixtures.sh` | ✅ 失败 | `model.api_key` 是裸字符串 → 拒绝 |

8/8 全部被抓住。

## 初版无效变异（自己的变异写错，不是测试漏洞）

初次尝试时两条变异未按预订被抓，均已修正后再验证（与 `mutation-record.md` 里 M4/M7 的处理方式一致）：

- **M4-初版**：「删除启发式前缀判断」只改了值前缀逻辑，但 `plaintext-secret.toml` 先被「敏感键是裸字符串」规则拦截，前缀启发式当时**没有任何 fixture 覆盖**。修正：新增 `heuristic-secret.toml`（非敏感键里放 `sk-…`），使启发式可观察；M4 修正版被抓住。
- **M5-初版**：把低版本分支整段替换成无条件 PASS——这是无效变异（行为被夸大到恒过）。修正：改为删除分支、fall through 到 accept；修正版被版本校验抓住。
- **M7-初版**：同样漏网，原因同上（名字规则没有非启发式明文 fixture）。修正：新增 `plaintext-plain.toml` 让「敏感键 + 非启发式明文」可观察；M7 修正版被抓住。

结论：一次变异检查发现了两类**真实的覆盖空洞**（启发式前缀规则、敏感键名字规则都缺乏独立 fixture），已分别补上；这不是为了凑数，是这两条规则本就声明为独立防线（ADR 0008 §4.2 的两条规则）。

## 备注

- 「写回必须保真或拒绝」是写入方行为（ADR 0008 §5.4），脚本只验读取侧接受/忽略；写回保真由 Phase 1 实现并自带测试（与 README 手工同步声明一致）。
- fixtures 全部用占位路径/占位主机（`/work/example`、`example.com`），无真实密钥。
## 评审补充（合并时由评审者追加）

评审者另做了 4 个变异，2 个被抓住、1 个是等价变异、1 个是已知且已记录的局限：

| 变异 | 结果 | 判定 |
|---|---|---|
| `replace-resets-index.jsonl` 第 2 行删掉信封必需字段 `critical` | ✅ jsonl 脚本退出 1 | 新 fixture 确实被信封校验覆盖 |
| 把 `check-config-fixtures.sh` 里"敏感键不得是裸字符串"的 `raise` 改成 `pass` | ❌ 仍退出 0 | **等价变异**，不是漏洞：`plaintext-plain.toml`（`api_key = "super-secret"`，不含 `sk-` 前缀）仍被**另一条独立规则** `check_secret_refs`（§4.1 要求敏感键位必须是 SecretRef 表对象）拒绝。两条规则是有意的双层防御，去掉任一层另一层仍然生效。若要让单层也可被变异捕获，需要给期望断言加上"拒绝原因"的匹配（当前 `EXPECT` 只断言 `outcome="reject"`）——**这是一条可选的加强项，不是缺陷**。 |
| 把 `config/README.md` 里的「拒绝」全改成「接受」 | ❌ 仍退出 0 | 已知局限，与 `fixtures/README.md` 同一类：脚本读 `EXPECT` 不读散文表格。两份 README 都已写明"必须手工同步"。 |
