# arc scope：一个仓库里的子项目作为任务的范围

- 状态：decided
- 涉及 ADR：0048（§3 task 字段、§7 worker 的 cwd 与 `--workspace`、§8 delivery check、§10 gate 的 `commands` 锁与 review 的 diff、§15 `config.toml`）、0036（context 文件从工作区向上到仓库根）、0025（项目 skill 向上查找）、0049（ACP `session/new` 的 `cwd`）、0008（配置 schema）
- 来源：owner 的问题"radian 跟 git 耦合深，对 monorepo 支持不好"；tkrs 仓库（README 自述"仓库放多个子项目，第一个是 `loh/`"）

## 背景

会话层几乎不依赖 git：`repo_root` 只向上找 `.git`，用来给 context 文件、项目 skill 的向上查找和 `@import` 画边界（0036、0025）；trust 和 `.radian/config.toml` 按目录记（0031、0043）；沙箱发现 `.git` 只为把它设成只读（0038）。没有 git 的目录也能跑。

把"一个 git 仓库"当成结构单位的是 arc（0048）：仓库 = `git rev-parse --show-toplevel`，一个仓库一个 store、一个 main、一把 `runner.lock`；worker 的 cwd 和 `--workspace` 固定是 worktree 根；gate 的 `commands` 在 worktree 根运行，整个仓库一把 `commands` 锁；review 看整仓 diff。

tkrs 里的表现：47 个 task 有 46 个在正文里写 `loh/`，worker-brief 靠一条文字规则"只改任务点名的子项目"，gate 命令全是 `cd loh && …`。flow 同时承担了 worker 种类（`default` = radian，`image` = codex）和子项目两个意思，第二个子项目一来就要写 flow 的笛卡尔积并复制 gate profile。review 没法说"越出 `loh/` 的改动要单列"；两个子项目互不相干的 gate 也得排队。不是痛点的：checkout 与 commands 阶段的时间（3–28 秒），冲突（是 docs 热点文件，和子项目无关）。

## 决定（已定）

1. **scope 是和 flow 正交的第二个维度。** `config.toml` 加 `[scopes.<name>] path = "<仓库根下的相对路径，不含 ..>"`，可选 `gate_profile`。没有 `[scopes]` 的仓库行为不变：隐式 scope 是仓库根。
2. **task 字段 `scope`**（一个 scope 名）；**plan 字段 `scope`** 作其 task 的缺省；都没有 = 仓库根。未定义的 scope 和未定义的 flow 一样：claim 拒绝并点名，`start` 退出 `3`，task 留在 `todo.ready`。
3. **worker**：cwd 和 `--workspace`（ACP `session/new` 的 `cwd`，command worker 的工作目录与 `ARC_WORKTREE` 之外新增 `ARC_SCOPE`）= `<worktree>/<path>`。context 文件与项目 skill 按 0036、0025 从该目录向上到仓库根叠加，brief 不再需要 `cd`。delivery check 不变：仍看整个 worktree 的 `git status`。
4. **gate**：`commands` 与 `key_commands` 的 cwd = `<checkout>/<path>`；`commands` 锁按 scope 分，不同 scope 的 commands 阶段可并行，同 scope 仍排队；integration 不变，仍是一个 main。
5. **review**：prompt 里的 diff 只含 scope 内的路径；scope 外的改动路径作为固定的一段列给 reviewer，并由 arc 机械地加一条 `major` finding（`change outside scope <name>: <paths>`）；task 字段 `allow_outside_scope = true` 关闭这条 finding。
6. **gate profile 的优先级**：task 的 flow 的 `gate_profile` 与 scope 的 `gate_profile` 都设时，scope 优先（scope 决定"验什么"，flow 决定"谁来做"）；都没设用 `gate.profile`。
7. **不做**：一个 git 仓库里多个 arc store（会在同一个 main 上互相抢，`main 动了` 的逻辑假设自己独占 main）；稀疏 worktree（`--no-checkout` + `sparse-checkout`，checkout 时间不是瓶颈，需要时另立 idea）；VCS 抽象层。
8. **tkrs 迁移**：`[scopes.loh] path = "loh"`，plan 缺省 `scope = "loh"`，gate 命令去掉 `cd loh &&`，worker-brief 删掉"只改任务点名的子项目"那条。

## 待决

- `list` 与 TUI 是否显示 scope 列：建议和 flow 一样，仓库定义了 `[scopes]` 才显示。
- 一个 task 要改两个子项目时的写法：建议不支持，拆成两个 task；若要支持，是 `scope` 取数组还是 `allow_outside_scope`。

## 验收

- 没有 `[scopes]` 的仓库：现有测试不变，`init` 写出的 `config.toml` 字节不变。
- 有 scope 的 task：worker journal 的 `SessionCreated.workspace` 是 `<worktree>/<path>`；启动行的 context 文件同时列出 `<path>/AGENTS.md` 与仓库根的 AGENTS.md；command worker 的工作目录是同一目录。
- 两个不同 scope 的 task 的 commands 阶段同时运行；同 scope 的排队。
- review 的 diff 只含 scope 内路径；scope 外改动产生那条 finding，`allow_outside_scope` 关掉它。
- 未定义的 scope：claim 拒绝并点名，`start` 退出 `3`。
- 手册：`docs/user/arc/files.md` 加每个键与字段（类型、缺省、取值、生效时机），drift test 读同一个闭集。
- 落地时 0048 在 docs-only commit 里改 §3、§7、§10、§15，并删掉本文件。
