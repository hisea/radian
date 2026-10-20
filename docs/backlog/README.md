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
| `project-skills.md` | decided | 0025、0031 | 接上项目级 skill（`<project>/.agents/skills`，到仓库根），并支持 `$ARGUMENTS` |
| `tool-small-fixes.md` | decided | （0037 的 64 KiB 默认若改则同步） | 剩余：grep 先丢上下文 + 上限降到 24–32 KiB + 描述指向 files/count；前台 shell 默认上限（等 shell-job）；patch 缩进容忍（待决） |
| `read-absorbs-list.md` | decided | 0043、0039 | `read` 读目录即列目录，`list` 退为一个版本的别名，`explore` 变三个工具；基于 shell/job 合并后的 main 开分支 |
| `file-tools-spawn-blocking.md` | decided | 无 | 提示词一句 → 基准 → `spawn_blocking`，让同一批 grep 真正并行 |
| `ask-user-pass.md` | decided | 0024、0036 | 保留 `ask_user`：描述写明只问用户拥有的取舍、能查到的不问；定义瘦身到约 1 KB |
| `bench-adapter-followups.md` | decided | 无 | Terminal-Bench 适配器：usage 回报 Harbor、`auth.json` chown、传 `--turn-timeout-secs` |
| `overflow-three-stage.md` | idea | 0018、0017 | 超长兜底按 70% → 50% → 35% 三档推进，今天只有一档 50% |
| `memory-provider.md` | idea | 0021、0013、0031 | MemoryBackend trait + 命令 backend + 离线 capture |
| `post-edit-diagnostics.md` | idea | 新 ADR | 编辑后诊断：tree-sitter 语法层 + 配置的检查命令；先数 dogfood 里 patch→check 序列 |
| `sandbox-auto-allow-shell.md` | idea | 0012、0016、0043 | workspace 档沙箱内 shell 自动放行，越界再问；前提是重开 Unix socket 限制 |
| `mcp-full-tier-ask.md` | idea | 0012、0043 | full 档 MCP 按 server 首次询问，工具定义按哈希固定 |
| `exec-jsonl-output.md` | idea | 0032 | `radian exec --output jsonl`：结果行、细分退出码、`-p -` 读 stdin |
| `eval-metrics.md` | idea | 无 | journal 指标提取器 + 固定任务集；Terminal-Bench 适配器在 Linux 盒子上做 |
| `cache-efficiency.md` | idea | 0018、0026 | `prompt_cache_key`、TTL、摘要复用前缀；实测现用路由命中 98.7%，接入 Anthropic 路由时再做 |
| `context-pruning.md` | idea | 新 ADR | 轻量档：清被同路径后续 `read` 取代的旧结果（占工具字节 24%）；条件：阈值 A/B 收益不够时才做 |
| `compaction-threshold.md` | idea | 0018、0008 | `[compaction] threshold` + 压缩后确定性补水；条件：eval-metrics 落地后做，A/B 定缺省值 |
| `herdr-prompt-radian.md` | idea | 0034 | `herdr agent prompt` 拒认运行中的 radian TUI，原因未查明 |
| `shell-auto-background-60s.md` | idea | 0037、0036、0011 | 转后台阈值 120 s → 60 s，提示词要求独立调用放同一条回复 |
| `prompt-deliver-first.md` | idea | 0036 | 提示词：先交付能跑的版本再迭代、用参照物差分；cobol smoke 是现成复现 |
| `runtime-resource-limits.md` | idea | 0036 | `<runtime>` 写实际可用 CPU/内存（cgroup），避免 `make -j16` 跑在 2 核容器 |
| `deadline-awareness.md` | idea | 0017、0036、0032 | radian 知道回合期限、自己收尾，并（待定）告诉模型剩余时间 |

只占索引行（暂无文件；来源都是总报告 `docs/research/2026-10-18-area-deep-dive/README.md`）：

| (无文件) | 状态 | ADR | 一句话 |
|---|---|---|---|
| no-edit 看门狗 | idea | 新 ADR（第 15 条） | 长时间只读不改时宿主自动发 steer；先在 D26 任务上 A/B |
| 持久 todo + `/goal` | idea | 新 ADR（第 19 条） | 目标确定性带过压缩，轮末自动续跑，resume 后一键续跑 |
| subagent 指定模型 | idea | 0043（第 14 条） | 小模型跑 explore/压缩；后台与续接是同条的后半 |
| CI 与发布 | idea | 分发 ADR（第 27 条） | Linux/macOS CI、`cargo-deny`、安装方式；前提是公开仓库 |
| SECURITY.md | idea | 无（第 28 条） | 一页威胁模型 + CVE 对照表 |
| 生态兼容 | idea | 待定（第 31 条） | 读 `.agents/skills`、接受 `mcpServers`、Codex 子集 hook、`radian import claude` |
