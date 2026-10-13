# agent-cli

`agent` is a chat client: one OpenAI-compatible model, optional MCP servers, optional
built-in file/git tools (`--workspace`), and sessions that are written to disk through
`radian-runtime` / `radian-session`.

## Sessions

Every run is a durable session: one JSONL file per conversation.

- **Where**: `--session-dir <dir>`, else `$RADIAN_SESSION_DIR`, else the user's *data*
  directory — `$XDG_DATA_HOME/radian/sessions` (default `~/.local/share/radian/sessions`)
  on Linux, `~/Library/Application Support/radian/sessions` on macOS. Sessions are records,
  not a cache, so they do not live under a cache directory (ADR 0013 decision 4).
- **New session**: `--session-id <id>` names it (the same value expands `{session_id}` in
  `--header`); otherwise an id is generated and printed to stderr.
- **`--resume <id>`**: reopen a session and continue; the previous history is sent to the
  model on the next turn.
- **`--list`**: the sessions of the current workspace (`--workspace`, else the current
  directory), newest activity first: id, created, last activity, message count.
- One writer per file: a second process opening the same session is refused.
- `-p` (one-shot) writes a session too.

## Input

While a turn is in flight, a line typed at the prompt is a **steer** (it changes the current
turn's direction); an idle driver gets a **followup** (a new turn). When the approval prompt
may read stdin (interactive without `--yes`), the prompt loop waits instead of competing for
the line, so an approval answer always reaches the approval.

Ctrl-C once cancels the turn; a second Ctrl-C while it is winding down exits 130. `/exit`,
`/quit` or EOF leave normally, letting MCP servers shut down first.

## Output

stdout carries only the model's visible answer (so `-p` can be piped). Status, tool lines
and **reasoning** go to stderr; reasoning is prefixed `[reasoning]` and kept on its own
lines, never mixed with the answer (ADR 0014).

## Profiles, the sandbox, and permissions

`--profile` picks the three layers of ADR 0012 at once:

| Profile | Sandbox | Shell | MCP | Built-in files |
|---|---|---|---|---|
| `safe` | not needed (nothing executes) | not offered | denied | read/list/git allowed; write/patch denied |
| `workspace-write` (default) | enforced (macOS seatbelt / Linux bubblewrap) | offered, sandboxed | asks | write/patch allowed under the workspace |
| `full-access` | none (chosen explicitly) | offered, **not sandboxed** | allowed | write/patch allowed under the workspace |

- **`full-access` is never a default.** The default is `workspace-write`.
- **Without `--workspace` no write or execute tool exists at all**: the built-in tools are
  only mounted with a workspace, and `shell` additionally needs a sandbox.
- **A failed probe refuses to run.** If the backend is missing or its probe fails, the CLI
  exits with an error instead of falling back to an unsandboxed run. There is no degrade path.
- **`--yes` is not "allow everything".** It swaps the approver for one that answers "allow
  once" without asking; `deny` rules still deny, `allow` rules still allow, and the sandbox is
  untouched.
- **Approval** shows the tool and its resource (the full command for `shell`) and offers
  `y` (once), `a` (this session) or anything else (deny). Ctrl-C while waiting cancels the
  turn. "This session" grants are in memory only and are gone after `--resume`.
- **`--allow-shell-command PREFIX`** allows one simple shell command prefix without asking
  (e.g. `--allow-shell-command "git status"`). It replaces the blanket "shell asks" rule, so
  unmatched commands still ask; a composite command (`git status; rm -x`) never matches.
- **Audit**: every decision writes one `PermissionResolved` to the session file, with
  `approver = policy` for a rule/grant decision and `user` for an answered prompt.

The startup lines state the profile, the backend and what it actually enforces. They never
claim credential isolation: the macOS backend leaves `mach-lookup` open, so the IPC boundary
is not tightened (ADR 0012 decision 4). MCP server processes are not sandboxed by this build.
