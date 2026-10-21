# full 档 MCP 按 server 首次询问

- 状态：idea
- 涉及 ADR：0012、0043
- 来源：`research/2026-10-18-area-deep-dive/README.md` 第三轮安全小节（注：没有独立的 `13-security.md` 文件，内容在总报告 README 里）

## 背景

缺省 `full` 档下 MCP 调用直接放行（`profile.rs` 已核对），MCP 进程也不进沙箱，一次提示注入就能外泄秘密。

## 待决方向

`full` 档 MCP 改成按 server 首次询问，并对工具定义做哈希固定。

## 验收

新 MCP server 首次调用必问一次；工具定义被掉包时能发现（待决：失败语义由落地时定）。
