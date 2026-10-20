# `/context`：按类别列出下一次请求的上下文估算

- 状态：decided＼�第一档）
- 涉及 ADR：0015（估算公式与 C1「用量来源绝不混淆」）、0036（system prompt 与上下文文件）、0021（记忆注入）、0018（压缩）
- 来源：`docs/research/2026-10-20-peer-slash-commands.md` §3 第一档

## 背景

footer 只显示一个总百分比。omp `/context`、Claude Code `/context`、Codex `/status` 都能告诉用户上下文被什么占满了。ADR 0015 的估算器已经对请求形状算字节，缺的只是按类别拆开并打印。

## 决定（已定）

- `/context` 把下一次正式请求的形状按类别估算后以通知输出：基础 system prompt、上下文文件（AGENTS.md 等，ADR 0036）、记忆注入（ADR 0021）、工具定义、历史里的用户消息、assistant 文本、reasoning（仅 `count_reasoning` 打开时列出）、工具结果；每行给出估算 token 与占比，最后一行是总估算 `T / B`。
- 同一个估算规则（ASCII 字节向上取整 ÷ `bytes_per_token`，非 ASCII 每字符 1），不引入分词器。
- 最近一次 provider **报告**的输入 token 单独一行并标明「reported」，估算行标明「estimated」（ADR 0015 C1）。
- 附带：消息条数、本会话压缩次数。
- 只读命令，回合进行中也可用，不打断回合。

## 验收

- 单测：各类别估算之和等于总估算；类别划分覆盖请求里的每条消息与每个工具定义，没有遗漏或重复。
- 通知里 reported 与 estimated 两行同时存在且标签不同；没有 usage 时 reported 行写「none yet」。
- `/help` 列出 `/context`。
