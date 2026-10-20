# 缓存效率：key、TTL、摘要复用前缀

- 状态：idea（条件：接入 Anthropic 路由、或 OpenAI 系路由的命中率掉下来时再做）
- 涉及 ADR：0018、0026
- 来源：`docs/research/2026-10-18-area-deep-dive/README.md` 第 3、4、9 条

## 背景

OpenAI/Responses 路由不发 `prompt_cache_key`（已核对今天没发）；Anthropic 断点只用 5 分钟 TTL；摘要请求换了一套 system，整段历史按全价计费。

实测（本机 158 个会话、13,020 次调用、2026-10-20）：全部是 OpenAI 兼容路由（Muse、DeepSeek、ChatGPT），**没有 Anthropic 会话**。缓存命中（cache_read / input）总体 98.7%；间隔超过 300 s 之后的 143 次请求仍 96.9%；按路由看 openai-responses 99.3%、opencode-go/deepseek 98.4%、chatgpt 97–98%。所以「长等待后缓存过期」在现用路由上没有发生，`prompt_cache_key` 不发也没有损失；压缩 158 个会话只有 1 次，摘要请求复用前缀的收益现在量不出来。

## 待决方向

OpenAI 和 Responses 路由发 `prompt_cache_key`，Anthropic 补第 4 个缓存断点；TTL 改 1 小时或长工具运行时保温；摘要请求复用原对话的 system、工具和历史以便读缓存（与"摘要降档"一起权衡，修订 ADR 0018）。
