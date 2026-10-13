# radian-tui dogfooding: radian works on radian (2026-10-12)

The owner runs `radian-tui` (main at `fb71382`) in a herdr pane against the opencode Zen
endpoint (`deepseek-v4.1-flash`), on a worktree of this repository (`radian-dogfood`,
branch `radian/dogfood`). The first task: check that the names `AGENTS.md` and the crate
READMEs mention still exist in the code, fix only the docs.

## Findings

| # | What happened | Why it matters | Status |
|---|---|---|---|
| D1 | `--workspace` pointed at a directory that did not exist. The error began "the `workspace-write` sandbox backend is unavailable or failed its probe ... refusing to run" and named the real cause (`invalid workspace ...: No such file or directory`) only at the end of a long line. | The first thing the user reads blames the sandbox, and suggests `--profile full-access`, for a typo in a path. A missing workspace should fail as a workspace error before any sandbox probe. | open |
| D2 | That failed start still created the session file. The next start with the same `--session-id` refused: "the file already holds a complete session; refusing to overwrite it (call `resume` instead)". | A start that fails leaves a session behind, and the retry fails with a message about something the user never did. Either validate everything before creating the session, or do not leave a session for a start that never ran. | open |
| D3 | After D1/D2 the owner chose `--profile full-access` for the run. | The task's "can `cargo test` run inside the sandbox" step was therefore not exercised. | to redo under `workspace-write` |
| D4 | The `full-access` run (`dogfood2`) ended mid-task while the owner was scrolling; the owner reported it as a crash. The journal ends cleanly with `TurnFinished` `cancelled`/`requested`, no panic and no error event. Resuming a copy and scrolling hundreds of times in an isolated pty did not reproduce it. The terminal showed a flood of echoed `^[[B` (wheel-as-arrow) and a trailing `%` (no final newline). | Whatever the cause, the app left no trace of **why** it exited once the alternate screen was gone. The TUI should print one exit-reason line on the normal screen, end with a newline, and keep a panic log next to the session. | open (cause unknown) |
| D5 | Under `full-access` and `workspace-write`, every distinct shell command asked: "allow for this session" is remembered per command, and the model writes compound commands, which `--allow-shell-command` never matches. | Approval fatigue made the run unusable. | `radian-tui --yes` added (same as `agent --yes`): no dialog, rules and sandbox unchanged, audited |

The run itself is recorded below as it goes.
