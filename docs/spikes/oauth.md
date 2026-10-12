# Phase 0b 可行性验证：OAuth（Anthropic / OpenAI 订阅授权）

- **结论：部分**（OpenAI 通过；Anthropic 不通过——官方明文禁止把消费者订阅用于第三方工具）
- 时间：2026-10-12（**所有 URL 的访问日期均为 2026-10-12**）
- 方法：**只读官方文档**（provider 官方开发者文档 / 官方条款）。未做任何真实登录、未申请任何凭据、未抓包、未使用任何非官方手段。文中每条结论给出 URL + 访问日期 + 原文短引用（引用保持原文，不改写成"官方表述"）。
- 一句话：**OpenAI 官方支持**把 ChatGPT Plus/Pro 订阅经 OAuth 用于第三方**开源**客户端；**Anthropic 官方把"经消费者订阅路由请求"明文列为未授权**，且**没有**面向第三方应用的 OAuth 文档。

## 结论总表

| 问题 | Anthropic | OpenAI |
|---|---|---|
| 1 第三方 OAuth 流程 | **未提供/未找到**（只有自家 CLI/Claude Code 的 OAuth） | **提供**：Sign in with ChatGPT，Authorization Code + PKCE + OIDC，回环回调，OSS 走动态注册 |
| 2 凭据怎么存 | 第三方流程不存在；官方自有工具用 Keychain / `0600` 文件 | 官方要求本地文件、原子写、`0600`、不提交不记录；refresh 轮换 |
| 3 订阅额度用于第三方 | **不允许**（Usage Policy 明文；Consumer Terms 限制非 API-key 自动化访问） | **允许**（OSS 路径，preview）：官方文档明确支持 |
| 4 错误 UX | 第三方 OAuth 无官方文档；API key 过期有 401 文档 | 官方给了完整错误码与 UI 指引（见 §4） |

**对计划的影响（先给结论）**：**首发只发 API-key provider**——对 Anthropic 是必须，对 OpenAI 也仍是最简单路径。OpenAI 的 ChatGPT-plan OAuth 官方支持、可作为后续可选能力；Anthropic 的订阅 OAuth **不做**。

---

## 评审核实（2026-10-12，评审者独立抓取官方页面）

决定首发范围的两条引用，评审者自己抓了页面核对，**逐字一致**：

- Anthropic Usage Policy（<https://www.anthropic.com/legal/aup>）："Resell, proxy, or otherwise provide access to Claude through unauthorized means" / "including services that route requests through consumer subscriptions"；页面标注 **Effective November 12, 2026**。注意这是**生效日在今天之后**的版本（今天 2026-10-12），实装前应再确认当时的生效版本。
- OpenAI（<https://developers.openai.com/siwc/quickstart>）："Eligible ChatGPT Plus and Pro users can use their ChatGPT plan for AI requests in participating apps"；关于试用范围，页面区分两件事——**Sign in with ChatGPT** 本身 "is currently available to selected commercial partners through a limited trial"，而 **ChatGPT plan usage** "is available to all open-source partners and selected private clients"。评审者注意到：该页面正文**没有**出现 "preview" 一词（本文档别处用 "preview" 概括其状态；`preview-limitations` 页另有其名）。

其余 URL 与引用评审者**未逐条抓取核实**，以本文档作者标注的访问日期为准。

## 1. 支持哪种流程

### Anthropic — 面向第三方应用的 OAuth：未提供/未找到

**API 认证只有三种方式，没有面向第三方应用的 OAuth 授权码/设备码流程。**
来源：<https://platform.claude.com/docs/en/manage-claude/authentication>（访问 2026-10-12）。原文：

> "The Claude API supports three ways to authenticate requests:"
> 表格三行分别为 **API key**（"Static `sk-ant-api...` secret sent as a bearer token in the `Authorization` header"）、**Workload Identity Federation**（"Short-lived bearer token exchanged from your identity provider's identity token"）、**App Attest**（"Short-lived access token issued to a genuine, attested installation of your registered iOS or macOS app"）。

注意：文档里出现的 `POST /v1/oauth/token` 是 **WIF 的 IdP JWT → 短期 API token 交换**，不是用户授权流程；App Attest 需要注册 App 且面向 iOS/macOS 应用。两者都不是"第三方 CLI/桌面应用用 OAuth 授权用户账号"。

**Anthropic 自家的 CLI 与 Claude Code 确实用 OAuth，但文档只描述自家客户端，没有第三方 client 注册/端点/scope 的公开文档。**
来源：<https://platform.claude.com/docs/en/cli-sdks-libraries/cli/authentication>（访问 2026-10-12）。原文：

> "`ant auth login` lets you call the API without creating or managing an API key. It opens a browser-based OAuth flow against the Claude Console and stores the resulting credentials under `$ANTHROPIC_CONFIG_DIR`"
> "On a remote host or in any environment without a local browser, pass `--no-browser` to print the authorize URL and paste the returned code back into the terminal."
> "During the browser flow, you select an organization and then a workspace. The issued token is scoped to that workspace"
> "Login writes credentials to `credentials/<profile>.json`."
> `ant auth login --scope "org:admin"`（管理员 scope）

来源：<https://code.claude.com/docs/en/authentication>（访问 2026-10-12）。原文：

> "**Claude Pro or Max subscription**: log in with your claude.ai account."
> "On first launch, Claude Code opens a browser window for you to log in."
> "If your browser shows a login code instead of redirecting back after you sign in, paste it into the terminal at the `Paste code here if prompted` prompt. This happens when the browser can't reach Claude Code's local callback server, which is common in WSL2, SSH sessions, and containers."

**查过、但没有第三方 OAuth 的官方页面**（均在 2026-10-12 访问）：`platform.claude.com/docs/llms.txt`（全站文档索引，grep `oauth` 只命中一个 MCP 凭据校验端点）、`.../docs/en/manage-claude/authentication`、`.../docs/en/cli-sdks-libraries/cli/authentication`、`.../docs/en/agents-and-tools/remote-mcp-servers`（那是 Claude 连第三方 MCP server 的 OAuth，不是第三方用 Claude API）、`code.claude.com/docs/llms.txt`、`code.claude.com/docs/en/authentication`。

**结论（问题 1，Anthropic）**：官方**未提供**面向第三方命令行/桌面应用的 OAuth；能查到的 OAuth 都属于 Anthropic 自家客户端（`ant` CLI、Claude Code、Agent SDK）。按硬约束"官方未说明，不采用"——**不采用**。

### OpenAI — 面向第三方应用的 OAuth：提供（Sign in with ChatGPT）

来源：<https://developers.openai.com/siwc/quickstart>（访问 2026-10-12）。原文：

> "Sign in with ChatGPT lets users sign in to your app with their ChatGPT account and, when eligible, use their ChatGPT plan for AI requests."
> "**For open-source developers:** Let users run AI workloads in your tools with their ChatGPT plan, without requiring them to provide an API key. The open-source sign-in flow registers your client and issues OAuth credentials for eligible Responses API requests, without a client secret or partner API key."
> "Sign in with ChatGPT is currently available to selected commercial partners through a limited trial. ChatGPT plan usage is available to all open-source partners and selected private clients."

**流程类型**：OAuth 2.0 **Authorization Code + PKCE + OpenID Connect**。
来源：<https://developers.openai.com/siwc/website>（访问 2026-10-12）。原文：

> "Add **Continue with ChatGPT** to your website using the OAuth 2.0 Authorization Code flow with PKCE and OpenID Connect."

**回调**：本机**回环端口** `127.0.0.1`（scheme/host/path 固定，只有端口可变；明确说不要用 `localhost`）。
来源：<https://developers.openai.com/siwc/token-sharing-open-source/sign-in>（访问 2026-10-12）。原文：

> "`redirect_uri` | Use an HTTP loopback callback on `127.0.0.1` from initial registration onward, for example `http://127.0.0.1:1455/auth/callback`. … only the port may vary. Keep the scheme, host, and path unchanged: `/callback` does not match `/auth/callback`. … Do not substitute with `localhost`."

**端点**（同一 sign-in 页 + website 页）：

> "Open the system browser at `https://auth.openai.com/api/accounts/authorize`"
> "POST a form-encoded `authorization_code` grant to `https://auth.openai.com/api/accounts/oauth/token` with the issued `client_id`, `code`, `code_verifier`, the same `redirect_uri`, and the same `resource`. No client secret is required."

**是否需要预先注册 client / 注册方式**：
- **开源路径 = 动态注册，自助**：首次用 `client_id=dynamic_agent_client` + `agent_name_hint` + `ext_agent_host_id`，回调返回签发的 `client_id`（如 `oaiapp_...`），之后复用。来源：sign-in 页（同上）。原文：
  > "Start first-time registration with `client_id=dynamic_agent_client`, include the host's `ext_agent_host_id`, and set `agent_name_hint` to your app's actual name … After the user names and authorizes the agent, the callback returns its issued `client_id`."
  > "This direct flow needs neither a client secret nor a partner API key."
- **网站/商业伙伴路径 = 需申请，限受邀**：<https://developers.openai.com/siwc/request-client-id>（访问 2026-10-12）。原文：
  > "Sign in with ChatGPT is currently offered to a select group of commercial partners. To join the waitlist, complete the Sign in with ChatGPT interest form."
  且 website 页说需 "Request an OAuth client"，client id "typically beginning with `oaiapp_`"，confidential client 有 client secret。

**scope**：身份与计划用量分开。来源：quickstart + sign-in 页（同上）。原文：

> "Identity-only clients request `openid profile email` to receive an ID token for sign-in. Clients that use ChatGPT plans also request scopes for Responses API access."
> "`scope` | Identity scopes: `openid profile email`. ChatGPT plan usage scopes: `offline_access resource.invoke chatgpt.tokens.use.direct`."
> "Using a ChatGPT plan requires separate Responses API scopes that the user must authorize. It doesn't grant access to the user's API key or conversations."

**设备码**：官方 OSS 文档只描述回环回调；**未找到**设备码流程或无浏览器路径的官方说明（Claude Code 的 paste-code 回退是 Anthropic 自有工具的行为，不能移植）。

**结论（问题 1，OpenAI）**：**提供**——Authorization Code + PKCE（S256）+ OIDC；OSS 走回环 `127.0.0.1` + 动态注册（自助、无 client secret）；商业/网站集成需申请、限受邀。

---

## 2. 凭据怎么存

### Anthropic

第三方 OAuth 流程不存在，所以"第三方 OAuth 凭据存哪"没有官方文档可引。官方**自家**客户端的存储（可作为失败模式的参考）：
来源：<https://code.claude.com/docs/en/authentication>（访问 2026-10-12）。原文：

> "On macOS, credentials are stored in the encrypted macOS Keychain. When the Keychain rejects the write, such as when it's locked in an SSH session, Claude Code stores your login in `~/.claude/.credentials.json` with file mode `0600` instead, the same storage it uses on Linux."

`ant` CLI 则把凭据放在 `$ANTHROPIC_CONFIG_DIR` 下的 `credentials/<profile>.json`（来源：cli/authentication 页，见 §1）。

### OpenAI（官方给了明确要求，且与 ADR 0008 兼容）

来源：<https://developers.openai.com/siwc/token-sharing-open-source/sign-in>（访问 2026-10-12）。原文：

> "Store credential files under an app-defined path such as `~/.config/YOUR_TOOL/`. Write files atomically with owner-only permissions (`0600` on Unix), and never commit or log them. Replace the access token, expiry, granted scopes, and rotating refresh token together after a successful refresh."

凭据记录字段（官方示例）：`email`、`issuer`、`subject`、`client_id`、`ext_agent_host_id`、`id_token`、`access_token`、`refresh_token`、`token_type`、`expires_in`、`scopes`、`saved_at`（同页）。

**生命周期与刷新**：<https://developers.openai.com/siwc/token-sharing-open-source/token-reference>（访问 2026-10-12）。原文：

> "Access tokens are valid for one hour (`expires_in: 3600`)."
> "Refresh tokens are valid for 30 days. Each successful refresh returns a replacement refresh token with a fresh 30-day lifetime."

刷新：<https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions>（访问 2026-10-12）。原文：

> "POST form-encoded `grant_type=refresh_token`, the issued `client_id` saved with this token set (not `dynamic_agent_client`), the saved `refresh_token`, and `resource=https://api.openai.com/v1` to `https://auth.openai.com/api/accounts/oauth/token`; omit `scope` to retain the grant."
> "Store and use the latest replacement, and serialize refreshes for the same session so two processes do not race a rotating token."
> 登出用 discovery 文档里的 `revocation_endpoint`，`token_type_hint=refresh_token`。

**与 ADR 0008 的关系（我们的方案，不是官方规定）**：ADR 0008 要求配置里 **secret 只存引用**（`SecretRef { from: env|keychain|file, name }`）。OpenAI 的 OAuth 凭据文件**正是**一个 `from: file` 的引用目标：配置文件里只写 `SecretRef { from: file, name: "~/.config/radian/openai-oauth/<label>.json" }`，token 本体留在 `0600` 文件里，配置不含明文。这与官方"文件 + `0600`"的要求一致。

- **keychain 引用**：macOS 可用，但**无头/SSH 环境**下 Keychain 可能锁定/不可写——Anthropic 官方自有工具的做法就是回退到 `0600` 文件（见上）。因此 Radian 应**同时**支持 `keychain` 与 `file` 两种引用；keychain 失败不是"无保护"，回退文件必须 `0600` 且不落日志。**"keychain 在无头/ssh 不可用怎么办"官方没有针对第三方 OAuth 的说明**（Anthropic 的 Keychain 回退是 Claude Code 的行为，OpenAI 直接规定用文件）——这条是**我们自己的设计决定**，不是官方结论。
- **多账号/多 provider**：OpenAI 官方要求**每个签发的 `client_id` 一条独立凭据记录**，即使 email 相同也要分开。来源：profiles-and-sessions 页（同上）。原文：
  > "Keep each registration's basic account info, issued client ID, and credentials separate, even when two registrations have the same email address. Use distinct, stable labels"
  > "Email helps users recognize an account but may be identical across account registrations."
  我们的方案：以 `(provider, 稳定标签, client_id)` 区分，`SecretRef.name` 编码该标签；**不要用 email 当键**。
- **host 标识**：官方要求每个"host"（一台机器/一个自托管 VM）一个稳定的 `ext_agent_host_id`，在首次登录前生成并持久化，且**不是认证凭据**。来源：<https://developers.openai.com/siwc/token-sharing-open-source>（访问 2026-10-12）。原文：
  > "Choose and persist the required `ext_agent_host_id`." … "A host ID is not an authentication credential."

---

## 3. 订阅额度能否用于第三方 agent（最要紧）

### Anthropic — 官方未允许，且明文限制：**不允许**

**Usage Policy 明文把"经消费者订阅路由请求"列为未授权访问。**
来源：<https://www.anthropic.com/legal/aup>（访问 2026-10-12；页面标注 "Usage Policy Effective November 12, 2026"）。原文（"Do Not …" 一节）：

> "Resell, proxy, or otherwise provide access to Claude through unauthorized means, including services that route requests through consumer subscriptions or misrepresent the product or client being used"

**Consumer Terms 限制非 API key 的自动化访问。**
来源：<https://www.anthropic.com/legal/consumer-terms>（访问 2026-10-12；页面标注 "Effective October 8, 2025"）。原文（第 3 节 "Use of our Services"，禁止清单）：

> "Except when you are accessing our Services via an Anthropic API Key or where we otherwise explicitly permit it, to access the Services through automated or non-human means, whether through a bot, script, or otherwise."

**订阅专属功能明确"第三方 provider 不可达"。**
来源：<https://code.claude.com/docs/en/feature-availability>（访问 2026-10-12）。原文：

> "### Features that require a Claude subscription
> These require signing in with a claude.ai account and are not reachable with an Anthropic Console API key or from a third-party provider:"

**结论（问题 3，Anthropic）**：**不允许**。Claude Pro/Max 订阅**不能**经 OAuth 给 Radian 这样的第三方 agent 消耗。要用 Claude 模型，必须走 **API key**（其使用由 Commercial Terms 约束；Commercial Terms A.1 允许 "power products and services Customer makes available to its own customers and end users"）。

### OpenAI — 官方明确支持（开源路径，preview）：**允许**

来源：<https://developers.openai.com/siwc/quickstart>（访问 2026-10-12）。原文：

> "**For users:** … Eligible ChatGPT Plus and Pro users can use their ChatGPT plan for AI requests in participating apps and manage app usage and access in ChatGPT settings."
> "ChatGPT plan usage is available to all open-source partners and selected private clients."

来源：<https://developers.openai.com/siwc/token-sharing-open-source>（访问 2026-10-12）。原文：

> "In addition to identity scopes, your open-source app can request permission to use the user's ChatGPT plan for eligible Responses API requests."

**限制（同样来自官方，必须一起读）**：
- preview / 限范围：quickstart 页 "Sign in with ChatGPT is currently available to selected commercial partners through a limited trial. ChatGPT plan usage is available to all open-source partners and selected private clients."；且 <https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations>（访问 2026-10-12）列了 Responses API 的强制项与不支持项，原文：
  > "**HTTP requests:** Set `store: false` and `stream: true`." … "**Unsupported fields:** Omit `background`, `conversation`, `max_output_tokens`, `max_tool_calls`, `metadata`, `moderation`, `multi_agent`, `prompt`, `prompt_cache_retention`, `safety_identifier`, `temperature`, `top_logprobs`, `top_p`, `truncation`, and `user`." … "**Unsupported tools:** Image generation, file search, Code Interpreter, native computer use, hosted MCP/connectors, and Responses `tool_search`."
- 用量共享：profiles-and-sessions 页（同上）原文：
  > "For ChatGPT Plus users, the five-hour usage limit is shared across all apps where they use their ChatGPT plan, including private and open-source clients. … The five-hour usage limit does not apply for Pro users."

**与 OpenAI ToU 的关系（只引原文，不下法律结论）**：
来源：<https://openai.com/policies/terms-of-use>（访问 2026-10-12；"Effective: January 1, 2026"）。原文：

> "You may not share your account credentials or make your account available to anyone else and are responsible for all activities that occur under your account."
> "Automatically or programmatically extract data or Output."

ToU **没有点名** Sign in with ChatGPT；而官方开发者文档明确提供并描述该机制。两条并存时如何解释属于法律问题——**本调研只记录原文，不判定**。可确定的是：SIWC 是官方提供的授权机制（用户本人同意授权），不是"共享账号凭据"；但它是不是 ToU 语境下的"官方明确许可"，**未验证**（见下节）。

**结论（问题 3，OpenAI）**：**允许**（开源客户端路径，preview；官方文档明确支持）。Anthropic 与 OpenAI 在此**相反**。

---

## 4. 错误 UX

### OpenAI（官方给了完整清单，可直接采用）

来源：<https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery>（访问 2026-10-12）。原文（节选）：
- 拒绝授权/无计划权限："If the result contains a valid ID token without that permission, retain the sign-in but mark ChatGPT plan usage as disabled. … offer a clear choice: enable ChatGPT plan usage or configure another supported billing path, such as the user's own API key."
- 未授权代码："If the callback contains `error=access_denied`, validate `state`, stop the attempt, and do not exchange a code."
- 刷新失败："**Unusable refresh token:** `invalid_grant` on refresh, `invalid_refresh_token`, `token_expired`, `refresh_token_expired`, `refresh_token_invalidated`, or `refresh_token_reused`. Clear unusable tokens and repeat OAuth with the saved issued client ID."
- 用量超限（429 `subscription_sharing_usage_limit_exceeded`）："Pause new requests that use the user's ChatGPT plan and link to ChatGPT settings → Usage."
- 不适用（403 `subscription_sharing_user_not_eligible`）："Explain the restriction; do not repeat the same request or loop through OAuth."
- 暂时不可用（503）："Preserve credentials and retry later with bounded backoff."
- 撤销无通知："OpenAI does not currently notify your tool when a user disconnects the app in ChatGPT settings. … ask the user to sign in again. Do not erase credentials solely because of a temporary network or infrastructure failure."

来源：<https://developers.openai.com/siwc/ui-ux-guidelines>（访问 2026-10-12）：达到用量上限时 "Make **Manage usage** the primary action. Open ChatGPT usage settings"；使用计划时在 composer 附近显示 "Using ChatGPT plan"。

### Anthropic

第三方 OAuth 不存在，故第三方 OAuth 的错误 UX **无官方文档**。API key 的失败态有文档：<https://platform.claude.com/docs/en/manage-claude/authentication>（访问 2026-10-12）原文："After a key expires, requests made with it return a `401 authentication_error`. Create a new key to restore access; expired keys cannot be reactivated."

### 我们需要处理的失败态（逐条：用户看到什么、能做什么）

| 失败态 | 用户可见信息 | 能做什么 | 依据 |
|---|---|---|---|
| 授权被拒（`error=access_denied`） | "未授权使用 ChatGPT 计划；本次登录已取消" | 保留普通登录（若已建立）、提供"改用 API key"或"重试授权" | OpenAI errors 页（官方）；Anthropic 无此流程 |
| token 过期且刷新失败（`invalid_grant` 等） | "登录已失效，请重新登录" | 清不可用 token、用已存 `client_id` 重走 OAuth；不要自动重试循环 | OpenAI errors 页（官方） |
| 时钟偏移 | "登录校验失败（时间不同步）" | 提示校准系统时间；**官方未给该场景的专门指引，措辞与动作由我们设计** | 官方未覆盖（标注） |
| 回环端口被占 | "无法启动本地登录回调（端口 1455 被占用）" | 换一个可用端口（官方允许仅端口可变），重试 | OpenAI sign-in 页（端口可变是官方规定） |
| 无浏览器（ssh/无头） | "此环境没有浏览器，无法完成 OAuth" | OpenAI OSS 官方只描述回环回调，**未提供设备码/无浏览器路径**；建议引导用 API key | 官方未覆盖（标注） |
| 网络不可用 | "无法连接登录服务" | 保留凭据、有界退避重试；**不要**因网络失败清凭据 | OpenAI errors 页（官方："Do not erase credentials solely because of a temporary network or infrastructure failure."） |
| provider 侧撤销授权 | "ChatGPT 计划授权已被移除，请重新登录" | OpenAI 官方不通知；发现后停止用旧 token、让用户重新登录 | OpenAI errors 页（官方） |

---

## 未验证的部分

1. **没有做任何真实登录或注册**（任务硬约束），因此"OpenAI 的 OSS 动态注册对 Radian 这类工具是否立刻可用、是否还有额外审批"**未验证**——只能引官方文档的 "available to all open-source partners"。
2. **OpenAI ToU 与 SIWC 的精确法律关系**未验证：ToU 未点名 SIWC，本调研不判定"是否构成明确许可"。
3. **OpenAI 无浏览器/设备码路径**：官方 OSS 文档未提供，**未验证**是否存在未公开的流程。
4. **Anthropic 是否存在未公开的第三方 OAuth 合作**：官方文档未载；按硬约束"官方未说明，不采用"，不做推测。
5. **时钟偏移、端口占用、无头环境**的具体官方处理建议：官方未逐条覆盖（仅"端口可变"是官方规定），其余为我们自设计。
6. **官方文档的抓取限制**：`openai.com` 与 `platform.openai.com` 对普通 HTTP 客户端返回 403；本调研用 `developers.openai.com`（官方开发者站）与其 `.md` 文档源、以及带常规浏览器头的 HTTP/1.1 取到了 `openai.com/policies/*`。**未使用任何第三方镜像/搜索引擎缓存**。

## 对计划的影响

1. **首发只发 API-key provider：是。** 计划 §5 Phase 0b 写"未通过时只发布 API-key provider"——本调研结果是**部分**：对 Anthropic 是**不通过**（必须 API key），对 OpenAI 的 API-key 路径也仍是最简单、无 preview 风险的路径。
2. **OpenAI ChatGPT-plan OAuth：官方支持，可作为后续可选能力**（open-source 路径）。若做，依赖：① Responses API（我们当前是 Chat Completions adapter，需要新增/改造）；② 强制 `store:false` + `stream:true` 与一长串不支持字段；③ 动态注册 + 回环回调 + `0600` 凭据文件 + refresh 轮换；④ 它是 **preview**，要 feature flag、明确 UI 与降级到 API key 的路径。建议排在 Phase 2/3 之后，作为独立 provider 能力。
3. **Anthropic 订阅 OAuth：不做。** 官方明文禁止订阅路由；第三方 OAuth 也无官方文档。Radian 用 Claude 一律 API key（或 WIF，若将来做服务端部署）。
4. **ADR 0008 兼容**：OAuth token 本体放 `0600` 文件，配置里只写 `SecretRef { from: file, name }`（或 keychain）；不违背"secret 只存引用"。keychain 在无头/SSH 的回退到 `0600` 文件（对齐 Anthropic 官方自有工具的做法）。
5. **错误 UX**：OpenAI 的官方错误码与 UI 指引可直接落地；其余（时钟、端口、无头）由我们设计并在 ADR/实现里写清。

## 引用清单（全部于 2026-10-12 访问）

Anthropic：
- <https://platform.claude.com/docs/en/manage-claude/authentication>
- <https://platform.claude.com/docs/en/cli-sdks-libraries/cli/authentication>
- <https://code.claude.com/docs/en/authentication>
- <https://code.claude.com/docs/en/feature-availability>
- <https://www.anthropic.com/legal/consumer-terms>（Effective October 8, 2025）
- <https://www.anthropic.com/legal/aup>（页面标注 Effective November 12, 2026）
- <https://www.anthropic.com/legal/commercial-terms>（Effective June 17, 2025）

OpenAI：
- <https://developers.openai.com/siwc/quickstart>
- <https://developers.openai.com/siwc/website>
- <https://developers.openai.com/siwc/request-client-id>
- <https://developers.openai.com/siwc/token-sharing-open-source>
- <https://developers.openai.com/siwc/token-sharing-open-source/sign-in>
- <https://developers.openai.com/siwc/token-sharing-open-source/token-reference>
- <https://developers.openai.com/siwc/token-sharing-open-source/profiles-and-sessions>
- <https://developers.openai.com/siwc/token-sharing-open-source/errors-and-recovery>
- <https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations>
- <https://developers.openai.com/siwc/ui-ux-guidelines>
- <https://openai.com/policies/terms-of-use>（Effective January 1, 2026）
- <https://openai.com/policies/usage-policies>（Effective October 29, 2025）