# 沙箱档 profile 的构造要拿到 `Sandbox` 实例，不再凭一个 `bool`

- 状态：decided＼�外部审核 P1-1 采纳）
- 涉及 ADR：0012（规则与档位）、0016（`workspace` 档的沙箱）、0043 §3、§5
- 来源：外部审核 2026-10-20 第三节 P1-1；核对记录见本条背景

## 背景

`Profile::workspace_write_sandboxed(workspace)` 是公共构造函数，产出「沙箱内 shell 一律 `allow`」的 profile，但只凭调用者约定：三处调用点（TUI 启动、`/access` 切换、`radian exec`）都是先 `build_sandbox` 再传 `sandbox.is_some()`，注释写着「两边不能漂移」。任何未来路径跳过第一步直接调第二步，就会在没有 OS 约束的情况下免审批。`radian-permissions` 已经依赖 `radian-sandbox`（`SandboxMode`、git 元数据清单），所以收一个 `&Sandbox` 不需要倒转依赖。

## 决定（已定）

- `workspace_write_sandboxed` 的参数从 `bool` 推导改为接收 `&radian_sandbox::Sandbox`（或它的一个不可在 crate 外构造的凭据类型）；拿不到实例就造不出放行的 profile。`sandboxed: bool` 字段由构造函数自己置位，不再有外部可传的布尔。
- 三处调用点改成直接传 `Sandbox`；`build_profile` 的签名随之改为 `Option<&Sandbox>`。TUI 与 `exec` 两份接线保持逐字一致。
- 不改规则内容、不改档位语义；这是类型约束补强，不是行为变化。

## 验收

- 编译期：删掉任一调用点的 `Sandbox` 传递即编不过（在 PR 说明里写明试过）。
- 现有 `workspace`/沙箱有无的 pty 与权限测试全部仍绿；变异：把构造函数里的 `sandboxed = true` 去掉 → 测试红。
- `crates/radian-permissions/README.md` 与 ADR 0012 相应一句话更新。
