# 网站文档生成器移出 TUI crate

- 状态：decided
- 涉及 ADR：0032
- 来源：2026-11-21 精简调查

## 背景

`radian-tui/src/site_docs.rs`（约 64 KB）把 `docs/user/*.md` 渲染成 getrad.dev 的静态手册（sitemap、llms.txt、JSON-LD、双语页），只被它自己的测试、`examples/site-docs.rs` 和 Pages workflow 使用。`radian docs` 用的是 `radian_coding::docs::DOCS`，不经过它。它编进了 `radian` 二进制所在的 crate。

## 决定（已定）

- 把网站生成器移到一个不进 `radian` 二进制的地方：workspace 里一个单独的 `publish = false` 小 crate（或 xtask），Pages workflow 改调它。
- 生成结果逐字节不变（用现有测试守护），本条目不改网站内容。
- `radian-tui` 里不再有 site_docs 代码；`radian docs` 不受影响。

## 验收

`radian` 二进制不含网站生成代码；Pages workflow 生成的站点与移动前一致；Verify 全绿。
