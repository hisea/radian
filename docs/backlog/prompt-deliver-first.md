# 提示词：先交付能跑的版本，再迭代

- 状态：idea
- 涉及 ADR：0036（角色规则）
- 来源：Terminal-Bench smoke（2026-10-20，LXC，opencode-go / deepseek-v4.1-flash）；dogfood D24（445 次调用无一次编辑）

## 背景（已核实）

- `cobol-modernization`（把 `/app/src/program.cbl` 用 Python 重写成 `/app/program.py`）：15 分钟、40 次模型调用，写了 20 多个 COBOL 探测程序去摸 GnuCOBOL 对怪异输入的边界行为，**一次 `patch`/`write` 都没有**，到 900 s 超时没写出 `program.py`，得 0 分。输出 15.2 万 token，大部分是推理；后半段单次调用 60–80 s。
- 任务本身就有参照物：编译原 COBOL 跑一遍，和 Python 输出逐字节比对即可。
- 与研究报告记下的"只查不改"同一类（`docs/research/2026-10-18-area-deep-dive/README.md` 第 15 条 no-edit 看门狗）。

## 待决方向

- 基础提示词加一条：尽早交出能跑的版本，再迭代；有参照物（原实现、期望输出）时用差分测试驱动，而不是先穷举边界行为。
- 与 no-edit 看门狗（索引行，第 15 条）一起看：本条的 cobol 运行是看门狗的现成复现用例。
- 同一任务试低一档推理（flash 模型），看是否减少发散。

## 验收

同一 Terminal-Bench 子集（含 `cobol-modernization`）改前改后各跑 3 次：首次 `write`/`patch` 出现的时间、成功率。
