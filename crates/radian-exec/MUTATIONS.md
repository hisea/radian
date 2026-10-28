# `radian-exec` sessions-on-disk mutation record (Phase 2 / B)

New rows use the id `<task id>-M<n>` (for example `TK0034-M1`); see "Mutation records" in `AGENTS.md`.

Base: `3451dc1` · one-shot worktree `/tmp/cli-mut` (deleted after) · each mutation run with
`cargo test -q -p radian-exec --test cli --no-fail-fast`; `git checkout --` after each;
`git status --short` empty at the end.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| M1 | `--resume` opens the session with an empty history (`Vec::new()` instead of the recovered one) | `resume_carries_the_previous_history` | ✅ failed |
| M2 | terminal input is enqueued with `source = System` instead of `User` | `a_new_session_writes_a_valid_jsonl` (asserts the durable `InboxEnqueued.source`) | ✅ failed |
| M3 | reasoning is printed to stdout with the answer (no `[reasoning]` split) | `reasoning_is_printed_apart_from_the_answer` | ✅ failed |
| M4 | the writer ignores `try_lock_exclusive`, so a second process opens the same session | `a_second_process_cannot_open_the_same_session` | ✅ failed |
| M5 | the `SessionCreated` snapshot carries the `--header` values | `the_session_file_contains_no_key_or_header_values` | ✅ failed |
| M6 | `--list` ignores the workspace filter (`list(None)`) | `list_shows_only_the_current_workspace` | ✅ failed |
| M7 | recovery does not run the core's `repair`, so an unpaired tool call is not classified | `a_killed_session_resumes_without_replaying_the_tool` | ✅ failed |
| M8 | recovery ignores the transcript when deciding admission (reverting the fix) | `a_killed_session_resumes_without_replaying_the_tool` (the input is replayed → an extra request → the mock runs out of replies) | ✅ failed |

8/8 caught, none equivalent.

## Bugs this slice found (kept as regression tests)

- **Ids collided across processes.** The inbox's numeric id counter restarted at 1 in a new
  process, so a resumed session saw the previous process's `inbox-1` and the new one's
  `inbox-1` as the same input. Fixed with a per-process id prefix; pinned by
  `radian-runtime::inbox::tests::two_inboxes_never_generate_the_same_id`.
- **`InputAdmitted` was written more than once per input.** `admit_seen` never recorded what
  it had written, so every later step in the turn wrote it again. Pinned by
  `radian-runtime::driver::each_input_is_admitted_once`.
- **A crash between "committed at the start of the step" and "admission recorded" replayed
  the input.** Recovery now treats the transcript (via `meta.id`) as authoritative and writes
  the missing record; pinned by
  `radian-runtime::recovery::an_input_in_the_transcript_without_an_admission_record_is_not_replayed`.

## Follow-up: the flaky `second_sigint_exits_130` (2026-10-12)

Review measured `second_sigint_exits_130` failing 2/15 on this branch (exit 0 instead of
130) and 0/15 on `main`. Root cause, found by instrumenting the CLI's stderr with timestamps:

- **`tokio::signal::ctrl_c()` does not buffer.** Every call creates a fresh `watch`
  receiver (`signal(SignalKind::interrupt())?.recv()`), so a SIGINT that arrives while no
  `ctrl_c()` future is registered is lost. The old CLI spawned one task that awaited the
  first and then the second `ctrl_c()` back to back; this slice's loop awaited a *fresh*
  `ctrl_c()` in `tokio::select!` on every iteration, so the second Ctrl-C could fall into
  the gap and never reach `on_signal` (the failing run's log showed the turn then finishing
  normally and the CLI leaving 0).
- **The grace period is still fully waited.** The same instrumented run showed
  `run finished Cancelled(Requested)` ~2031 ms after the first signal — the full 2 s MCP
  `cancel_grace` (cancel + ping never answered by `fx__freeze`). So this was a CLI signal
  plumbing regression, not a runtime behaviour change.

Fixes (commit `8cd481d`):

1. One long-lived listener task loops on `ctrl_c()` and forwards into a channel, so no
   signal can fall between two calls.
2. `next_event`/`wait_for_turn` select with `biased` and the signal branch first, so a
   pending Ctrl-C beats a ready EOF or a turn that just ended.
3. The second Ctrl-C of an open wind-down always forces, even if the turn ended in the
   meantime.

The test no longer sleeps: it waits for the CLI's `Ctrl-C again to quit` marker (the state
gate proving the first signal was handled while the turn is winding down), sends the second
signal, keeps stdin open while waiting (closing it would race an EOF against the signal),
and asserts the forced exit happened in under 1 s (i.e. it did not wait out the 2 s grace).

| # | Mutation | Result |
|---|---|---|
| M-S1 | remove the "second Ctrl-C of an open wind-down forces" check | ✅ caught: `second_sigint_exits_130` fails with `the CLI did not exit while stdin stayed open` (the forced exit never happens) |
| M-S2 | revert to a fresh `tokio::signal::ctrl_c()` per loop iteration | ❌ not caught in 12 runs (0/12): with the state-gate test the second signal is sent while the loop is already awaiting, so the lost-signal window is too narrow to hit reliably. The single listener removes the whole class anyway; the original 2/15 reproduction is the evidence that the window was real. |
| M-S3 | the forwarding task loops on a fresh `tokio::signal::ctrl_c()` per signal instead of one persistent `unix::signal` subscription | ✅ caught, but only under a pinned scheduler: six busy loops and the test pinned to the same four cores hang about one iteration in seven (the second Ctrl-C falls into the re-subscription gap; the hang-time stderr shows the turn still ending Cancelled and the prompt coming back, then silence). The persistent listener trips zero in 300 pinned iterations against the same load. |

`second_sigint_exits_130` was then run **30 times** with `--no-fail-fast`; all 30 passed
(0 failures). Outputs: `/tmp/a66-30runs/run-{1..30}.txt`. Unpinned stress stayed green
later too (60 + 100 iterations); the gap only shows with the test and the load pinned
to the same cores, where the forwarding task is preempted between forwarding the
first signal and re-subscribing. Non-unix keeps the portable `ctrl_c()` loop: no
persistent subscription exists there.

## Profiles, sandbox and permissions (Phase 3 / cli-tools)

One-shot worktree `/tmp/cli-tools-mut` at `9f8a5bc`, `cargo test -q ... --no-fail-fast`,
`git checkout --` after each; `git status --short` empty at the end.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| M1 | `default_profile()` returns `full-access` | `full_access_is_not_the_default_and_no_write_tools_without_a_workspace` | ✅ failed |
| M2 | an injected probe failure returns `Ok(None)` (degrade to unsandboxed) | `a_failed_sandbox_probe_refuses_instead_of_degrading` | ✅ failed |
| M3 | `Action::Deny` is downgraded to `Ask` (so `--yes` would allow it) | `yes_does_not_bypass_a_deny_rule` | ✅ failed |
| M4 | the shell `allow` matcher drops the "simple command" requirement and matches across separators | `a_composite_shell_command_always_asks` | ✅ failed (see note) |
| M5 | the permission layer stops racing the approver against cancellation | `cancelling_at_the_shell_approval_does_not_hang` | ✅ failed (the CLI hangs; the test's watchdog fires) |
| M6 | `observe_input_source` overwrites instead of only lowering | `radian-permissions::the_turn_source_only_ever_lowers` | ✅ failed |
| M7 | `workspace-write` builds the sandbox as `full-access` | `shell_cannot_write_outside_the_workspace` | ✅ failed |

Notes:

- **M4's first form was an equivalent mutation**: normalizing separators alone changed
  nothing, because `simple_prefix_matches` still returned early on `!is_simple`. Both edits
  are needed; with both, the composite is allowed and the test fails.
- **"A session grant survives `--resume`" has no natural mutation.** The grant lives only in
  the `PermissionHooks` instance (a `HashSet` in memory) and is never serialized anywhere, and
  `--resume` is a new process. Making it survive would mean persisting it — a new feature, not
  a one-line change — so the property is structural; `radian-permissions` covers the layer
  (`a_session_grant_applies_to_the_next_call_and_not_after_resume`) and the CLI test covers
  the process boundary.
- M5 was run with a 120 s cap; the test's own watchdog is what fires.

## D2：CLI 的会话创建顺序（2026-10-13）

一次性检出 `/tmp/dogfix-mut`（`git worktree add --detach HEAD`，HEAD = `fb6780b`），变异后
`cargo test -p radian-exec --no-fail-fast a_failed_probe_leaves_no_session`，用 `git checkout --`
恢复、`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M9 | `chat` 在检查之前创建会话文件（把 `open_journal` 提到 `profile`/`sandbox`/`coding` 之前） | ✅ failed | `a_failed_probe_leaves_no_session_and_the_retry_creates_one`（`left files behind: ["retry.jsonl"]`） |

「先建会话再检查」的 CLI 实例。工作区错误在 `setup` 里已经验证，所以用探测失败（sandbox 在
`chat`）更能直接观察「检查失败仍留下文件」。

## --export / --search（2026-10-13）

一次性检出 `/tmp/clisess-mut`（`git worktree add --detach HEAD`，HEAD = `4d973d4`），每个变异用
`git checkout --` 恢复、`git status --short` 为空；`cargo test -p radian-exec --no-fail-fast
<精确测试名>`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M10 | 导出不把本进程的 key/`--header` 值传给 `with_secret` | ✅ failed | `export_redacts_the_key_and_header_values`（`the header leaked: ...`） |
| M11 | 默认打开推理（`include_reasoning(true)` 写死） | ✅ failed | `export_omits_reasoning_and_provider_payload_by_default`（默认导出含 `SECRET-REASONING`） |
| M12 | `--export` 不按工作区过滤 | ✅ failed | `export_refuses_another_workspaces_session`（退出 0 而不是 2） |
| M13 | `--out` 已存在时直接覆盖（去掉 `--force` 检查） | ✅ failed | `export_out_refuses_to_overwrite_without_force`（第二次退出 0） |

4/4 抓住，无漏网、无等价变异。M10 的 key 断言由 `sk-` 模式兜底，所以真正抓变异的是 header 值那一条
（header 值没有内置模式，只有 `with_secret` 会把它换掉）。

## 第二轮：--model 必填性与 search 的索引位置（2026-10-13）

一次性检出 `/tmp/clisess2-mut`（base `d4034ba`），每个变异用 `git checkout --` 恢复，
`git status --short` 为空；`cargo test -p radian-exec --no-fail-fast <精确测试名>`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M14 | 交互模式不带 `--model` 也能启动（去掉 clap 的 `required_unless_present_any` 并让 `model_name` 默认成 `"m"`） | ✅ failed | `a_conversation_still_requires_a_model`（不再有 clap 的 `Usage:` 行；实际是后面的 “no API key” 退出 2） |
| M15 | `--search` 的缓存写成 `.jsonl` 名（`index.db` → `search.jsonl`），即看起来像新建了一个会话文件 | ✅ failed | `session_commands_do_not_touch_the_session_directory`（`created or modified a .jsonl file`，`index.db` 也不再出现） |

M14 的断言特意包含 clap 的 `Usage:` 行：只断言 `--model` 字样的话，后面的 setup 错误也含它，变异就抓不住
了（第一次写测试时正是这个洞）。M15 覆盖 brief 要求的「search 写进了一个新的 .jsonl」。

## 第三轮：--export 的 session id 校验（2026-10-13）

一次性检出 `/tmp/clisess3-mut`（base `421221b`），变异后
`cargo test -p radian-exec --no-fail-fast export_refuses_a_path_like_session_id`，用 `git checkout --`
恢复、`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M16 | `--export` 不调用 `validate_session_id`（直接拿 id 拼文件名） | ✅ failed | `export_refuses_a_path_like_session_id`（`../x` 读到上级伪造文件，退出 0） |

测试里的伪造文件 `dir/x.jsonl` 的 `SessionCreated.workspace` 故意写成当前工作区，所以只靠
工作区比对是挡不住的；M16 证明去掉 id 校验后它会被导出，而校验在场时被拒。

## `--version` (2026-10-13)

Base HEAD = `8e5fc47`. Source restored from a copy in `target/mut-bak/` and compared with
`cmp` (`git status --short` shows only the intended edits afterwards); the mutation was run
in place with `cargo test -p radian-exec --test version --no-fail-fast`.

| # | Requirement mutated | Mutation | Caught by |
|---|---|---|---|
| V1 | `--version` exists and prints the package version | `version` removed from the `#[command]` attribute (clap then has no `--version` at all) | `version.rs::version_prints_the_program_name_and_the_cargo_package_version` FAILED (exit code 2 instead of 0: clap rejects the unknown flag) |
| V2 | the version comes from `Cargo.toml`, not a literal | `version = "7.7.7"` written into the attribute instead of the bare `version` (which is `crate_version!`) | `version.rs::version_prints_the_program_name_and_the_cargo_package_version` FAILED (same assertion, with `--version must print the package version 0.1.0`) |

2/2 caught. The test also pins the exact line (`agent <version>` on stdout, empty stderr), so
a version that is printed to stderr, or that uses the package name (`radian-exec`), fails too.

V2's complement (the version really tracks `Cargo.toml`) was checked by setting both packages
to `9.9.9`: the binaries then print `9.9.9` and the tests stay green — no assertion is pinned
to the literal `0.1.0`.

## 追加（smallfixes，D15 报错与 D14 清理）

Base `f552250` · one-shot worktree `/tmp/small-mut` · `--no-fail-fast`；每个变异后从副本还原，
`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M10 | 已存在会话的报错不含 `--resume <id>`（换成 "it holds a complete session"） | ✅ 抓住 (rc=101) | `session_id_already_exists_points_at_resume` |
| M11 | `ScratchDir::drop` 不删除目录 | ✅ 抓住 (rc=101) | `scratch_directories_are_removed_when_the_guard_drops` |

M10 的变异只在报错字符串上；测试还断言已有 `.jsonl` 字节不变，所以「顺手覆盖文件」也会被抓。

## ADR 0016：档位别名（2026-10-13）

一次性检出 `/tmp/tiers-mut`（base `9c060d2`），`--no-fail-fast`，变异后 `git checkout --` 恢复。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M10 | 删掉 `ProfileArg` 的 `alias = "workspace-write"` / `alias = "full-access"` | ✅ failed | `tier_names_and_aliases_are_accepted_but_help_lists_only_new_names`（旧名不再被接受） |

## ADR 0020 沙箱禁读接线（2026-10-13）

一次性 worktree `/tmp/home-mut`（从 `62cdcf3` 创建，用完删除），`--no-fail-fast`；变异后从副本还原，
`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M1 | 宿主不把实际会话目录加进 `deny_read` | ✅ 抓住 (rc=101) | `workspace_shell_cannot_read_the_sessions_directory` 与 `workspace_shell_cannot_read_a_moved_sessions_directory`（两条都失败） |
| M5 | `session_dir` 仍用旧的数据目录（`~/.local/share/radian/sessions`） | ✅ 抓住 (rc=101) | `radian_home_places_sessions_under_the_radian_root` |

## ADR 0021 第二片：`agent memory` 的确认（2026-10-13）

Base `5d80946` · 一次性 worktree `/tmp/memsvc-mut` · 独立 `CARGO_TARGET_DIR` ·
`cargo test -p radian-exec --test cli memory --no-fail-fast`，变异后 `git checkout --` 还原。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M3 | 非 tty 下跳过确认（`if !stdin().is_terminal()` 改 `if false`） | ✅ 抓住 (rc=101) | `memory_forget_needs_confirmation_off_a_terminal`（期望退出 2，得到 1）与 `memory_split_and_merge_move_records_between_projects`（期望拒绝，结果没拒绝） |

M3 只改一行条件；两个测试都断言「未确认时既不执行也不返回 0」，而不是只看 stderr。

一次性 worktree `/tmp/memsvc-mut2`（base `09947cb`）· 独立 `CARGO_TARGET_DIR` · `git checkout --` 还原。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M8 | `note_unlinked` 永不输出（`if global || project` 改 `if true`） | ✅ 抓住 (rc=101) | `memory_unlinked_workspace_refuses_project_writes_and_lists_global`（默认作用域的 `list` 必须报未关联 + 候选） |

## compact-overflow：交互模式识别 `/compact`（ADR 0018 §手动）

Base `dc307e6` · 一次性检出 `/tmp/covf-mut` · 独立 `CARGO_TARGET_DIR` · `git checkout --` 还原。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M9 | `compact_request` 去掉“后跟空白或行尾”的检查（`/compactfoo` 当命令） | `cli::an_interactive_compact_is_a_command_and_compactfoo_is_a_message` | ✅ failed |

## dogfooding D21/D22：闭合的 stdout 与 `--session-id`

Base `a0e8b4c` · 一次性检出 `/tmp/df7-mut`（detached，独立 target，跑完删除）· `--no-fail-fast` ·
每个变异 `git checkout --` 还原、`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M10 | `radian_host::output::write_stdout` 在写错误时 `.unwrap()`（BrokenPipe 仍 panic） | ✅ failed | `cli::a_closed_stdout_is_not_a_panic_for_every_output_command`（exit 101；TUI 的 `list::a_closed_stdout_is_not_a_panic` 走同一 helper） |
| M11 | `OutputError::BrokenPipe` 的退出码改成 1 | ✅ failed | `cli::a_closed_stdout_is_not_a_panic_for_every_output_command` |
| M12 | `--session-id` 不与 `--list`/`--search` 互斥（去掉 conflicts） | ✅ failed | `cli::list_and_search_refuse_session_id` |

## Phase 4：配置的优先级（ADR 0008）

Base `8028d9e` · 一次性 worktree `/tmp/config-mut` · 独立 `CARGO_TARGET_DIR` · `git checkout --` 还原。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M1 | CLI 不再覆盖配置（`model.model` 反而优先） | `cli::a_config_file_supplies_the_model_and_the_cli_overrides_it` | ✅ |

## config 第二轮：缺模型的判别力 + harness 不读真实配置

Base `79f60cb` · 一次性 worktree `/tmp/config2-mut` · 独立 `CARGO_TARGET_DIR` ·
`git checkout --` 还原、`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M13 | `model_name` 不再报错（`.ok_or_else(..)` → `.unwrap_or("mutant-default-model")`，即去掉 setup 的模型校验） | `cli::a_conversation_still_requires_a_model` | ✅ failed（stderr 变成 `no API key: …`，没有那句独有的 `no model: …`） |
| M14 | `spawn_agent` 不再钉 `RADIAN_HOME`（去掉 `.env("RADIAN_HOME", …)`） | `cli::the_harness_never_reads_a_config_outside_its_temp_root` | ✅ failed（`HOME/.radian/config/config.toml` 被读到，启动被 v0 配置拒绝） |

诚实补充：M13 里旧断言 `stderr.contains("--model")` 在当前 API key 文案下**也会**失败（那段文案
只含 `--api-key-env`），所以这次加强与其说是补漏，不如说是把判据钉成那句独有文案；但它必须
与“落到 API key 错误”区分开，这一点现在由整句断言保证。

## Phase 5：`agent config import-pi`

Base `5aa84b6` · 一次性 worktree `/tmp/pimig-mut` · 独立 `CARGO_TARGET_DIR` ·
每个变异 `git checkout -- .` 还原、`git status --short` 为空。宿主侧的同一批变异见
`crates/radian-host/MUTATIONS.md` 的 M15–M21。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M15 | 存在 `auth.json` 时读取它 | `cli::config_import_pi_never_reads_auth_json` | ✅ |
| M17 | 差异说明回显字面密钥值 | `cli::config_import_pi_drafts_a_valid_configuration_and_never_shows_a_literal_key` | ✅ |
| M20 | 执行 `!command` | `cli::config_import_pi_lists_a_command_key_and_does_not_run_it` | ✅ |

## ADR 0026 第 0 片（b）：描述符与 `auth.json`

Base `23149f4` · 就地变异，每个变异 `git checkout -- <file>` 还原、`git status --short` 只留本切片的新测试。
变异 2–4 改的是 `crates/radian-host`，但抓住它们的测试在 `radian-exec`，所以记在这里。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| M22 | `--api-key-env` 重新被接受（在 `Cli` 里加回该字段） | `cli::the_removed_api_key_env_flag_is_a_usage_error` | ✅ failed（clap 不再报 `unexpected argument`） |
| M23 | 配置里的 `model.api_key` 被静默忽略（`("api_key", _) => {}`） | `radian-host::config::tests::model_api_key_is_refused_with_an_auth_json_pointer`、`cli::a_config_with_model_api_key_is_refused` | ✅ failed（错误变成「没有凭据」，不再点名 `model.api_key`） |
| M24 | `opencode-go` 漏掉 `x-opencode-session` | `cli::opencode_go_sends_the_session_header_byte_for_byte` | ✅ failed（请求头为 `None`） |
| M25 | 非通用描述符接受 `--base-url`（把检查短路） | `cli::a_non_generic_descriptor_refuses_base_url_and_header` | ✅ failed（退出码 0） |

## Slice: `agent login` / `agent logout`,具名 provider(ADR 0026 D3/D10)

Base `d3b5163` · 一次性 worktree `/tmp/login-mut` · 每个变异 `git checkout -- .` 还原后跑对应
测试,`git status --short` 为空。L1(TUI 侧)见 `crates/radian-tui/MUTATIONS.md`。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| L1 | (TUI)向导明文渲染 key | key 输入不回显 | `radian-tui pty::pty_login_custom_walk_saves_without_showing_the_key` FAILED |
| L2 | CLI 的 key 提示不关 echo(termios 原样) | key 输入不回显(粘贴同) | `cli::pty_agent_login_custom_hides_the_key_and_restores_echo` FAILED("the key leaked") |
| L3 | 具名 provider 查 key 用通用描述符的槽位(D10 之前的行为) | 每个自定义端点有自己的 key 位 | `cli::named_providers_keep_separate_keys_and_config_wins_over_auth` FAILED;`radian-host provider::a_named_provider_resolves_from_config…` FAILED |
| L4 | 同名时 auth.json 压过配置 | 配置优先 + 警告 | `radian-host provider::a_named_provider_resolves…`、`anthropic_kind…`、`a_named_route…`、`auth_env_is_the_fallback…` FAILED;`cli::named_providers…`(config-wins 断言)FAILED |
| L5 | 验证请求的 401/403 报错把 key 拼进文本 | 错误文本无 key(ADR 0013) | `radian-host login::a_200_verifies_and_401_names_the_key_problem_without_the_key` FAILED |
| L6 | `/logout` 清空整个 auth.json | 只删一个条目,其他原样 | `radian-tui pty::pty_login_failed_verification_can_save_and_logout_removes_only_one` FAILED("the sibling survives") |
| L7 | CLI 任何路径都不恢复 echo(Drop 与显式调用都去掉) | Ctrl-C 与正常退出都恢复回显 | `cli::pty_agent_login_ctrl_c_during_the_key_prompt_restores_echo` FAILED(icanon 断言)、`pty_agent_login_custom…` FAILED |

7/7 抓住。**诚实补充(L7 第一轮)**:只把 Drop 里的恢复去掉是等价变异——`read_hidden` 在成功
与 Ctrl-C 路径上都**显式**调用 `guard.restore()`,Drop 只是兜底,测试照样过(假通过)。改成
"所有路径都不恢复"(即 brief 说的"CLI 不恢复回显")后才被抓住。

## ADR 0013 脱敏复审跟进：CLI 运行期错误也只装启动 key（2026-10-14）

Base `3d014a5` · 一次性 worktree `/tmp/radian-pi-priv-mut2`，跑完 `git checkout -- .`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| P3 | CLI `setup.secrets` 退回"只用启动路由的 `credential.key` + route headers" | `a_runtime_error_redacts_every_auth_key_not_only_the_route_key`（401 错误体回显另一个 provider 的 key 原样进 stderr） | ✅ |

1/1 抓住。

## ADR 0032 片 1：`radian exec` 与顶层管理命令（2026-10-15）

Base `ce5dfa2` · 一次性 worktree `/tmp/exec-mut`（从 HEAD 建，跑完删）· 每个变异先打补丁
（补丁自带锚点断言），再 `cargo build -p radian-tui --bin radian`（测试用的是**新编的**
radian，不是上一次构建的），然后 `cargo test -p radian-exec --test exec`；跑完 `git checkout -- .`，
`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| E1 | `radian exec` 不分派给库，直接返回 0（等于"分派到 TUI/什么都不做"） | `radian_exec_one_shot_is_byte_identical_to_agent`、`radian_exec_list_matches_agent_list`、`radian_exec_memory_is_the_same_as_the_top_level_memory` | ✅ |
| E2 | 顶层 `radian memory` 不走同一段代码，自己打印一行（改一个输出字） | `radian_memory_matches_agent_memory`、`radian_exec_memory_is_the_same_as_the_top_level_memory` | ✅ |
| E3 | TUI 启动被拒仍返回 1（`RunError::Startup` 的码改回 1） | `a_tui_startup_refusal_is_a_usage_error` | ✅ |

3/3 抓住。**教训**：第一次跑变异时忘了在新 worktree 里先编 `radian`，测试里的
`radian_bin()` 断言"文件不存在"而失败——那不是"被抓住"，是环境没准备好；变异脚本必须
先构建被测二进制。

## ADR 0032 片 2：测试迁到 `radian exec`、删 `agent` 二进制、全仓改名（2026-10-15）

Base `bf6ccf8` · 一次性 worktree `/tmp/e2-mut`（从 HEAD 建，跑完删）· 每个变异先打补丁
（补丁自带锚点断言），再 `cargo build -p radian-tui --bin radian` 与
`cargo build -p agent-extensions --bin mcp-fixture`（测试用的是新编的二进制；陈旧检测会
拒绝旧二进制），然后按**单个**测试过滤跑（cargo 只接受一个位置参数）；跑完
`git checkout -- .`，`git status --short` 为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| X1 | 库的诊断前缀改回 `agent: ` | `diagnostics_carry_the_radian_prefix`（cli.rs） | ✅ |
| X2 | TUI 的缺凭据提示改回 `run agent login …` | `the_missing_credential_hint_names_radian_login`（exec.rs） | ✅ |

2/2 抓住。**教训**：变异脚本必须在**新 worktree 里**先编 `radian`（陈旧检测会直接拒绝旧
二进制，这是它该做的）；另外 MCP 相关测试需要先编 `mcp-fixture`，否则它们会以环境错误
失败、把"被抓住"的假象混进来——所以最后是按目标测试逐个过滤确认的。

## ADR 0043：`radian exec` 挂 lead tools（2026-10-16）

Base `2f41f69` · 直接改源码、跑单个测试、从 `.radian/scratch/mut/` 副本 `cp` 还原，
`git status --short` 跑完为空。测试前先 `cargo build -p radian-tui`（陈旧检测会拒绝旧二进制）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| S6B2-1 | `lib.rs` 的 `task_builder: Some(task_builder)` 改回 `None`（headless lead 不挂 lead tools） | `a_headless_lead_mounts_the_task_tools`（tools 列表里没有 `spawn_task` 等） | ✅ |

1/1 抓住。

## ADR 0016：`radian-exec` 的 `build_sandbox` 继承外层沙箱（2026-10-16）

worktree `.radian-worktrees/inherited-sandbox`，分支 `dogfood/inherited-sandbox`。变异只改
`src/profile.rs`，跑 `cargo test -p radian-exec --locked --no-fail-fast --test cli`，跑完从副本还原并
`cmp` 确认逐字节一致。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| IS-11 | `workspace` 档位无条件选 `inherited`（不看内核；`RADIAN_SANDBOX_PROBE_FAIL=1` 也就不再拒绝） | `a_failed_sandbox_probe_refuses_instead_of_degrading` | ✅ FAILED |

1/1 抓住。与 `radian-tui` 的同名变异（其 MUTATIONS.md 的 IS-4/IS-5）是同一形态：两处
`build_sandbox` 都必须先问内核，且探测失败仍是拒绝。
## oauth-redact：启动后才登记的 token 也要脱敏（2026-10-16）

Base `5cfb0b1` · worktree `.radian-worktrees/oauth-redact`。两条名单（`Setup::secrets` 用于所有
报错、`export_secrets` 用于导出/搜索）都只在启动时读一次 `resolved_keys()`；`chatgpt` 的 access
token 在会话里被 `ensure_fresh` 刷新并写回 `auth.json` 之后，回显它的 401 或导出就会原文带出
token（ADR 0026 禁止）。两个读点都改成走 `redaction_secrets()`（每次脱敏时现读
`radian_host::registered_secrets()`）。变异直接改 `src/lib.rs`，改前 `cp` 到
`.radian/scratch/mut/`，改后 `cp` 还原并 `cmp` 一致（`git status --short` 跑完只剩被测文件）。
命令：`timeout 300 cargo test -p radian-exec --locked --no-fail-fast --lib the_error_and_export`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| OR-C1 | `redact` 直接遍历入参（不再经 `redaction_secrets`） | `tests::the_error_and_export_lists_carry_a_value_registered_after_startup`（输出里留着 `late-cli-token-…`） | ✅ 1 failed |
| OR-C2 | `export_secrets` 末尾返回原 `secrets`（去掉 `redaction_secrets`） | 同上（`the export list must carry the registered token: []`） | ✅ 1 failed |

2/2 抓住。

## ADR 0043 slice ①：缺省 `full`、显式 flag 优先、allow_env（2026-10-17）

命令：`timeout 300 cargo test -p radian-exec --locked --no-fail-fast --test cli <name>`；每个变异
先 `cargo build -p radian-tui --bin radian`，`cp` 备份 / 还原，`cmp` 逐字节一致。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| D43-1 | `profile::default_profile()` 回到 `WorkspaceWrite` | `full_is_the_default_and_no_write_tools_without_a_workspace`（`[profile] full` 不成立） | ✅ 1 failed |
| D43-2 | 项目的 `full` 覆盖显式 `--profile`（`if cli.profile.is_some()` 分支失效） | `an_explicit_flag_wins_and_the_line_names_the_project_ask`（`[profile] full` 不成立） | ✅ 1 failed |
| D43-3 | `build_sandbox` 不把 `allow_env` 加进 policy（删掉循环） | `the_shell_environment_carries_herdr_and_allow_env`（`left: "pane-9||"`） | ✅ 1 failed |

3/3 抓住。

## ADR 0043 fix round 1 B：子进程必须处理 SIGTERM（2026-10-17）

`radian exec` 现在为 SIGTERM 装一个长驻监听（与 SIGINT 同一个 channel），收到后取消当前轮、
立刻关闭 inbox 并 join driver（会话结束会杀掉每个后台 job 的整个 session），退出码 128+15。
否则 SIGTERM 走内核默认动作，进程当场被杀，`Jobs::kill_all` 与 shell 工具的取消清理都不会跑。

命令：`timeout 400 cargo test -p radian-exec --locked --no-fail-fast --test subagent a_cancelled`；
变异前 `cp` 备份、`cargo build -p radian-tui --bin radian`，变异后 `cp` 还原并 `cmp` 逐字节一致。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SG-TERM | `spawn_signals` 不再监听 SIGTERM（空任务） | `a_cancelled_parent_turn_stops_the_command_the_childs_shell_started`、`a_cancelled_parent_turn_stops_the_childs_background_job`（两条都等不到 pid 消失） | ✅ 2 failed |

## `radian exec --max-model-calls` defaults to 1000（与 TUI 共享常量）

`radian-exec` 的 `--max-model-calls` 从 `Option<u32>`（缺省走 core 的 32）改成 `u32` +
`default_value_t = radian_host::DEFAULT_MAX_MODEL_CALLS`（1000，与 TUI 同一个常量）。轮结束时
`RunFinished` 的 sink 打印一行命名上限与 flag（`radian_host::budget_exceeded_line`），`one_shot`
不再重复打印通用状态行。变异在源码原地改，每次从 `/tmp/mut/` 的 `cp` 副本还原并
`cmp` 逐字节一致；运行都带 `--no-fail-fast`。

命令：`timeout 300 cargo test -p radian-exec --locked --no-fail-fast --lib the_default_model_call_limit`；
预算行那条要新 binary：先 `cp` 备份、`cargo build -p radian-tui --bin radian`，再
`timeout 300 cargo test -p radian-exec --locked --no-fail-fast --test exec a_budget_exceeded`，
之后 `cp` 还原并 `cmp` 一致。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| MMC-1 | flag 的默认值回到 `radian_core::DEFAULT_MAX_MODEL_CALLS`（32） | `lib::tests::the_default_model_call_limit_is_the_shared_host_value`（`left: 32, right: 1000`） | ✅ 1 failed |
| MMC-2 | `ui.rs` 的 `RunStatus::BudgetExceeded` 分支删掉（回到 `[turn] ended as …`） | `exec::a_budget_exceeded_turn_names_the_limit_and_the_flag`（stderr 不含那行） | ✅ 1 failed |

## rework 5: `--image` is not read whole before a check (2026-10-23, branch `work/TK0013-image-inputs`)

`load_images` now calls the shared `radian_coding::image::load_path`, which reads a bounded
64 KiB head, checks the metadata size against 5 MiB, and reads the body only for an accepted
image. The mutation puts the old whole-file `std::fs::read` back; the test-only path read
counter (the `testing` feature) makes the miss visible on a 2 GiB `set_len` sparse file.
Restored from `.radian/setup.rs.bak` byte-identically.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| RW5-1 | `load_images` reads the whole file (`std::fs::read`) before the magic/size checks | ✅ 2 failed | `setup::tests::a_large_non_image_is_refused_without_reading_it_whole` and `setup::tests::an_over_limit_image_is_refused_without_reading_its_body` (0 bytes through the bounded reader) |

## TK0161: the pty login tests wait for the whole `stty -a` output (2026-10-25, branch `work/TK0161-pty-stty-read-race`)

`PtyChild`'s shell now prints `STTY-END` after `stty -a`, and `wait_for_stty` waits for that
marker before `assert_stty_has_echo` runs. The mutation was run in place with the reproduction
in place: the pty shell ran
`stty -a | while IFS= read -r line; do printf '%s\n' "$line"; sleep 0.05; done`
so the lines arrive in separate writes, as they do when the writer and the drain are not
scheduled together. Command: `cargo nextest run -p radian-exec --locked --test cli -E
'test(pty_agent_login)'`. Restored from `.radian/cli.rs.final`, `cmp` identical,
`git status --short` empty afterwards.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0161-M1 | put the old wait back: `wait_for(.., "speed ", ..)` and then assert on `seen.split("EXITED:0").nth(1)` / `"EXITED:130"` without waiting for the marker | ✅ 2 failed | `pty_agent_login_custom_hides_the_key_and_restores_echo` and `pty_agent_login_ctrl_c_during_the_key_prompt_restores_echo`: `canonical mode must be restored: ` in `assert_stty_has_echo` — the buffer held only the `speed …` line |

Measured with that reproduction: the old wait failed 20/20 runs of each test, `wait_for_stty`
passed 20/20 of each. With the real `stty -a` and ten `yes` CPU hogs on this macOS box neither
form failed (old 0/100, new 0/200 per test): the natural race needs the Linux line writer plus
the full workspace load.

## `Result.model_error` (TK0184, 2026-10-26)

Branch `work/TK0184-exec-result-model-error`. Each mutation was applied in place, only
`cargo nextest run -p radian-exec --locked --profile sandbox --lib` was run, and the source was
restored from a `cp` under `.radian/mut-backup`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0184-M2 | the `Result` writer drops `model_error` (the insertion block removed from `result_line`) | ✅ 1 failed | `jsonl::tests::the_result_line_carries_model_error_only_on_a_failed_model_error` (the failed line has no `model_error`) |
| TK0184-M3 | `retry_after_secs` read from `Unavailable.retry_after` only (the `UsageLimit.resets_in` arm removed) | ✅ 2 failed | `jsonl::tests::the_model_error_field_carries_kind_status_and_wait` (`usage_limit` reports `null` instead of `6240`) and `jsonl::tests::the_result_line_carries_model_error_only_on_a_failed_model_error` |

## Rework 2: the runtime callback carries `UsageLimit` to the `Result` (TK0184, 2026-10-27)

Branch `work/TK0184-exec-result-model-error`. The mutation was applied in place, only the new
test was run (`cargo nextest run -p radian-exec --locked --profile sandbox --lib -E
'test(a_usage_limit_error_reaches_the_result_through_the_driver)'`), and the source was restored
from `.radian/mut-backup/chat.rs` (`cmp` identical). The restored lib then passed 32/32.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0184-M4 | `ExecRuntimeEvents::on_model_error` stores nothing (the callback body is a no-op) | ✅ 1 failed | `jsonl::tests::a_usage_limit_error_reaches_the_result_through_the_driver` (the stored slot stays `None`, so the `Result` has no `model_error`) |

## TK0191: the startup route resolves its catalog cap once (2026-10-27)

Branch `work/TK0191-chat-max-tokens-and-opencode-go-usage`. The mutation was applied in place, the
`radian` binary rebuilt (`cargo build -p radian-tui --bin radian`, so the test does not refuse a
stale binary), and the source restored from `.radian/mut-backup/setup.rs` (`cmp` identical). The
catching test is in the sandbox profile's `/tmp` exclusion group (`scratch` hard-codes
`/tmp`, which the workspace tier denies), so it was run from outside that sandbox:
`cargo nextest run -p radian-exec --test cli --locked --profile default -E
'test(=the_startup_line_names_the_catalog)'`. Unmutated, the same command passed (1/1).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0191-M1 | the startup route's `request_output` is `RouteOutput::None` (the catalog cap is never resolved), so the sent `max_tokens`, `R` and `B` all fall back to the old none / 16384 / 922000 | ✅ 1 failed (rc=100) | `cli::the_startup_line_names_the_catalog` (cli.rs:6746, the `B 750000` assertion; the line printed `[budget] C 1000000, I 922000, output 384000, R 16384, B 922000`, and the mock request carried no `max_tokens`) |

## `radian acp`: one session and a cancelled turn (TK0203, 2026-10-27)

Branch `work/TK0203-radian-acp-server`. Each mutation was applied in place, the `radian`
binary was rebuilt (`cargo build -p radian-tui --bin radian`, so the test does not refuse a
stale binary), and the source was restored from `.radian/mut-backup/acp-mod.rs` (`cmp`
identical). The catching tests run in the sandbox profile:
`cargo nextest run -p radian-exec --locked --profile sandbox --test acp -E 'test(<name>)'`.
Unmutated, each test passed.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0203-M1 | `stop_reason` maps `RunStatus::Cancelled` to `end_turn` instead of `cancelled` | ✅ 1 failed | `acp::a_cancelled_turn_answers_cancelled_and_keeps_the_interrupted_prefix` (the response says `stopReason: end_turn`) |
| TK0203-M2 | the existing-session branch of `session/new` returns the current session id instead of the one-session error | ✅ 1 failed | `acp::one_process_holds_one_session` (the second `session/new` succeeds; its error message is empty) |

## `radian acp`: `session/load` continues the journal (TK0204, 2026-10-27)

Branch `work/TK0204-radian-acp-session-load`. The mutation was applied in place, the `radian`
binary was rebuilt (`cargo build -p radian-tui --bin radian`), and the source was restored from
`.radian/mut-backup/acp-mod.rs` (`cmp` identical). The catching test ran in the sandbox profile:
`cargo nextest run -p radian-exec --locked --profile sandbox --test acp -E
'test(session_load_replays_the_journal_and_continues_it)'`. Unmutated, the whole `acp` binary
passed 12/12.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0204-M1 | `session/load` does not set `--resume`, so the opener generates a new id and starts a new journal instead of continuing the loaded one | ✅ 1 failed | `acp::session_load_replays_the_journal_and_continues_it` ("the transcript is replayed": the loaded session's recovered history is empty, so no `session/update` is sent) |

## `radian acp`: a failed `session/new` is retryable (TK0207, 2026-10-27)

Branch `work/TK0207-arc-acp-worker-followups`. The child headers are consumed only once the
session is open, so a `setup` failure (a refused config) leaves them for a retried
`session/new`. The mutation was applied in place to `crates/radian-exec/src/acp/mod.rs`, the
`radian` binary was rebuilt (`cargo build -p radian-tui --bin radian`), and the source was
restored from `.radian/mut/acp-mod.rs` (`cmp` identical). The catching test ran in the sandbox
profile: `cargo nextest run -p radian-exec --locked --profile sandbox --test acp -E
'test(a_failed_session_new_can_be_retried)'`. Unmutated, the whole `acp` binary passed 13/13.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0207-M2 | `session/new` takes the child headers before `setup` succeeds (`child_headers.take()` instead of `as_ref()` plus a clone) | ✅ 1 failed | `acp::a_failed_session_new_can_be_retried` (the retry answers `internal`: "the internal child headers were already consumed") |

## `radian acp`: the prompt answer waits for the journaled turn end (TK0209, 2026-10-27)

Branch `work/TK0209-acp-prompt-waits-for-durable-turn-end`. `prompt_turn` now waits for the
driver's `RuntimeEvent::TurnFinished` (the journaled turn end) after the live `RunFinished`,
instead of answering on the live event. The catching test sets the existing
`RADIAN_TURN_COMMIT_DELAY_MS` hook to 2000 ms, so the gap is wide and deterministic. The
mutation was applied in place to `crates/radian-exec/src/acp/mod.rs`, the `radian` binary was
rebuilt (`cargo build -p radian-tui --bin radian`), and the source was restored from
`.radian/mut/acp-mod.rs` (`cmp` identical). Test:
`cargo nextest run -p radian-exec --locked --profile sandbox --test acp -E
'test(the_prompt_answer_waits_for_the_journaled_turn_end)'`. Unmutated, the whole `acp` binary
passed 14/14.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0209-M1 | the prompt answer waits only for the live `RunFinished` again (`if live_done && journaled_done` → `if live_done`) | ✅ 1 failed | `acp::the_prompt_answer_waits_for_the_journaled_turn_end` ("the answer arrived before the turn was durable": the journal held `MessageAppended` but no `TurnFinished`) |


## `radian acp`: a prompt is answered by its own turn (TK0222, 2026-10-28)

Branch `work/TK0222-acp-prompt-waits-for-its-own-turn`. `prompt_turn` now waits for the
journaled end of the turn that claimed **its** `InputClaimed` message (the `TurnLedger`) and
reads that turn's live `RunFinished` status, instead of the next turn end and whatever status
came last. The test drives a real background job whose settle opens a wake turn between the
prompt's enqueue and its claim; the existing `RADIAN_CLAIM_HOLD_FILE` test seam makes that order
deterministic (the driver claims nothing while the file exists). The mutation was applied in
place to `crates/radian-exec/src/acp/{mod,events}.rs`, the `radian` binary was rebuilt
(`cargo build -p radian-tui --bin radian`), and the source was restored from `.radian/mut/`
(`cmp` identical). Test:
`cargo nextest run -p radian-exec --locked --profile sandbox --test acp -E
'test(a_wake_turn_end_does_not_answer_the_prompt_after_it)'`. Unmutated, the whole `acp` binary
passed 18/18.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0222-M1 | the prompt wait ignores which turn claimed the message and stops at the next journaled turn end (the old generation-counter shape), and takes the newest turn's live status | ✅ 1 failed | `acp::a_wake_turn_end_does_not_answer_the_prompt_after_it` (the wake turn's failed end answered: `stopReason: refusal`, `_meta.radian.text: PROMPT-ONE`) |

## `radian acp`: a prompt's `_meta` is its own turn's (TK0222 rework, 2026-10-28)

Branch `work/TK0222-acp-prompt-waits-for-its-own-turn`. The answer's cost and text are folded
from the claimed turn's own journal records (`jsonl::summarize_turn`, selected by turn id), and
`_meta.radian.error` is kept per turn id in the `TurnLedger`, so a turn that starts and finishes
before the answer task reads the journal cannot stand in for the prompt's turn. The mutations
were applied in place to `crates/radian-exec/src/acp/{mod,events}.rs`, the `radian` binary was
rebuilt (`cargo build -p radian-tui --bin radian`), and the source was restored from
`.radian/mut/` (`cmp` identical).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0222-M3 | `prompt_turn` folds the whole session again (`summarize_journal`) for the answer's cost and text | ✅ 1 failed | `acp::a_prompt_reports_its_own_turns_cost_and_text_not_the_sessions` (`cost.micros` was 2700 — the first turn's and the wake turn's fees — instead of the prompt turn's 1350) |
| TK0222-M5 | the ledger keeps one session-global error slot instead of keying it by turn (`turn_failed` stores under a fixed key, `error` ignores the turn) | ✅ 1 failed | `acp::events::tests::a_later_turn_failure_does_not_replace_an_earlier_turns_error` (`error("t-1")` read the wake turn's message) |

2/2 caught. The cost test's wake turn **succeeds** and reports usage, so the session-wide fold
differs from the prompt's own turn; the earlier wake test's wake turn fails and reports no
usage, so it could not tell the two folds apart.

## `radian acp`: `session/load` refuses another workspace (TK0223, 2026-10-28)

Branch `work/TK0223-acp-load-refuses-other-workspace`. `open_session_request` reads the
journal's `SessionCreated.workspace` through `radian_host::sessions::other_workspace` (the
comparison `--export` uses) before `setup`, and refuses a session of another workspace with
JSON-RPC `INVALID_PARAMS`. Each mutation was applied in place, the `radian` binary was rebuilt
(`cargo build -p radian-tui --bin radian`), and the source was restored from `.radian/mut/`
(`cmp` identical). The tests ran in the sandbox profile:
`cargo nextest run -p radian-exec --locked --profile sandbox --test acp -E 'test(<name>)'`.
Unmutated, the six new tests pass 6/6.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0223-M1 | the workspace check never refuses (`other_workspace` always returns `None`) | ✅ 4 failed | `acp::session_load_refuses_a_session_of_another_workspace`, `acp::session_load_refuses_another_workspace_when_the_cwd_sets_it`, `acp::a_refused_load_leaves_the_process_able_to_load_a_valid_session`, `acp::a_refused_load_leaves_the_process_able_to_start_a_new_session` (the cross-workspace load succeeded) |
| TK0223-M2 | the check compares the request's raw `cwd` instead of the canonical process workspace | ✅ 1 failed | `acp::session_load_accepts_the_workspace_reached_through_a_symlink` (the symlinked path no longer matches the recorded canonical workspace) |
| TK0223-M3 | the refusal consumes the internal child headers before returning (`*child_headers = None`) | ✅ 2 failed | `acp::a_refused_load_leaves_the_process_able_to_load_a_valid_session` and `acp::a_refused_load_leaves_the_process_able_to_start_a_new_session` (the retry answers `internal`: "the internal child headers were already consumed") |
| TK0223-M4 | a journal with no readable `SessionCreated` is treated as another workspace instead of being left to the opener | ✅ 1 failed | `acp::session_load_of_a_truncated_journal_is_the_openers_error` (the reply is `INVALID_PARAMS` with "not in this workspace" instead of the opener's `INTERNAL_ERROR`) |

4/4 caught. M2 changes only the symlink case: the test harness's `$TMPDIR` path is already
canonical on this machine, so the other tests see the same string either way.

## Resume applies the same workspace rule (TK0223 rework, 2026-10-28)

Branch `work/TK0223-acp-load-refuses-other-workspace`. `radian_host::open_session` now reads
the resumed journal's `SessionCreated.workspace` after the locked parse and before
`into_journal` can write the interrupted-turn `Aborted` line, and refuses a session of another
workspace through `radian_host::sessions::workspace_mismatch` — the equality and the wording
`--export` and `radian acp`'s `session/load` use. `radian --resume`, `radian exec --resume`
and `session/new` with `--resume` therefore agree with `session/load`. The two
`session/load` tests the review found passing on the old code now assert the refusal first.

Each mutation was applied in place, the `radian` binary was rebuilt
(`cargo build -p radian-tui --bin radian`), and the source was restored from `.radian/mut/`
(`cmp` identical). The tests ran in the sandbox profile:

- `cargo nextest run -p radian-exec --locked --profile sandbox --test cli -E 'test(<name>)'`
- `cargo nextest run -p radian-exec --locked --profile sandbox --test acp -E 'test(<name>)'`
- `cargo nextest run -p radian-tui --locked --profile sandbox --test resume`
- `cargo nextest run -p radian-tui --locked --profile sandbox --test pty -E 'test(<name>)'`

Unmutated, every test named here passes.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0223-M5 | `open_session` skips the resume workspace check | ✅ 2 failed | `resume_refuses_another_workspaces_session` (`radian-exec::cli` and `radian-tui::resume`: the cross-workspace resume succeeds and reaches the model) |
| TK0223-M6 | `workspace_mismatch` refuses every recorded workspace (`recorded != workspace` → `true`) | ✅ 1 failed | `pty_resume_renders_the_stored_history` (a session of the process's own workspace is refused) |
| TK0223-M1 (re-run on the rewritten tests, together with M5: the pre-change load path) | the ACP pre-check is removed, so `session/load` falls through to the unguarded opener | ✅ 2 failed | `session_load_accepts_the_workspace_reached_through_a_symlink` (the other workspace's session was loaded and replayed) and `session_load_of_a_truncated_journal_is_the_openers_error` (the first, different-workspace load was accepted) |
| TK0223-M2 (re-run on the rewritten test) | the ACP check compares the request's raw `cwd` instead of the canonical process workspace | ✅ 1 failed | `session_load_accepts_the_workspace_reached_through_a_symlink` (the linked path no longer matches the recorded canonical workspace) |
| TK0223-M4 (re-run on the rewritten test) | a journal with no readable `SessionCreated` is treated as another workspace | ✅ 1 failed | `session_load_of_a_truncated_journal_is_the_openers_error` (the reply is `INVALID_PARAMS` instead of the opener's `INTERNAL_ERROR`) |

5/5 caught. M5 and M1 together are the pre-change behavior for `session/load`: the rewritten
symlink and truncated tests fail on them, which is what the review asked for.

## `radian acp`: bounded stdio (TK0224, 2026-10-28)

Branch `work/TK0224-acp-stdio-bounded`. Each mutation was applied in place to
`crates/radian-exec/src/acp/{wire,mod}.rs`, and the source was restored from `.radian/mut/`
(`cmp` identical). The unit-test rows ran
`cargo nextest run -p radian-exec --locked --profile sandbox --lib -E 'test(<name>)'`; the
integration rows rebuilt the `radian` binary (`cargo build -p radian-tui --bin radian`) and ran
`cargo nextest run -p radian-exec --locked --profile sandbox --test acp -E 'test(<name>)'`.
Unmutated, the `acp` binary passed 30/30 and the `radian-exec` lib passed 41 of its 42 tests
(one is skipped under the sandbox profile).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0224-M1 | the reader appends past the limit (`take <= self.limit` and the append cap removed) | ✅ 2 failed | `acp::wire::tests::a_line_longer_than_the_limit_is_discarded_without_being_held` (the over-long line came back as a line, and the accumulated buffer passed the limit) and `acp::wire::tests::eof_in_the_middle_of_an_over_long_line_is_eof` |
| TK0224-M2 | both producers drop when the queue is full (`try_send` instead of waiting) | ✅ 1 failed | `acp::wire::tests::a_full_outbox_makes_both_producers_wait_and_drops_nothing` (a producer resolved at once instead of waiting) |
| TK0224-M3 | an over-long line is answered as invalid params (`-32602`) instead of a parse error (`-32700`) | ✅ 1 failed | `acp::an_over_long_line_is_a_parse_error_and_the_next_line_is_served` |
| TK0224-M4 | an over-long line with no newline before EOF is answered as over the limit instead of end of input | ✅ 2 failed | `acp::wire::tests::eof_in_the_middle_of_an_over_long_line_is_eof` and `acp::eof_in_the_middle_of_an_over_long_line_is_a_clean_shutdown` |
| TK0224-M5 | the exact-limit boundary is `<` instead of `<=` | ✅ 1 failed | `acp::wire::tests::a_line_exactly_at_the_limit_is_accepted` |

5/5 caught. M2's mutation is the "bounded but lossy" alternative: the queue stays at its
capacity, but the update is dropped, which is what the test's waiting and delivery assertions
refuse.

