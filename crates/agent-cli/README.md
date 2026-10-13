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
