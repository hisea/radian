# `full` 档全部放行，不弹任何内建提示（像 pi）

- 状态：decided＼�"full 档我想全都不限，就像 pi"）；三处细节待 owner 确认，见下
- 涉及 ADR：0012（主：`full` 档的规则表，"`full` 不等于无限自动运行"一句反转）、0043（§3 缺省 `full + on-ask`、四类沙箱场景、1a.7 子会话审批）、0016（档位描述）、0038（切到 `full` 的确认面板文案）、0031（信任门的沙箱选项）、0036（提示词里关于审批的句子，如有）
- 来源：��背景在 `docs/research/2026-10-18-area-deep-dive/7-permissions-sandbox.md` §1、§5.2

## 背景

缺省档位是 `full`，没有沙箱，但 shell 仍是 `ask`（`profile.rs` 的注释："full-access is not run anything silently"）。dogfood 里 shell 占工具调用的 53%，每两次调用停一次，大家只好 `--yes`。owner 的目标：缺省设置少摩擦、高效率，保一定安全。决定把 `full` 做成 pi 的模样：不问，安全交给用户的环境（容器、专用 OS 用户，或者改用 `workspace`）。

这样两档的分工变得干净：**`full` = 信任环境、不问；`workspace` = 沙箱强制、沙箱内不问（`sandbox-auto-allow-shell.md`），越界才问。** 审批层剩下的用途只有：`workspace` 的越界请求与 MCP、用户自己写的 `ask`/`deny` 规则、`approval = always` 的逐条审阅。

## 决定（已定）

- `full_access()` 里 shell 的 `Ask` 改为 `Allow`；MCP 已经是 `Allow`；`write`/`patch` 维持工作区内 `Allow`。文件工具按 ADR 0039 的 fd 级实现本来就写不到工作区外，这是工具能力不是权限限制，不改。
- `full` 档下 `--yes` 没有作用（没有 `ask` 可跳过）；无头 `radian exec` 在 `full` 档不带 `--yes` 也不再因 ask 被拒。`--yes` 对 `workspace` 档保留（越界请求自动放行）。
- ADR 0012 规则 5（非人来源只降不升）保留原文，在 `full` 档下没有可降的东西；`workspace` 档不变。
- `subagent` 子会话继承 `full` 就等于不限（0043 1a 不宽于父，仍成立）。
- 启动行与 footer 改成明说：`full · no sandbox · no prompts`；`/access` 切到 `full` 的确认面板（0038 决定 1）文案加一句"full 不开沙箱也不弹审批"。
- 文档：`crates/radian-permissions/README.md` 删"full-access is not run anything silently"；`docs/sandbox.md` 与 README 加一段像 pi `docs/security.md` 的声明：`full` 信任你的环境，想要边界就用 `workspace`、容器或专用 OS 用户。
- 相关事项的归属：7-permissions ③（`shell-compound-allow.md`）与 ⑥（`shell-readonly-allow.md`）照做，它们在 `full` 下被整体 allow 覆盖，实际作用在 `workspace` 档与 `approval = always`；"`full` 档 `.git/hooks` 写要问"的提议不做（`workspace` 档的同类问题见 `workspace-git-exec-entries-file-tools.md`）。

## 待 owner 确认的三处细节

1. **用户自己的 `deny`/`ask` 规则在 `full` 档是否仍生效。** 本文按"仍生效"写：不限指的是没有内建提示，用户显式写的规则是用户的意思，和 pi 的 permission-gate 扩展同类。
2. **信任门的缺省选项。** `full` 不问之后，第一次进新仓库时信任门（0031/0043）那一个选项"这个项目用沙箱"成了唯一的安全决定：一个陌生仓库的 `AGENTS.md` 注入在 `full` 下会直接跑 shell。建议未受信任的项目在对话框里把 `workspace` 放在第一位（缺省高亮），受信任的项目不变。这改 0031，由 定。
3. **`approval = always` 是否保留**：保留则 `full` 下仍可逐条审阅，成本是一个配置值；不保留则 `full` 彻底没有审批路径。本文按保留写。

## 验收

`full` 档下 `rm -rf x`、`curl … | sh`、`cargo test && git push` 都不出现提示，journal `PermissionResolved.approver = policy`；`radian exec --profile full` 无终端、不带 `--yes` 能跑 shell；用户 `--deny "rm -rf"` 仍拒绝（变异：deny 被忽略，测试应失败）；`workspace` 档 shell 的行为不变（在 `sandbox-auto-allow-shell.md` 落地前仍 `ask`）；启动行、footer、`/access full` 面板三处文案含"no prompts"（文案由测试守着）；0012 的 `MCP servers are NOT sandboxed` 披露不变。0012、0043、0016、0038 在 docs-only commit 里同步。
