# AWS Bedrock Converse 流式接口

- 状态：idea（没有明确需求不做）
- 涉及 ADR：0026（凭据、descriptor）、0020（`~/.radian/` 布局与沙箱可读范围）、0013（出网清单）
- 来源：pi 的 `bedrock-converse-stream`

## 背景

工作量是几种里最大的：请求要 AWS SigV4 签名，响应是 AWS 自己的二进制事件流（eventstream），不是 SSE；凭据来自 AWS 的配置链（环境变量、`~/.aws`、SSO）。

## 待决问题

- 先确认目标模型能否经 Bedrock 提供的 OpenAI 兼容接口访问；能的话这一项不需要做。
- 若做：签名与凭据链用现成 crate 还是自写；凭据文件是否让沙箱可读（ADR 0020）。

## 验收

有具体用户需求，并写成 ADR 后再派发。
