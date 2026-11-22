# 删掉 footer 的 `command` 项

- 状态：decided
- 涉及 ADR：0027、0008、0013
- 来源：2026-11-21 精简调查

## 背景

footer 的 `[tui.footer.items.<id>] command` 让用户配置一个定时运行的外部命令，把输出画进状态行（`docs/user/footer.md`）。这是 ADR 0027 唯一公开的扩展点，实现是 `radian-host` 的 `command_footer.rs`（约 32 KB）加测试。pi 的 footer 是固定的；内建项（git 分支、用量等）用的是内部 `StatusContributor`，不依赖这个公开扩展点。

## 决定（已定）

- 删掉 `command` 项：`command_footer.rs`、`CommandContributor`、`command` 与 `period` 参数、对应文档和测试。
- 已有配置里带 `command` 或 `period` 的项：一行警告后按未知 id 处理（ADR 0008 显示类键只警告不拒绝），`footer.md` 写明这两个参数已退役、inert。
- 内部 `StatusContributor` 机制保留，只服务内建项；ADR 0027 删掉"公开扩展点是 `command` 组件"一句，改成没有公开扩展点。
- footer 预设不在本条目里改。

## 验收

带 `command` 项的旧配置能启动，footer 只显示警告提到的 id 文本或空；内建项照常刷新；`footer.md`、`configuration.md` 与 docs 漂移测试一致。
