# agent-cli

The library behind `radian exec` (ADR 0032): a chat client with one model
(OpenAI-compatible or Anthropic), optional MCP servers,
optional built-in file/git tools (`--workspace`), and sessions that are written to disk
through `radian-runtime` / `radian-session`.

## Providers

`--provider` picks the wire protocol; the default is unchanged.

- `openai-compatible` (default): `/chat/completions`. `--base-url` defaults to
  `https://api.openai.com/v1`; the key comes from `$OPENAI_API_KEY` unless `--api-key-env`
  names another variable.
- `anthropic`: the Messages API (`/v1/messages`). `--base-url` defaults to
  `https://api.anthropic.com`; the key comes from `$ANTHROPIC_API_KEY` unless `--api-key-env`
  names another variable. `--max-tokens` (default 16384) is sent because the Messages API
  requires the field. `--thinking off|adaptive|enabled:<budget>` enables extended thinking
  (default `off`; `adaptive` is the recommended mode on Claude 4.6+); `enabled:<budget>`
  sends `{"type":"enabled","budget_tokens":N}`.

The key is read only from the environment variable, never from the command line; it is never
printed and never written to the session file. The session's config snapshot records the
provider name and the model, not the key or a header value.

`radian exec --version` prints `radian <version>` and exits 0 before any setup runs: no key is read, no
session file is created, and no network is touched. The version is the package's own
(`CARGO_PKG_VERSION` in `Cargo.toml`), never a literal in the source.

## Sessions

Every run is a durable session: one JSONL file per conversation.

- **Where**: `--session-dir <dir>`, else `$RADIAN_SESSION_DIR`, else `~/.radian/sessions`
  (`$RADIAN_HOME/sessions` when `RADIAN_HOME` is set; ADR 0020). Sessions are records, not a
  cache. The same `~/.radian/` root holds `config/`, `cache/` and `memory/`; the sandbox
  denies read on `sessions/` and `memory/`.
- **New session**: `--session-id <id>` names it (the same value expands `{session_id}` in
  `--header`); otherwise an id is generated and printed to stderr.
- **`--resume <id>`**: reopen a session and continue; the previous history is sent to the
  model on the next turn.
- **`--list`**: the sessions of the current workspace (`--workspace`, else the current
  directory), newest activity first: id, created, last activity, message count.
- One writer per file: a second process opening the same session is refused.
- `-p` (one-shot) writes a session too.

## Session management

Three read-only commands work on the sessions of the **current workspace** (`--workspace`,
else the current directory). None of them starts the model, none needs `--model`, and none
creates or modifies a session file (`.jsonl`).

- **`--list`**: id, created, last activity and message count, newest first.
- **`--export <id>`**: the session as Markdown on stdout by default;
  `--format json` gives the raw event document. `--out FILE` writes it to a file instead
  (refused if the file exists, unless `--force`).
  - **Redaction cannot be turned off** (ADR 0013 decision 3): this process's key (from
    `--api-key-env`) and every `--header` value are redacted, on top of the always-on
    `sk-`/`Bearer`/`Authorization` patterns. Opting in to extra content never weakens it.
  - **Reasoning and raw provider payloads are omitted by default** (ADR 0014, ADR 0013);
    `--include-reasoning` and `--include-provider-payload` add them to this export only.
  - **Only the current workspace's sessions can be exported.** Another workspace's id is
    refused with `session "<id>" is not in this workspace`, the same filter `--list` uses.
  - The export reads the session JSONL directly; it does not create or update the index.
- **`--search <text>`**: substring search over the visible message text, one tab-separated
  row per hit: `session-id`, last activity, and the hit snippet on a single line. No match
  prints nothing on stdout, says `no match` on stderr and exits 0. `--list` and `--search`
  share the `index.db` cache in the session directory; it is disposable and re-derivable
  from the JSONL, never a session file (ADR 0006 §10). `--export` reads the JSONL directly
  and does not touch the index.

- **`--fork <parent>`** (ADR 0022): create a child session whose initial history is the
  parent's **effective** history at a turn boundary (a committed compaction is in effect, an
  uncommitted replacement is dropped, unpaired calls are repaired). `--at <seq>` picks a
  boundary; omitted, the parent's last `TurnFinished` is used. `--session-id <child>` names
  the child (generated otherwise). It needs no model and no key, and the child inherits the
  parent's workspace and config snapshot. The child is written and fsynced **first**; the
  parent then gains exactly one `SessionForked`, so it never records a child that does not
  exist. A parent another process is writing is refused with exit 1, without waiting.
  - A fork point must be a turn boundary: a mid-turn seq, a seq inside a replacement
    segment, an unknown seq and a seq past the end are each refused with the reason.

These are their own mode: `--export`/`--search`/`--list`/`--fork` conflict with `--prompt`,
`--resume` and each other, and `--list`/`--search` also conflict with `--session-id` (a
listing does not use one; `--fork` uses it to name the child); clap rejects a combination with
exit 2. `--model` is only required for a conversation (`-p` or interactive).

A closed stdout is not an error: `radian exec --list | head -1` (and the same for `--export`,
`--search` and every `radian memory` subcommand) stops quietly with **exit 0** and prints
nothing. The process's default `SIGPIPE` disposition is deliberately **not** restored: a write
to an exited MCP child's pipe would then kill the whole command with a signal.

## Memory (`radian memory …`)

`radian memory` is the user side of ADR 0021 (M1): durable memory shared across sessions,
stored under `~/.radian/memory/` (`$RADIAN_HOME/memory`). It is its own mode: **no `--model`,
no key, no session file, no network**. Put `--workspace <dir>` before `memory` to name the
workspace; the default is the current directory.

Memory is scoped to a **project** (a git repository, shared by all its worktrees through the
git common dir, or the canonical workspace path outside a repository) and to **Global**. Reads
default to Project then Global, with a Project record overriding a Global one with the same
key. Writes default to Project; `--global` writes a Global record.

| Command | What it does |
|---|---|
| `list [--global\|--project] [--all]` | Active records (`--all` adds archived), Project first; `--json` for machine output |
| `search <词…> [--global\|--project]` | keyword search (every word must match); `--json` |
| `show <id> [--global\|--project]` | one record; `--json` |
| `add [--global] [--kind preference\|decision\|fact\|lesson] [--key KEY] <内容\|->` | a new record; `-` reads stdin; the default kind is `fact` |
| `edit <id> --revision N <内容\|->` | change the content |
| `archive <id> --revision N` / `restore <id> --revision N` | lifecycle |
| `confirm <id> --revision N` | confirm an Agent/Import record (its `source` is kept) |
| `forget <id>` | logical delete (never resurrected by a rebuild); needs confirmation |
| `purge [--global\|--project]` | physically rewrite the log, removing forgotten content; needs confirmation |
| `export [--global\|--project]` / `import [--global\|--project] <文件\|->` | the editable Markdown view |
| `project` / `project new` | show the identity key and project state, or register it; `--json` |
| `relink <ProjectId>` | attach this workspace to an existing project |
| `split <from> <to> <id…>` / `merge <into> <from>` | move records between projects; need confirmation |

- **A write registers the project itself** (ADR 0021): a default-scope `add` in a
  git repository whose project is not in the registry registers it, saves there, and prints
  one `[memory] created project …` notice line naming the project and its identity key. When a
  project with the same repository name already exists (a moved or re-cloned repository), the
  notice adds the `relink`/`merge` hint. **Only a write registers**: reads and injection never
  do — a default-scope `list`/`search` on an unregistered workspace reads Global only and says
  so. A directory that is not a git repository is **not a project workspace**: a default-scope
  `add` fails with "not a project workspace … the memory was not saved", registers nothing and
  never points at another scope; `radian memory project new` still registers such a directory
  by hand. `project`/`relink` remain the explicit route to link or re-link an identity key.
- **`--global` is for facts that hold in every project**, or when the user asks for global: a
  memory that mentions this project's paths, branches or structure belongs to the project
  scope (the `memory_save` tool description says this too).
- **Confirmations** (`forget`, `purge`, `split`, `merge`): a terminal asks `y/N`; `--yes`
  skips it; **without a terminal and without `--yes` the command refuses, exits 2 and does
  nothing**.
- **A command that takes an `<id>` finds the scope itself** (`show`, `edit`, `archive`,
  `restore`, `confirm`, `forget`): an id is unique across scopes, so the command looks the id
  up in Project (when this workspace is linked) and then Global and acts on the scope that
  holds it — a Global record still works from an unlinked workspace. An explicit
  `--global`/`--project` **only restricts** the search to that scope; an id that no visible
  scope holds is `no memory record <id>`.
- Every user command writes `source: user`; the store applies the revision and same-key rules
  (`KeyConflict`, `RevisionConflict`).
- **Exit codes**: 0 on success; 1 for a conflict, a missing record or an unlinked workspace;
  2 for a usage error or a refused confirmation (plus clap's own usage failures).

### The agent's own memory tools and injection (ADR 0021 D5/D6)

During a conversation the model also gets four tools: `memory_search` and `memory_get` (read,
**allowed** by the rules) and `memory_save` and `memory_revise` (write, always **asked**;
`--yes` answers the ask and the decision is still audited). Agent writes are `source: agent`
and `unverified`; a user record is never modified, and a user key still wins. A `memory_save`
in the default scope registers a git workspace whose project is not registered yet (the same
notice line as above, on stderr); a directory that is not a project workspace is refused with
a tool error that says the memory was not saved and does not point at another scope.

Each request also injects the current memory into the **system prompt** (never the
transcript): Active records in the D6 order, up to 2000 tokens (Global 500 / Project 1500 soft
shares, whole records only), with the user/confirmed and agent-unverified segments labelled
apart. The injected ids/revisions/scopes are recorded as a `MemoryInjected` runtime event when
the set changes; the content is never written. A memory read failure is a `[memory]` warning,
never a failed turn. Startup prints `[memory]` with the project state and the budget.

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

## Context budget (ADR 0018) and the model catalog

The context window `C` comes from one of three places, in order:

1. **`--context-window <TOKENS>`** — explicit, and always wins. It never fetches: only an
   already-existing cache is read for the `input`/`output` numbers, and with no cache just the
   explicit number is used.
2. **The models.dev catalog**, matched by `(provider, model)`. The provider is
   `--catalog-provider`, or derived from the `--base-url` hostname (`opencode.ai` →
   `opencode`, `api.anthropic.com` → `anthropic`, `api.openai.com` → `openai`,
   `api.deepseek.com` → `deepseek`, ...). The catalog is a read-only GET with **no key, no
   header and no user data**, cached owner-only under `~/.radian/cache/catalog/models.json`
   (`$RADIAN_HOME/cache/catalog/models.json`)
   and refreshed at most once every 24 h (3 s timeout; a failed fetch falls back to the
   cache). `--offline-catalog` never goes out; `--catalog-provider` overrides the hostname
   mapping.
3. Neither: `C` is **unknown**, there is no budget, and the startup says so. No number is
   guessed (a model-name-to-window table would age and be mostly remembered numbers —
   ADR 0015 C1).

The catalog reports `context` (C), `input` (I) and `output` separately. The effective input
budget is **`B = min(I, C − R)`**, where **`R = max(min(16384, ⌊C/4⌋), the request's max
output)`** (ADR 0018) is the reservation kept for the answer and the next round: a
quarter of the window, capped at 16384, raised by the output the request itself sends. A
missing `I` falls back to `C − R`; a missing `C` leaves `B = I`. **`C ≤ R` is a startup error**
(the window cannot hold the reservation), never a silently clamped number; with the adaptive
`R` it takes the request's own output reaching `C` (for example Anthropic's default
`max_tokens = 16384` against an 8192-token window).

- The startup `[budget]` line shows `C`, `I`, `output`, `R` and `B`, and where they came from.
  The catalog's `output` is only **checked** against the request's max output (a warning); it
  does not change `R`.
- Crossing `B` does **not** stop the turn: the projection is recorded and the request is sent
  anyway (ADR 0018 replaced ADR 0015's stop). The provider's over-length error remains the only
  thing that refuses an oversized request.
- A provider-reported input is trusted as reported, and only the growth after it is estimated.
- **Compaction (ADR 0018)** keeps a request inside the window. When the projection crosses
  `B`, the history is summarized (the recent part is kept) before the request is sent;
  `--no-auto-compact` turns that proactive path off. If the provider still reports an
  over-length error, an **overflow fallback** compacts to half of `min(T_before, B)` and
  retries the request once (even under `--no-auto-compact`). Interactive mode also accepts
  `/compact [note]` — a whole line, not a message, and not available in `-p` — which
  summarizes on demand between turns; Ctrl-C cancels it without committing.

## Configuration file (ADR 0008)

Read-only, read from `--config FILE`, else `RADIAN_CONFIG`, else
`~/.radian/config/config.toml` (`$RADIAN_HOME/config/config.toml`; ADR 0020). A missing file is
an empty configuration and is never created. The file is a **default**; every flag wins.

Wired here: `model.provider`, `model.base_url`, `model.model` (so `--model` is optional when it
is set), `model.api_key` (a `{ from = "env", name = "…" }` reference, equivalent to
`--api-key-env`), `model.headers` (equivalent to `--header`), `model.context_window`,
`permissions.profile`, `compaction.auto`. `mcp.*` and `model.include_usage` are known and not
wired yet; the startup names every ignored key (never a value).

A secret **value** is refused: a bare string on `api_key`/`token`/… and a secret-looking string
anywhere (the ADR 0008 §4.2 heuristic) both fail startup, and the message names the key path
without echoing the value. `schema_version` is required; a newer one is read read-only with a
warning, an older one is refused. See the root `README.md` for the example file.

## Migrating a Pi configuration (`radian config import-pi`)

`radian config import-pi --from <pi-agent-dir>` drafts a Radian configuration from a Pi agent
directory (`~/.pi/agent` by default for Pi; `PI_CODING_AGENT_DIR` moves it). It is its own
mode: **no `--model`, no key, no session file, no network**.

```sh
radian config import-pi --from ~/.pi/agent > /tmp/radian.toml   # a draft on stdout
radian config import-pi --from ~/.pi/agent                      # review it, write nothing
radian config import-pi --from ~/.pi/agent --write              # save it (refuses to overwrite)
```

- `--from` is **required** and has no default: the command never reads a directory you did not
  name. A missing directory exits 1.
- The **draft** goes to stdout; the **report** (what was mapped, what was skipped and why, what
  you still have to do) goes to stderr. Nothing is written without `--write`.
- `--write` writes to the file `--config`/`RADIAN_CONFIG` names, else
  `~/.radian/config/config.toml`, atomically and owner-only (`0600`, directory `0700`), after
  validating the draft with the same reader the host uses. **An existing file is refused**
  (exit 1) — this version has no faithful write-back (ADR 0008 §5.4).
- **Credentials are never migrated.** `auth.json` is never read (not even `stat`ed), so an
  unreadable credential file cannot fail the run. A `$NAME`/`${NAME}` value becomes
  `{ from = "env", name = "NAME" }`; a **literal** value is not copied — the draft references a
  suggested variable and the report says so without ever showing the value; a `!command` value
  is listed and **never executed**.
- Radian does **not** promise compatibility with Pi's file format. See
  [`docs/pi-migration.md`](../../docs/pi-migration.md) for the mapping table.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | the turn completed |
| 1 | the turn failed, or the driver ended with an error |
| 2 | usage or setup error (including a failed sandbox probe) |
| 130 | forced exit after a second Ctrl-C |

## Profiles, the sandbox, and permissions

`--profile` picks the three layers of ADR 0012 at once:

| Profile | Sandbox | Shell | MCP | Built-in files |
|---|---|---|---|---|
| `safe` | not needed (nothing executes) | not offered | denied | read/list/git allowed; write/patch denied |
| `workspace` (default) | enforced (macOS seatbelt / Linux bubblewrap) | offered, sandboxed | asks | write/patch allowed under the workspace; network on |
| `full` | none (chosen explicitly) | offered, **not sandboxed** | allowed | write/patch allowed under the workspace |

- **`full` is never a default.** The default is `workspace`.
- **The old names `workspace-write` and `full-access` are still accepted as aliases** (ADR
  0016); they are not listed in `--help`.
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
