# `check-fixtures.sh` 变异检查记录（Phase 0 / schema）

日期：2026-10-12 · 基线：`fbbed74`（工作树 `/tmp/schema-mut`，一次性检出，已删除）
方法：每个变异改完后运行 `bash docs/schema/check-fixtures.sh`，期望**非 0 退出且 FAIL 信息指向被破坏的行为**；改完立即 `git checkout --` 还原。收尾时 `git status --short` 为空。

| # | 变异 | 结果 | 被什么抓住 |
|---|---|---|---|
| M1 | `v1-minimal.jsonl` 第 3 行删掉信封必需字段 `ts` | ✅ 失败 | `line 3: missing envelope field ts` |
| M2 | `v1-minimal.jsonl` 第 4 行 `seq` 3→2（重复） | ✅ 失败 | `line 4: 期望 seq 3，得到 2` |
| M3 | `truncated-tail.jsonl` 的截断挪到中间行（第 3 行被并入半行） | ✅ 失败 | `完整行数 4 != 期望 5`（torn tail 只允许最后一行） |
| M4 | `v1-minimal.jsonl`（期望全接受）末尾追加一条 `critical:true` 未知事件 | ✅ 失败 | 行数校验（较弱，见 M4'） |
| M4' | 同上，但**同位数替换**第 8 行为 `critical:true` 未知事件（行数不变） | ✅ 失败 | `line 8: 未知 critical 事件未被策略拒绝`（直达 AC-S-3b 拒绝策略） |
| M5 | 脚本期望表里 `v1-minimal` 事件数 9→8（改错期望） | ✅ 失败 | `完整行数 9 != 期望 8` |
| M6 | `schema-version-newer.jsonl` 的 `schema_version` 2→1 | ✅ 失败 | `schema_version 1 != 期望 2` |
| M7 | `crash-after-started.jsonl` 删掉 `call-2` 的 `Started` | ✅ 失败 | 行数校验（较弱，见 M7'） |
| M7' | 同上，但把该 `Started` 的 `call_id` 改成 `call-x`（行数不变） | ✅ 失败 | `repair {'call-2': 'not_started_after_crash'} != 期望 {'call-2': 'outcome_unknown_after_crash'}`（直达 ADR 0002 恢复语义） |
| M8 | `unknown-event-noncritical.jsonl` 的未知事件 `critical` false→true | ✅ 失败 | `line 2: 未知 critical 事件未被策略拒绝` |

备注：M4/M7 初版被行数检查提前拦截，证据弱；M4'/M7'/M8 是为直达策略与 repair 逻辑补做的强化变体。10/10 全部被抓住，无漏网。

## 评审补充（合并时由评审者追加）

评审者另做了 6 个变异，5 个被抓住（删 `ts`、整行重复、torn tail 挪到中间行、`seq` 跳号、未知事件的 `critical` 位翻转），1 个**没有**被抓住：

| 变异 | 结果 | 说明 |
|---|---|---|
| 把 `fixtures/README.md` 里的「拒绝」改成「接受」 | ❌ 脚本仍退出 0 | 期望写死在脚本的 `EXPECT` 字典里，脚本从不读 README。M5「改错期望」改的是脚本内的表，不是 README。 |

这不是脚本的缺陷（它无法校验一份散文表格），但 README 原文写着「脚本以本表为准运行」，与事实不符。已改掉该说法，明确两处各自独立、必须手工同步。
