# arc：iOS 客户端

- 状态：idea（形态、动作与范围已定；仓库位置、分发方式、最低系统版本未定）
- 涉及 ADR：0048（§11 owner 动作与二次确认）、0034（通知内容的隐私口径）
- 来源：`arc-serve.md`（服务端接口）；`research/2026-10-22-arc-app-product-design.md`

## 已定

- **原生 iOS app，SwiftUI。** 不做 PWA。它永远是客户端，只连 `arc-serve.md` 的 hub 接口，不读 arc 文件，不跑循环。
- **一个 app 看所有仓库**：连一个 hub，hub 管多个仓库（`arc-serve.md` 第 1 条）。
- **配对**：扫 hub 打印的二维码，拿到地址与 token；token 存 Keychain。
- **owner 动作**：`confirm`、`merge`、`reject`（必须填 note）、`cancel`、`auto`、`pause`、`reverify`、`priority`。merge、reject、把 auto 调到 `merge`、cancel 先弹二次确认，与 TUI 相同（ADR 0048 §11）。
- **v1 能看的**：plan 与 task 列表（需要你处理的排最前，与 TUI needs-you 行一致）、task 详情（state、phase、gate 结论、最终报告、diff）。不看实时会话。
- **通知**：app 自己不收推送，没有 APNs 集成。锁屏通知由 arc 钩子经 Bark 或 ntfy 送达（`arc-serve.md` 第 4 条）；app 注册一个 URL scheme，示例钩子把通知链接设成 app 的 task 页，点开后 app 连 hub 取正文。
- **事件**：前台时连 hub 的 SSE 流刷新；回到前台或重连时先重新读列表（事件不重放）。

## 待决问题

- 代码放在这个仓库（如 `apps/ios/`）还是单独一个仓库：Swift 工具链与 CI 和 Rust 工作区完全不同。
- 分发：个人 Apple 开发者账号 + TestFlight，还是只在自己设备上装。
- 最低 iOS 版本。
- Bark / ntfy 的通知链接能否直接打开第三方 app 的 URL scheme：落地前在真机上核对。
- 第二版是否加实时会话（需要 hub 投影 journal，见 `arc-serve.md` 第 3 条末尾）与 app 自带推送（需要 APNs 密钥与一个新的出站网络面，按 ADR 0013 默认关闭）。

## 验收

未定。至少：手机在 tailnet 内扫码配对后，能浏览两个仓库的 plan 与 task，完成每个 owner 动作且二次确认生效，锁屏通知点开直接到对应 task 页。
