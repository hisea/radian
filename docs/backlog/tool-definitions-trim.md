# 工具定义瘦身

- 状态：idea
- 涉及 ADR：0043、0037、0021
- 来源：`docs/research/2026-10-18-area-deep-dive/2-tools.md` P4（含 lead 的变体）

## 背景

17 个工具约 20 KB（`scratchpad/tools2/radian_specs.py` 口径）。定义在可缓存前缀里，所以省的是上下文窗口与首轮 prefill，不是每轮价格。

## 待决方向

合并 `job_list`、`job_output`、`shell_output` 为一个"读命令输出"工具；合并 `memory_save`/`memory_revise`（或随 memory-provider 项一起删）；schema 里删掉复述描述的文字；别名只保留一个版本。合并工具要写 ADR 0043 修订并给一个版本的别名；只删描述文字不需要。

## 验收

重跑字节数脚本确认降到约 12–13 KB，评测集成功率不降（删掉的描述里有些是 dogfood 踩坑后加的，不能只凭直觉）。
