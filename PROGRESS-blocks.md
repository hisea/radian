# PROGRESS: pi/blocks（ADR 0028 第 1 片：块模型、按块铺背景、thinking 折叠、代码块不折行）

Brief: scratchpad/briefs/adr28-s1.md · worktree ../radian-pi-blocks · base 8456074 · 并行: p3 /model, p4 /login, p8 auto（都碰 app.rs/render.rs 的命令/Modal/状态栏；本片只动 transcript 排版与绘制）

- [ ] C1 块模型（R1+R9）：blocks.rs（Paragraph/Code 扫描、增量重解析只重算最后一个块）、BodyLine 布局（chars/end/text）、Entry 缓存（块缓存键含折叠模式；行数缓存仍按宽度）、render/select/find 接线、select 映射保持正确、virtual/perf 不退化
- [ ] C2 按块铺背景：用户消息整行铺 user.bg + 上下空行；工具卡片三状态（新角色 tool_pending_bg/tool_success_bg/tool_error_bg，接 ADR 0029 映射/内建/[tui.colors]，mono 用修饰不铺色）；选区画在背景之上
- [ ] C3 notice 结构着色：label: value 纯函数切分，标签 accent（新角色 notice_label），正文保 ui-r1（新角色 notice_body ← muted，dark=默认前景）
- [ ] C4 thinking 折叠：tui.display.reasoning = collapsed|expanded|hidden（默认 collapsed）、▸ thinking (N lines) + 流式动态标记、toggle_reasoning=Ctrl-T
- [ ] C5 R9 验收测试（200 列代码块 80 列不折行带 …）
- [ ] C6 文字主次照 pi＼�：fg_source 分配照 pi（主=正文/用户/工具标题，次=工具输出/notice 值/thinking，弱=hint/截断/计数/补全说明，强调=标签）；内建 dark/light 按 pi 的 okhsl 亮度三层（89/67/56）重定（替换 ADR 0029 'dark 逐字节不变'约束，��报告列出截图测试变化）；主题文件照原样用，不自动推导
- [ ] C7 16 社区主题兼容性测试：@firstpick/pi-themes-bundle@0.1.7 拷进 tests/data/themes/（附 LICENSE.pi-themes-bundle，README 登记）；测试 17 主题零警告 + 主/次/弱三层取到值；有警告列报告、不改主题文件
- [ ] C8 MUTATIONS（≥5 条）+ perf-baseline 文档
- [ ] C9 Verify 全套 + pty×3；删除本文件后回报

设计要点（select 映射）：布局是唯一事实——BodyLine{chars,end,text} 同时供渲染与 select 的 cell→(entry,offset) 映射；用户块的上下空行在布局里是 chars=[] 的 gap 行（点击映射到缝，复制不出文字）。因此 selection 的坐标测试要按新几何更新（用户条目 +1 行偏移），复制语义测试原样通过。
