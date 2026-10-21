# ADR 0033: `!` and `!!` Shell Commands

Status: accepted

## Decisions

1. **Semantics**: a line starting with `!` in the input box is a shell command typed by the user.
   - `!cmd` runs. **Command and output enter model context** as one `User`-source user message shaped ``Ran `cmd` `` plus fenced output (bounded truncation) and exit status. The command string itself also enters context.
   - `!!cmd` runs. Output **stays out of** context. It shows only in the transcript.
   - Parsing runs before `/` commands in `command.rs` (`!/bin/sh` style commands starting with `/` still parse as `!`). The result is `Outcome::Bash { command, exclude_from_context }`. Execution is a loop effect.
2. **No approval**: a human typed the command. Another prompt is pure friction. Approval targets model-initiated actions.
3. **Always runs in the sandbox**: the same current-tier sandbox policy as the shell tool applies. `User` source never skips the sandbox. Tier `full` holds no sandbox. It runs as usual.
4. **Journal records it for audit**: each run writes one `UserBash { command, exclude_from_context, exit_code, cancelled }` (non-critical). `!!` records the same. A context entry adds normal `InboxEnqueued`/`MessageAppended`.
5. **`cd` never persists**: the command runs in a child process. `cd` affects only that command. It never changes the session working directory.
6. **Run and cancel**: only one runs at a time. Esc cancels a running command (same mechanism as turn cancel). `!` waits for turn end or queues while a model turn runs.
7. **No PTY**: it runs through plain pipes. PTY needs the ADR 0016 TIOCSTI constraint first. Sandboxed `TIOCSTI` on a terminal held outside the sandbox must fail. It enters the escape matrix.
8. **Display**: one transcript block shows it. The first row is `$ cmd`. `!` and `!!` use different colors for in-context versus out-of-context. `/help` and the tip table gain one row.

## No changes to `agent-core`

Execution lives in host (same path as the `radian-coding` shell tool). A context entry is a normal `User` message (ADR 0005).

## Constraints and Costs

- No approval: only sandbox and deny rules constrain a dangerous command. A user-typed command equals a command run in the user own terminal.
- Command and output in context consume budget (ADR 0018 `B`). Output truncates with bounds.

## Verification

`agent-cli`/`radian-tui` tests cover `!`/`!!` parsing, in-context and out-of-context output, sandbox execution, and journal `UserBash`. Fixture `user-bash.jsonl` passes reference validation.
