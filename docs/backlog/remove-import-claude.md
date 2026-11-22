# 删掉 `radian config import-claude`

- 状态：decided
- 涉及 ADR：0046、0045、0008
- 来源：2026-11-21 精简调查

## 背景

`radian config import-claude` 把 Claude Code 的配置一次性搬进 radian（ADR 0046）。实现 `config/migrate_claude.rs` 约 110 KB，测试 `claude_migrate.rs` 约 64 KB，是 config 目录里最大的一块。radian 的定位是 pi 同类，迁移入口是 `import-pi`（`docs/user/pi-migration.md`），保留。

## 决定（已定）

- 删掉 `import-claude` 子命令、`migrate_claude.rs`、它的测试和 fixture。
- ADR 0046 改为 `Status: removed`，写明移除，正文只作历史（同 ADR 0048 的写法）。ADR 0045 里提到 import-claude 的句子删掉。
- `docs/user/cli.md`、`configuration.md`、`hooks.md` 里的 import-claude 段删掉。
- 运行 `radian config import-claude` 时报"未知子命令"，按 CLI 的通常规则退出。
- `import-pi` 不动。

## 验收

`radian config --help` 不再列 import-claude；`import-pi` 的测试照常通过；文档和 docs 漂移测试里没有 import-claude。
