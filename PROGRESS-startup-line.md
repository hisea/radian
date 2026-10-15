# startup-line — 进行中

HEAD: (待提交)
分支 pi/startup-line, 从 09f5cd8 开。

## 任务
1. 启动摘要行的沙箱词取自真实后端: host 算 `sandbox_word` (workspace=backend_name, safe=no exec,
   full=no sandbox), `SessionFacts.sandbox_word`; TUI 直接用, 不再搜文本。
2. describe.rs 的 `**on**` → `on`; README.md:230 同步。
3. 不动 radian-coding/src/shell.rs。
测试: describe 单测 (sandbox_word 传值 / 无 Markdown) + pty 摘要行含真实后端名、不含 none。

## 排队 (startup-line 做完后)
1. **brand-config** (ADR 0027, main 最新; glyphs 已合 main 6d37853): [tui] brand 配置项,
   footer 的 brand 条目与 startup.rs 的一行回退共用; 不写时用 glyphs 档默认; ≤4 显示宽度, 超出
   报配置错误; schema/fixture/ADR 0008 表同步; 测试两处都用配置值、缺省走档位默认、超宽报错;
   变异各一条。从最新 origin/main 开 pi/brand-config。
2. **glyphs2** — 暂缓通知)
ADR 0027 (main ab2ae4b):
- 参考字体去掉 Operator Mono → 六款。
- unicode 档改用 token ↑/↓、成功 ✓、失败 ×、折叠 ›、截断 …; spinner 默认 ▖▘▝▗
  (braille/arc 可选, README 写明需要字体支持)。
- 加上下文填充线 (percentage 一档) 和 tui.footer.transparent。
- 从最新 main 开 pi/glyphs2 做。
