# 待办（backlog）

状态：accepted＼�

## 规则

- ADR 是**代码现在样子的快照**。打算做、还没做的事放在这里，一件事一个文件。
- **引用只走一个方向**：backlog 文件可以引用 ADR（`涉及 ADR`），ADR 绝不引用 `docs/backlog/`。事项做完就删掉对应的 backlog 文件，ADR 里永远是干净的快照。
- backlog 条目**不是 worker brief**：只写稳定的东西（是什么、为什么、定好的设计、涉及哪些 ADR、怎么算做完）。worktree 路径、在某提交核对过的行号、命令，写进派发时现写的 brief，不写在这里。
- 事项落地时，同一个分支做两件事：**（a）** 在自己的 docs-only commit 里改 ADR，**（b）** 删掉 backlog 文件；两件事和代码一起合进 `main`。`main` 上的 ADR 永远和代码一致。
- 状态只有三种：`idea`（没定）、`decided`（设计已定，可以派发）、`in-progress`（已派发，注明分支）。

## 条目模板

```md
# 标题

- 状态：idea | decided | in-progress（分支名）
- 涉及 ADR：0008、0026
- 来源：决定 / 研究报告路径与章节

## 背景

## 决定（已定）/ 待决问题（未定）

## 验收
```

`idea` 的条目可以只有 5–15 行；不要编造决定，开放问题就写成开放问题。

## 索引（按状态排：in-progress、decided、idea）

| 文件 | 状态 | ADR | 一句话 |
|---|---|---|---|
| `mcp-servers-json.md` | decided（生态 2/4） | 0008、0031、0012、0043 | MCP 配置接受标准 `mcpServers` 结构与 `${VAR}` 展开，只做 stdio；项目 `.mcp.json` 进信任门、按内容哈希确认，确认即批准其 server（一道门）；缺省 `inherit_env=["HOME"]`；环境白名单不变；MCP 文本结果套 `read` 的 64 KiB 上限并带截断标记 |
| `command-hooks-claude-subset.md` | decided（生态 3/4） | 新 ADR；0012、0031、0011、0017、0016 | 外部命令 hook 只做 `PreToolUse`/`PostToolUse`/`Stop`，配置与 stdin 用 Claude/Codex 的子集，`allow`/`ask` 拒收，用户级 `[hooks]`、项目级 `.radian/hooks.json` 进信任门，工具名只在导入时翻译；吸收总报告第 17 条；编辑后检查是项目级 `PostToolUse`：配了才跑、同步每次、不批处理不延后、不内置不自动启用（本机数据：92% 的编辑消息只有一次编辑、跨轮段平均 1.83） |
| `import-claude.md` | decided（生态 4/4） | 新 ADR；0029、0008、0025、0031 | `radian config import-claude` 一次性翻译，来源哪一级产物落哪一级， commands/skills/`mcpServers`/hooks，丢弃项逐行报告，不读凭据、不覆盖、不导入会话与 memory；依赖前两条与 project-skills |
| `tui-queue-command.md` | decided（第一档） | 0011、0027、0023 | `/queue <msg>` 在回合进行中投 followup，footer 显示待发条数；不做取回/重排 |
| `tui-btw-side-question.md` | decided（第一档） | 0010、0018、0023、0007、0017 | `/btw <q>` 一次额外请求，与正式请求同前缀、不执行工具、只在覆盖层、不落 transcript/journal；一次一个 |
| `tui-new-session.md` | decided（第一档） | 0022、0023、0043、0026、0037 | `/new` 空闲时原地开新会话，沿用当前生效的模型/档位；有 job 时拒绝；不加 `/clear` 别名 |
| `tui-context-command.md` | decided（第一档） | 0015、0036、0021、0018 | `/context` 按类别打印下一次请求的估算，reported 与 estimated 分开标 |
| `tui-copy-command.md` | decided（第一档） | 0027、0028 | `/copy` 复制最近一条已提交 assistant 的 markdown 源；`/copy code` 取最后一个代码块 |
| `tui-retry-command.md` | decided（第一档） | 0017、0011、0003 | `/retry` 在模型错误结束后重发上一条用户消息 |
| `tui-bell.md` | decided（第一档） | 0027、0024、0008、0037 | `tui.bell = off|attention|all`（缺省 off）：审批/提问/回合结束发 BEL |
| `tui-loop-continuation.md` | decided | 0011、0005、0037、0027、0023、0017 | 续跑引擎第一、二步：`/loop [--every] [--times] [--for] [prompt]` 轮末重投，`User` 来源 + meta，缺省上限 20 次/1 h，不计唤醒闸门 |
| `goal-mode.md` | decided（依赖 tui-loop-continuation） | 新 ADR；0018、0036、0011、0005、0043、0037、0034、0006、0022 | `/goal` 持久目标 + `goal {progress\|done\|wait}` 工具 + 预算；目标块确定性注入不进摘要；resume 后 paused；`exec -p --goal` 待决 |
| `tui-inspector-panel.md` | decided（mcp 页等 mcp-servers-json） | 0023、0035、0037、0043、0045、0027 | 一个带标签页的 Overlay：`jobs | tools | mcp | hooks`，`/panel` 总入口，只读除杀 job，文本形式保留 |
| `sandbox-token-profile.md` | decided | 0012、0016、0043 | 沙箱档 profile 构造收 `&Sandbox` 而不是 `bool`，拿不到实例造不出放行 profile（外部审核 P1-1） |
| `exec-result-run-usage.md` | decided | 0044、0007 | `Result` 加 `run_turns`/`run_usage`/`run_cost`，以 tee 的 `written_len` 为本次运行边界；累计字段不变（外部审核 P2-2） |
| `foreground-orphan-sweep.md` | decided | 0037、0039、0020 | resume 时清扫 `.jobs/` 里无登记项的输出文件；删会话时删目录（外部审核第四节） |
| `tool-source-tag.md` | idea | 0012、0043 | 工具来源注册时打标签，替代 `__` 推断 MCP；与 mcp-servers-json 一起（外部审核 P2-4） |
| `overflow-three-stage.md` | idea | 0018、0017 | 超长兜底按 70% → 50% → 35% 三档推进，今天只有一档 50% |
| `memory-provider.md` | idea | 0021、0013、0031 | MemoryBackend trait + 命令 backend + 离线 capture |
| `subagent-ask-forwarding.md` | idea | 0043、0010、0024 | 子会话仍要问的 ask 转回父会话；缺 durable `PermissionRequested` 与 control 收审批；依赖沙箱内放行 |
| `subagent-explore-model.md` | idea | 0043、0026 | 用户级配置给 `explore` 更便宜的模型（0043 §1 已允许，未实现） |
| `verify-test-binaries.md` | idea | 新 ADR | 合并集成测试二进制压 Verify 时间；`set_var` 与全局状态先处理；release 调优不做 |
| `mcp-full-tier-ask.md` | idea | 0012、0043 | full 档 MCP 按 server 首次询问，工具定义按哈希固定 |
| `eval-metrics.md` | idea | 无 | journal 指标提取器 + 固定任务集；Terminal-Bench 适配器在 Linux 盒子上做 |
| `cache-efficiency.md` | idea | 0018、0026 | `prompt_cache_key`、TTL、摘要复用前缀；实测现用路由命中 98.7%，接入 Anthropic 路由时再做 |
| `context-pruning.md` | idea | 新 ADR | 轻量档：清被同路径后续 `read` 取代的旧结果（占工具字节 24%）；条件：阈值 A/B 收益不够时才做 |
| `compaction-threshold.md` | idea | 0018、0008 | `[compaction] threshold` + 压缩后确定性补水；条件：eval-metrics 落地后做，A/B 定缺省值 |
| `herdr-prompt-radian.md` | idea | 0034 | `herdr agent prompt` 拒认运行中的 radian TUI，原因未查明 |
| `shell-auto-background-60s.md` | idea | 0037、0036、0011 | 转后台阈值 120 s → 60 s，提示词要求独立调用放同一条回复 |
| `prompt-deliver-first.md` | idea | 0036 | 提示词：先交付能跑的版本再迭代、用参照物差分；cobol smoke 是现成复现 |
| `runtime-resource-limits.md` | idea | 0036 | `<runtime>` 写实际可用 CPU/内存（cgroup），避免 `make -j16` 跑在 2 核容器 |
| `deadline-awareness.md` | idea | 0017、0036、0032 | radian 知道回合期限、自己收尾，并（待定）告诉模型剩余时间 |
| `acp-server.md` | idea（暂定不做） | 新 ADR；0032、0009、0024、0011、0010、0043 | ACP 服务端：先做外部适配器验证需求；审批→`request_permission`、inbox→`session/prompt`、live→`session/update`；在 exec jsonl 与控制口客户端之后 |

只占索引行（暂无文件；来源都是总报告 `docs/research/2026-10-18-area-deep-dive/README.md`）：

| (无文件) | 状态 | ADR | 一句话 |
|---|---|---|---|
| no-edit 看门狗 | idea | 新 ADR（第 15 条） | 长时间只读不改时宿主自动发 steer；先在 D26 任务上 A/B |
| 持久 todo + `/goal` | idea | 新 ADR（第 19 条） | 目标确定性带过压缩，轮末自动续跑，resume 后一键续跑 |
| CI 与发布 | idea | 分发 ADR（第 27 条） | Linux/macOS CI、`cargo-deny`、安装方式；前提是公开仓库 |
| SECURITY.md | idea | 无（第 28 条） | 一页威胁模型 + CVE 对照表 |
