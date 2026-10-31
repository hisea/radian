# 大窗口下输出预留从 C/4 降到 15%

- 状态：decided（2026-10-31）
- 涉及 ADR：0018（§The output the chat route sends、§预算）
- 来源：2026-10-31 Mac 轮，128k 下 R = 32k（25%），B = 96k；同行触发线 85–90%（pi window − 16384；oh-my-pi window − max(15%, 16384)；opencode 同；codex 90%）

## 决定

- C 和 O 都知道时，chat 请求发 `max_tokens = min(O, max(⌊0.15·C⌋, min(16384, ⌊C/4⌋)))`。效果：C = 32,768 → 8,192（不变）；C = 65,536 → 16,384（不变）；C = 128,000 → 19,200（原 32,000），B 从 96k 到约 108.8k；C = 1,000,000 → 150,000（原 250,000）。
- 只知道 C 的规则 `min(16384, ⌊C/4⌋)` 不变；不知道 C 不发，不变。R = max(min(16384, ⌊C/4⌋), 发出的 max_tokens) 的算式不变，只是输入变了。
- ADR 0018 的那句话改写；`docs/user/providers.md` 里写 max_tokens 规则的句子同步（有闭集/默认值的 drift 测试照常）。

## 边界情况

- O 小于算出来的值：取 O。
- `--context-window` 给的 C 与 catalog 的 C 不同：用生效的那个（今天的规则）。
- 每请求覆盖（extra body）仍然优先。

## 验收

- 单元测试：上面四个 C 值各一条；O 更小时取 O。
- 变异：系数 0.15 改 0.25 → 128k 测试失败；floor 去掉 → 32k 测试失败。
- 与 A 一起复跑 128k 两臂：压缩次数下降，0 below_target。
