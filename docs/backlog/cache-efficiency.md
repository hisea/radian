# 缓存效率：key、TTL、摘要复用前缀

- 状态：idea
- 涉及 ADR：0018、0026
- 来源：`docs/research/2026-10-18-area-deep-dive/README.md` 第 3、4、9 条

## 背景

OpenAI/Responses 路由不发 `prompt_cache_key`（已核对今天没发）；缓存 TTL 只有 5 分钟，`job_output` 中位等待 361 s，等完缓存大概率过期；摘要请求换了一套 system，整段历史按全价计费。

## 待决方向

OpenAI 和 Responses 路由发 `prompt_cache_key`，Anthropic 补第 4 个缓存断点；TTL 改 1 小时或长工具运行时保温；摘要请求复用原对话的 system、工具和历史以便读缓存（与"摘要降档"一起权衡，修订 ADR 0018）。
