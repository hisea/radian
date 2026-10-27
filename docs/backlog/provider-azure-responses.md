# Azure OpenAI 的 Responses 接口

- 状态：idea（有 Azure 用户需要时再做）
- 涉及 ADR：0026（D10 命名 provider、D12 Responses 的 API key 路径）
- 来源：pi 的 `azure-openai-responses`

## 背景

协议本身就是 Responses，差别在地址与认证：deployment 形式的 URL、`api-version` 查询参数、`api-key` header。不需要新 adapter，是 Responses adapter 的一个配置变体。

## 待决问题

- 配置形状：命名 provider 加 `kind = "azure-openai-responses"`，还是 Responses 的 `kind` 加 `api_version` 与认证 header 方式两个键。
- 动手前先核实 Azure 当前是否也接受标准 Bearer 与无 `api-version` 的 `/openai/v1` 路径；若接受，这一项可能只剩文档。

## 验收

一个 Azure deployment 经 Responses 跑通，配置项按 AGENTS.md 文档规则写进手册。
