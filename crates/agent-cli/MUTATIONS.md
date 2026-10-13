# `agent-cli` sessions-on-disk mutation record (Phase 2 / B)

Base: `3451dc1` · one-shot worktree `/tmp/cli-mut` (deleted after) · each mutation run with
`cargo test -q -p agent-cli --test cli --no-fail-fast`; `git checkout --` after each;
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

`second_sigint_exits_130` was then run **30 times** with `--no-fail-fast`; all 30 passed
(0 failures). Outputs: `/tmp/a66-30runs/run-{1..30}.txt`.

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
`cargo test -p agent-cli --no-fail-fast a_failed_probe_leaves_no_session`，用 `git checkout --`
恢复、`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M9 | `chat` 在检查之前创建会话文件（把 `open_journal` 提到 `profile`/`sandbox`/`coding` 之前） | ✅ failed | `a_failed_probe_leaves_no_session_and_the_retry_creates_one`（`left files behind: ["retry.jsonl"]`） |

「先建会话再检查」的 CLI 实例。工作区错误在 `setup` 里已经验证，所以用探测失败（sandbox 在
`chat`）更能直接观察「检查失败仍留下文件」。

## --export / --search（2026-10-13）

一次性检出 `/tmp/clisess-mut`（`git worktree add --detach HEAD`，HEAD = `4d973d4`），每个变异用
`git checkout --` 恢复、`git status --short` 为空；`cargo test -p agent-cli --no-fail-fast
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
`git status --short` 为空；`cargo test -p agent-cli --no-fail-fast <精确测试名>`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M14 | 交互模式不带 `--model` 也能启动（去掉 clap 的 `required_unless_present_any` 并让 `model_name` 默认成 `"m"`） | ✅ failed | `a_conversation_still_requires_a_model`（不再有 clap 的 `Usage:` 行；实际是后面的 “no API key” 退出 2） |
| M15 | `--search` 的缓存写成 `.jsonl` 名（`index.db` → `search.jsonl`），即看起来像新建了一个会话文件 | ✅ failed | `session_commands_do_not_touch_the_session_directory`（`created or modified a .jsonl file`，`index.db` 也不再出现） |

M14 的断言特意包含 clap 的 `Usage:` 行：只断言 `--model` 字样的话，后面的 setup 错误也含它，变异就抓不住
了（第一次写测试时正是这个洞）。M15 覆盖 brief 要求的「search 写进了一个新的 .jsonl」。

## 第三轮：--export 的 session id 校验（2026-10-13）

一次性检出 `/tmp/clisess3-mut`（base `421221b`），变异后
`cargo test -p agent-cli --no-fail-fast export_refuses_a_path_like_session_id`，用 `git checkout --`
恢复、`git status --short` 为空。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M16 | `--export` 不调用 `validate_session_id`（直接拿 id 拼文件名） | ✅ failed | `export_refuses_a_path_like_session_id`（`../x` 读到上级伪造文件，退出 0） |

测试里的伪造文件 `dir/x.jsonl` 的 `SessionCreated.workspace` 故意写成当前工作区，所以只靠
工作区比对是挡不住的；M16 证明去掉 id 校验后它会被导出，而校验在场时被拒。
