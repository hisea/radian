# Session JSONL v1 fixtures

每个文件一个场景。**本表是给人读的；脚本的期望写在 `../check-fixtures.sh` 的 `EXPECT` 字典里，脚本不读本表**——改动任何一处必须同步改另一处，否则两边会悄悄漂移（评审时已验证：只改本表，脚本仍然通过）。读取方 = 支持 `schema_version = 1` 的 v1 实现（ADR 0006 §7）。

| 文件 | 场景 | 读取方期望结果 | 对应 AC |
|---|---|---|---|
| `v1-minimal.jsonl` | 只含 Phase 1–5 字段；走完一个完整 turn，含一次工具调用（`ToolExecutionStarted` → `ToolExecutionSettled` → 结果消息）。9 个事件。 | **接受**，0 跳过；`MessageAppended.index` 为 0..3 连续；无未配对 tool call（repair 为空）。 | AC-S-1 / AC-S-4（后向样本） |
| `v1-forward.jsonl` | `SessionCreated` 带 AC-S-2 **全部**预留可选字段（`agent_id`、`profile_ref`、`workspace_binding`、`parent`），外加一个未来的**非 critical** 事件 `HeartbeatRecorded`。6 个事件。 | **接受**；跳过并记录 `(seq=3, HeartbeatRecorded)`；预留字段按未知可选字段忽略（AC-S-3a）；其余 5 个事件完整重建。 | AC-S-2 / AC-S-4（前向样本） |
| `unknown-optional-field.jsonl` | 已知事件 `MessageAppended` 的 `data` 里多一个读取方不认识的字段 `display_hint_v2`。4 个事件。 | **接受**，0 跳过；未知字段忽略。 | AC-S-3(a) |
| `unknown-event-noncritical.jsonl` | 未知事件类型 `SpinProgressRecorded`，`critical:false`。5 个事件。 | **接受**；跳过并记录 `(seq=1, SpinProgressRecorded)`；其余 4 个事件重建。 | AC-S-3(b) |
| `unknown-event-critical.jsonl` | 未知事件类型 `LedgerCommitted`，`critical:true`。2 个事件。 | **拒绝打开**；错误必须点名：文件、行号（第 2 行 / seq=1）、事件名、读取方支持的 schema 版本。 | AC-S-3(b) |
| `unknown-enum-values.jsonl` | 已知事件里的**字段值**是未来枚举值：`InboxEnqueued.lane`、两条 `TurnFinished` 的 `outcome`/`reason`、`PermissionResolved` 的 `decision`/`approver`。6 个事件。 | **接受**，0 跳过；未知枚举值映射为 `Unknown`（ADR 0006 §7(d)），不报错、不跳过整行；无 `MessageAppended`，`replay` 为空、`repair` 为空。 | AC-S-3(d) / AC-S-4（枚举前向样本） |
| `schema-version-newer.jsonl` | 每行 `schema_version: 2`（高于 v1）。2 个事件。 | **只读打开或拒绝打开**（宿主二选一），**绝不能以 v1 语义追加写入**；追加模式必须直接报错。 | AC-S-3(c) |
| `truncated-tail.jsonl` | 5 个合法行 + 一个截断的尾部（无 LF 的半行）。 | **接受（丢弃尾部）**：torn tail 字节不解析、不计 `seq`、不进 replay；重放得到前 5 个事件（seq 0..4）；安全追加前必须先截断到第 5 行的 LF 之后。 | ADR 0006 §3.2 |
| `crash-after-started.jsonl` | assistant 声明两个调用：`call-1` 完整（Started+Settled+结果消息），`call-2` 只有 `ToolExecutionStarted`、无 Settled、无结果消息；无 `TurnFinished`。8 个事件。 | **接受**（文件本身合法）；按 ADR 0002 恢复规则，`call-2` 补 `error_code = "outcome_unknown_after_crash"` 的合成结果，`call-1` 原样；Phase 1 的恢复测试直接使用。 | ADR 0002 崩溃语义 / ADR 0006 §15-2 |
| `memory-injected.jsonl` | 一条非 critical 的 `MemoryInjected { records: [{id, revision, scope}] }`（ADR 0021 D6），两条记录：`global` 与 `{"project": "prj-1"}`。6 个事件。 | **接受**，0 跳过；事件按 id/revision/scope 原样往返，不带内容；不进 replay/repair。 | ADR 0021 D6 |
| `approval-policy-changed.jsonl` | 一个完整 turn 之后，一条 critical 的 `ConfigChanged { change: {"approval":"auto"} }`（resume 恢复所读的状态记录）和一条非 critical 的 `ApprovalPolicyChanged { policy: "auto" }`（ADR 0026 D9 的开关审计）。7 个事件。 | **接受**，0 跳过；`ApprovalPolicyChanged` 必须 `critical:false` 且按 `policy` 原样往返；两条都不进 replay/repair。 | ADR 0026 D9 |
| `fork-parent.jsonl` | 一个完整 turn（`TurnStarted` → 两条消息 → `TurnFinished`）之后一条非 critical 的 `SessionForked { child_session_id, parent_seq }`（ADR 0022）。6 个事件。 | **接受**，0 跳过；`SessionForked` 不进 replay/repair，`parent_seq` 与 `fork-child.jsonl` 的 `parent.seq` 一致。 | ADR 0022 §3 |
| `fork-child.jsonl` | 一个 fork 出来的子会话：`SessionCreated.parent = { session_id, seq }`（ADR 0022；ADR 0006 §6 的保留字段加 `seq`），随后两条物化的 `MessageAppended`（index 从 0 起）。3 个事件。 | **接受**，0 跳过；`parent` 按未知可选字段忽略，子会话的 history 就是这两条消息。 | ADR 0022 §2 |
| `compaction-verbatim.jsonl` | `compaction-committed.jsonl` 加上 ADR 0018 的 `origin.user_messages`：摘要消息的 `origin` 是 `{"kind":"compaction_summary","user_messages":["…","…"]}`（第二条含 `"` 与 `}`，用来钉住它是 JSON 字符串而不是被解析的正文）。9 个事件。 | **接受**，0 跳过；`origin` 是宿主/实现的不透明字段，其内部结构不由 schema 校验；读取方按 ADR 0018 只从 `origin.user_messages` 读回原话，正文不解析。 | ADR 0018 / ADR 0005 `origin` 脚注 |
| `replace-resets-index.jsonl` | 旧 revision 两条消息（user 带 `meta`，assistant 带 `interrupted:true`）→ `TranscriptReplaced{revision:1,len:2}` → 新 revision 从 index 0 重计；一条 `ModelUsageRecorded`；新 revision 的 `call-9` 有 `Started` 无 `Settled`。10 个事件。 | **接受**，0 跳过；遇 `TranscriptReplaced` 清空已重建消息并把期望 index 归零，`len` 等于其后的 `MessageAppended` 条数；`meta`/`interrupted`/`usage` 形状按 v1（§5.2/§5.10/§5.11）；按 ADR 0002/0005 清空旧 revision 标记后，`call-9` 补 `outcome_unknown_after_crash`。 | ADR 0005 G8 / ADR 0006 §5.11 |

通用约束（所有 fixture 共同满足，脚本逐项校验）：每行是合法 JSON 对象；信封七字段（`schema_version`/`session_id`/`seq`/`ts`/`event`/`critical`/`data`）齐全且类型正确；`seq` 从 0 严格 +1；同一文件内 `schema_version`、`session_id` 恒定；`ts` 为 RFC 3339 UTC（`Z` 结尾）；除 `truncated-tail.jsonl` 外所有文件以 LF 结尾，且**只有**该文件允许存在无 LF 的尾部。

fixture 内不含真实密钥与真实路径（一律 `/work/example` 等占位）。
