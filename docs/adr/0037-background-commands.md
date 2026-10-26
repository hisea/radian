# ADR 0037: Background Commands and Completion Notices

Status: accepted

## Decisions

### 1. Explicit background

`shell` owns `background?: boolean`. `background: true` moves the command to background at once, **with no deadline**. `timeout_secs` is ignored. The ack row states "timeout_secs is ignored for background jobs; bound it in the command (`timeout N …`) or stop it with `job kill`". Deadline needs live inside the command (`timeout N …`) or use `job { op: "kill" }`. Foreground `timeout_secs` stays as-is.

### 2. Only the explicit path leads to background

A foreground `shell` call never moves itself to background: background runs only when the model explicitly writes `background: true`. Foreground default timeout is **300 s**, cap **600 s**, unchanged. A long command with no `timeout_secs` (such as `cargo test`) waits in foreground to the default timeout. It ends as timeout at the mark. The result names both rerun paths: larger `timeout_secs` (cap 600) or `background: true`. The `[shell] auto_background_secs` config key is deleted. Old-config occurrences warn one row and ignore. They never error (ADR 0008 skip rules). No threshold parameter is exposed to the model: background wants mean `background: true`.
- `!`/`!!` (ADR 0033) run in foreground only.

### 3. Output

Output files live in the session own directory (`<sessions_dir>/<session-id>.jobs/<id>.out`, dir 0700, file 0600). They sit on the ADR 0020 sandbox-deny list. The model never sees or writes them. Opens use `O_CREAT|O_EXCL|O_NOFOLLOW`. The model sees only job id and the `job` hint (`available through job`). It never sees real paths. It reads only through `job`. The workspace `.radian/jobs/` receives no more writes.

### 3a. Truncated foreground output enters the same registry

A **foreground** `shell` command with truncated results keeps full output in the same session-private directory. It registers in the **same job registry** as one **already settled entry**. Id is a plain job id. State is the command exit state (`exited n` / `signal n` / `timed out`). Output file is the kept full text. The model receives a job id, never a path. `job { op: "read" }` reads it exactly like a background job. No second id shape, no second reader exists.

- It takes no background slot (already settled). It writes no `BackgroundJobStarted`/`BackgroundJobSettled`. Those are background-job durable records. Foreground keeps are live state, dropped with the session.
- Keeps happen only when the caller provides a **session-private directory** (`<sessions_dir>/<session-id>.jobs/`, same root as job output). With no such directory, truncated foreground results register no entry, and the result states dropped byte counts.
- Keeps are bounded: one entry owns a byte cap, the whole session owns total and entry-count caps. Overflow evicts oldest foreground entries first, and their ids later read as unknown. Single entries can truncate at caps too, and results state the truncation.
- Untruncated output registers no entry and keeps no file.
- Normal session close deletes all kept files. The panic path adds no file I/O, and state drops with the session.

### 4. One `job` tool

The model owns **one** background tool: `job { id, op }`, `op` takes three values.

- `read`: reads output of `id`. No range parameter returns the delta since the last read (bounded ring) plus one `[status: …]` row (running / exited n / signal n / killed / timed out). One of `tail_lines` (last N rows), `from_line`/`max_lines` (page by `read` row numbers), or `pattern` (matching rows only, Rust regex) reads the whole file by rows, and the result states total file rows and whether more remain. Background jobs and 3a foreground entries share the same path.
- `wait`: blocks to `id` end or `timeout_ms` (default 30 s, cap 600 s), returns delta and state. Timeout returns `[status: running]` and **never kills the job**. Completion notices (decision 5) also wake the model on arrival. `wait` and notices are two coexisting wait paths.
- `kill`: sends SIGTERM first, SIGKILL after a 3 s grace. The result settles as killed, with no completion notice. Ended jobs are a no-op with a note.
- An explicit-background job that ended by itself (exited or signalled; not stopped) and whose run fits the foreground default timeout (300 s) carries one hint row on the first `read`/`wait` that sees it ended, just before `[status: …]`: `[this job ended after N s; a foreground shell call (default 300 s, max 600 s) returns such a command in one step]`. A job that ran past the default never gets it (background was the right call), a 3a retained entry never gets it, and a later `read` never repeats it. The run time is captured at settle, not read from the clock at read time; the threshold is the foreground default, with no new knob. Completion notices and the `shell`/`job` descriptions are unchanged.

No `list` exists: job ids already live in model context. The human-facing list is the TUI inspector jobs page. `/jobs` opens it. `/tools`, `/mcp`, `/hooks` open their own pages. `/panel` returns to the last page. Left/right keys or Tab switch pages, up/down picks rows, Enter shows detail, Esc closes. Text form stays: `/jobs kill <id>`, `/jobs kill all`, and parameterized queries still print as notice output, scripts and herdr unaffected. The jobs page is read-only, its only write is `k` (one confirm, same path as `/jobs kill`). Ended background jobs and foreground kept entries read through `job` (decision 3a). `/jobs kill` on ended entries is a no-op.

`shell` and `job` descriptions state each rule once, schema parameter notes never repeat descriptions. Explicit-background results carry one line "it notifies you on completion; never sleep or poll; with nothing else to do, end the answer". Both tools pin a byte cap by test on compact-JSON `{"name","description","input_schema"}` UTF-8 bytes. `shell` ≤ 2,730 B. `job` ≤ 1,660 B.

Removed names `job_output`, `job_list`, `job_kill`, `shell_output`, and similar stay deleted (ADR 0043 decision 3). Calls receive unknown-tool errors. No acceptance, no new-name hints. Tools only rename or delete directly, never set transition aliases.

### 5. Completion notices

- On completion the shape is fixed. **Busy takes `inject_pending_wake`** (`next_step`, `pending_wake` mark). Mid-turn absorption means context. Turn-end stranding opens its own next turn (ADR 0011 L5). **Idle takes `followup`** (`next_turn`, wakes one turn). Source is **System**. Shape reads `background job <id> finished [status: …]. Read its output with job.`
- Self-`wait`ed completions, `job kill`ed jobs, and session-exit settles send no notice.
- Busy-notice wakes count only at turn open. A stranded notice opening the next turn counts **one** wake. Host counts in the inbox pending-wake callback. It never pre-deducts by enqueue count. Absorbed ones never count. A full gate degrades busy notices to `inject_degraded` too (no `pending_wake` mark), same shape as idle-side degrade.
- Wakes pass one sliding-window rate gate: **max 10 wakes per 10 minutes**, overflow degrades to inject-only. The window passing restores automatically, no user message needed. A woken turn absorbs waiting injections, degrade counts clear with it, user-message arrival clears immediately too. Normal worker chains (minute-level wake gaps) never touch the gate. Self-kicking chains ("a woken turn starts one more background command", second-level wake gaps) top out in seconds. `Stop` hook continuations follow the same rate rule with separate counts (ADR 0045 §3).
- Enqueue failure means `followup` / `inject_pending_wake` / `inject_degraded` any returning `Err` (commonly full disk failing journal writes). Retries stay bounded: 3 tries total, gaps about 0.2 s, 1 s. No infinite retries, no one settle blocking other settles long. Failed journal appends never poison the journal (`next_seq` advances only on success), post-recovery space lets retries succeed. Full queues and other transient causes share the retry cover.
- 3 failed tries mark that settle notice lost. It records one lost. Later successful wakes never absorb it. Only user messages clear it. It writes one failure-cause row to the diagnostics directory (best effort; a full disk can fail that write too). It shows one visible stop with the same mechanism as wake caps. TUI shows one notice row (names job id, failure cause, and "send any message to continue"). herdr reports `blocked` with the `job notice lost` label, no command text or output content. The next turn start clears it. `radian exec -p` meeting lost exits 3 and prints one stderr row, same as degraded wakes.
- Same-lane retries only: `followup` failure never falls back to `inject`. Each enqueue writes the journal first (ADR 0001 S1), and a full disk fails the same way.
- Over-large results carry preview plus job id only, and the model takes needed parts through `job read`. Notices count as normal input toward the next request budget.

### 6. Journal and recovery

- Start and end each own one durable record. Records are `BackgroundJobStarted { job_id, command, output }` with `BackgroundJobSettled { job_id, state, duration_ms }`. Both own `critical: false`. No new record types added. 3a foreground entries never write these two records.
- Progress (produced byte counts) is live, never written.
- Restart finds start-without-settle jobs, reports "unknown after crash" once per job, drops them, never resubmits.
- A reopened same session (`--resume`) sweeps its own jobs directory. The registry drops on restart. No registered entries survive restart. One sweep runs after `Jobs::new`. Every plain file and symlink present at that point deletes directly (deletes the link itself, never follows; subdirectories stay put). Sweep failure logs one warn row only, never blocks opening.

### 7. Cancel, exit, and stop signals

- `job kill` takes id. Existing Esc/Ctrl-C cancel kills foreground commands (they belong to the turn). It **never** kills background jobs along the way (background jobs own no turn). Background kills use `job kill` or `/jobs kill`, plus the inspector jobs page (`/jobs` opens it) with `/jobs kill <id>` / `/jobs kill all` (text).
- Normal session exit and panic paths both stop all still-running jobs.
- **Stop signals cover the whole session, never only the process group**. A child calling `setpgid` leaves the job process group but stays in the job session. So every stop path now signals by **session**: `killpg` first, then one signal each to every process with `getsid(pid) == job session id` (macOS `proc_listallpids` + `getsid`, Linux `/proc/<pid>/stat` field 6; `libc` only, no new deps). Covers `job kill`, session exit (with `Drop`), leader natural-end cleanup, foreground command timeout/cancel. The signal order and grace are `SIGTERM` first, then `SIGKILL` after 3 s, on every stop path. The "signal only when the leader is unreaped" pid-reuse guard stays unchanged. Processes escaping the session through their own `setsid` stay out of scope (they own no part of that job session).

### 8. Sandbox and approval

Jobs start under current-tier sandbox and approval rules (one approval covers the whole command lifetime). Background changes no tier, skips no deny rules, and routes around nothing through "read output later". Approval and sandbox already happened at start. Completion notices add no rights.

The `job` tool itself never prompts: it only reads, waits on or stops the session's own jobs, and the command was approved when it started. Its rule is a fixed `allow` in every tier, so a headless session keeps it too. A user's `deny` or `ask` rule for `job` still wins, the host's tool-name gate still runs first, and a tier or a session that does not mount `job` is unaffected.

### 9. Concurrency cap

Max **4** background jobs at once. A 5th start returns a tool error naming the cap and `job kill`. 3a foreground entries never count.

### 10. `job` is the only read path

`job` owns ring, deltas, `wait`, `[status]`. It resolves job id to session-private output files. `read` never reads those files (they sit outside the workspace, sandbox-denied), and never substitutes for job-history views. Tool descriptions state "use `job` for background output".

## Constraints and Costs

- One background job costs one process group plus one output file plus one ring of memory. Caps and ring sizes are fixed.
- Wakes are product behavior: a "woken turns starting one more background command" chain self-kicks. The 10-per-10-minute rate gate gates, on by default.
- Background command end notices enter context and cost budget. Over-large results carry preview plus job id only.
- Session exit and panic paths both stop processes. The panic path adds no file I/O (`killpg`/`kill` are syscalls, not I/O).
- Restarted orphan jobs report unknown once and drop, never resubmit. Sessions depending on background commands need the model to restart them after a crash.
- `wait` stays for `radian exec` style scenes with no notice waiter. It and completion notices are two coexisting wait paths.

## Verification

`crates/radian-coding` unit tests: start returns at once, output files hold content, timeout/cancel kills sessions, exit cleanup, slot caps, foreground commands never auto-move to background (past the old threshold stays foreground, foreground default timeout ends as timeout, both rerun paths named, no job registered), foreground default 300 s and cap 600 s pinned, old config keys warn one row and ignore, explicit background ignores `timeout_secs`, `job read` deltas with `job wait` timeout never killing, `job kill` settles as killed, every `job_session_kill` stop path, truncated foreground output registers as settled entries with three `job read` by-row reads matching background jobs, untruncated registers nothing, evicted reads unknown, foreground entries take no slots and write no journal. The four old names receive unknown-tool errors in tests., `shell`/`job` definition bytes under caps, `shell` description and parameter prose pinned line by line. `radian-runtime` mocks cover busy inject, idle wake, no notice for waited/killed/exited, rate gates (minute-level chains never degrade, second-level self-kick loops degrade in bounded counts, window passing restores with no user message, user-message arrival clears). Fixture `background-jobs.jsonl` passes reference checks. `journal-selfcheck` passes.
