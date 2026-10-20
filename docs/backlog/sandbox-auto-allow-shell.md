# workspace 档沙箱内自动放行 shell

- 状态：idea
- 涉及 ADR：0012、0016、0043
- 来源：`docs/research/2026-10-18-area-deep-dive/README.md` 第 8、16 条

## 背景

workspace 与 full 两档 shell 都是 `ask`（`crates/radian-permissions/src/profile.rs:139-170`），dogfood 只能用 `full + --yes`。

## 待决方向

沙箱内自动放行，越界（沙箱拒绝）时模型显式请求才问人，绝不自动去沙箱外重跑；subagent 继承父会话规则。复合命令逐段判断，授权按前缀、加禁止清单、按项目持久化。

## 前提（必须先解决）

Linux bwrap 不限制 Unix socket（`docs/sandbox.md`：已实测、接受为已知限制，决定 4）。owner 必须先重开这一条（seccomp 堵上），否则不能做。
