# `job` 结果带用时提示：本可以前台跑的后台任务

- 状态：decided
- 涉及 ADR：0037（决策 4 `job` 的返回形状；决策 1、2 的前台缺省 300 s、上限 600 s；决策 3a 的前台保留条目）
- 来源：A/B ab7 的 radian 会话日志；75 个 dogfood 会话（同一模型）的汇总

## 背景

- ab7 的 radian 会话：196 次模型调用；22 次 `background: true` 全是 cargo，最长 72 s，没有一个超过前台缺省 300 s；23 次 `job`（19 次 `wait`、4 次 `read`）；22 次后台启动里 14 次的下一个调用就是 `wait`。每次 `wait` 都是一次带完整 prompt 的模型调用（第 50 次调用 206,705 token，第 196 次 327,025），这是它比同一 brief 的 opencode 多出的调用的主要来源。
- `shell` 说明当时已经写着 `timeout_secs` 缺省 300、上限 600，`background` 参数已经写着 "Use for servers, watchers and commands expected to run longer than 600 s. Run tests and builds in the foreground with timeout_secs"。ab7 的二进制含这句话；brief 也没有要求后台。模型没有照做。再往说明里加一句同样的话没有证据支持，还违反 0037「每条规则只说一次」。
- 同一模型 75 个会话的汇总：350 次后台启动，121 次紧跟 `wait`，70%–87% 的后台任务 300 s 内结束。平均每个会话多 1–2 次调用；ab7 这样的会话多十几次。
- 「结束回复、等完成通知」和 `wait` 一样多一次调用，只有前台一次拿到结果才省。所以提示要指向前台，不是指向通知。
- 模型的实际习惯：前台调用几乎都手填 `timeout_secs: 30`，没有用过缺省值；它把 cargo 一律当长命令。提示里要带上缺省 300 s 和上限 600 s 这两个数。

## 决定

1. `job` 的 `wait` 或 `read` 返回时，若任务已结束、是显式 `background: true` 启动的（决策 3a 的前台保留条目不算）、用时不超过前台缺省超时（300 s），在 `[status: …]` 之前加一行：

   ```
   [this job ended after 70 s; a foreground shell call (default 300 s, max 600 s) returns such a command in one step]
   ```

2. 用时超过 300 s 的任务不加，后台是对的选择。每个任务只加一次：第一次看到它已结束的那次 `job` 调用加，之后的 `read` 不再加。
3. 完成通知不改，它已经带用时。`shell` 和 `job` 的说明文字不改，spec 字节上限不受影响。
4. 不新增 durable 记录（ADR 0010）：用时来自 `job` 读取结果里已有的 elapsed。
5. 阈值就是前台缺省超时，不另加配置项。

## 验收

- 单元测试：300 s 内结束的后台任务 `wait` 后含该行；超过 300 s 的不含；3a 前台保留条目不含；同一任务第二次 `read` 不含；该行在 `[status: …]` 之前。
- 变异各一个，报告里写明抓住它的测试：阈值失效（超过 300 s 也提示）、提示重复出现、3a 条目也提示、提示丢失。
- ADR 0037 决策 4 在 docs-only commit 里补这一行的规则，和代码一起合入；`journal-selfcheck` 过。
- A/B 指标：后台启动、300 s 内结束、下一个调用就是 `wait` 的次数（ab7 基线 14）；总模型调用数作副指标。同一 brief 同一模型至少跑两次，单次方差大于这里要测的效应。
