# ADR 0031: Project Trust

Status: accepted

## Background

A cloned or opened workspace that can quietly install its own instructions, scripts, and services before going live leaves the user no chance to refuse. Project trust decides "whether to read in workspace-provided resources". It **never** decides what tools can touch.

## Decisions

### T1 What trust gates and what it leaves alone

Project resources that trigger a trust decision (only listed resources trigger):

| Resource | Meaning |
|---|---|
| `<dir>/.agents/skills/` (`dir` from workspace up to repository root) | Project skills (ADR 0025; loaded only when trusted) |
| `<workspace>/.radian/config.toml` | Project narrow configuration (ADR 0008; only `permissions.profile` reads, and it only narrows; its narrowing ignores trust) |
| `<workspace>/.mcp.json` | Project MCP servers (read and started only when trusted; untrusted never reads the file and never starts any process) |
| `<workspace>/.radian/hooks.json` | Project external-command hooks (loaded and run only when trusted; untrusted never reads the file and never starts any hook process) |

- A bare `.radian/` directory never triggers. Only a listed resource triggers.
- **Trusted loads**: project skills enter discovery only when trusted (system prompt, `/skills`, `skill` tool, and `/skill:` all work). Untrusted means invisible and uncallable; startup and non-interactive notice lines state it as-is. Project `.mcp.json` works the same: only trusted reads it, and its stdio servers start only when trusted. Untrusted never reads the file and never starts any process; startup and non-interactive notice lines state it as-is. Project `.radian/hooks.json` works the same: only trusted loads and runs its hooks. Untrusted never reads the file and never starts any hook process; the `[hooks]` startup line states it as-is. **The exception is `<workspace>/.radian/config.toml`**: it still triggers the question, but its `[permissions] profile = "workspace"` narrowing **always applies**, trusted or not, because it only narrows. That file holds no trust-gated content.
- **Directory trust is total**: a trusted directory `.mcp.json` loads directly with no per-content hash confirm. A later-changed file loads its new content directly at next startup with no new question. Cost is accepted: a trusted project brings a new server through `git pull`, and next startup runs it directly.
- **It is not a complete startup boundary**: refusal never revokes already-loaded runtime configuration, session files, or workspace resolution from before this session.
- **It never limits tool calls**: trusted or not, tools still follow ADR 0012 `ToolRuleSet`, ADR 0016 tiers, and the sandbox. Trust neither widens nor narrows these boundaries.
- Context files (`AGENTS.md` and `CLAUDE.md`) **never gate**, but they always count as untrusted input (ADR 0036).
- `<workspace>/.radian/` holds only `skills/` and `config.toml`. `.radian-sandbox/` never gates and never counts as a resource.

### T2 Record location and format

- Location is `~/.radian/trust/trust.json` (`$RADIAN_HOME/trust/trust.json`). It never lives in `~/.radian/config/` (configuration is human-written; trust records are machine-written).
- Format is one JSON object `{ "<normalized directory path>": true | false }`, UTF-8, keys sorted, temp file plus `rename` atomic write, 0700 directory and 0600 file modes.
- Reads accept only `true` and `false`. Other types or non-objects **wholly reject with a warning** (fail closed: re-ask rather than guess). A missing file means empty decisions.
- Each write also writes one `ProjectTrustChanged { path, decision }` (non-critical) audit record.
- **The sandbox cannot read it**: `~/.radian/trust/` joins the ADR 0020 denylist (same as `sessions/`, `memory/`, and `credentials/`). The host creates it (0700) before hiding it in the sandbox. The escape matrix covers `trust_unreadable`.

### T3 How to ask, options, and how to change

**When to ask**: once at session startup, on finding a listed resource with no saved decision (and no command-line override). Before asking it never loads project skills and never reads `.mcp.json`. The `config.toml` narrowing exception always applies.

**TUI**: one startup modal (not the `ask_user` tool: that tool asks model-turn questions and needs a Prompter and pending queue; the trust decision happens before any turn). Options:

1. **Trust this directory and use the sandbox with this project** (saves trust and remembers `workspace`)
2. **Trust this directory** (saves)
3. **Trust parent directory `<path>`** (saves the parent and clears this directory, so children inherit)
4. **Trust only this session** (no disk write)
5. **Distrust this directory** (saves `false`)
6. **Distrust, only this session** (no disk write)

The default highlight is **option 1 (trust with sandbox)**. Esc means option 6 (no decision, closes the popup), not option 5. Both load no project skills and read no `.mcp.json`. They differ only in persistence. The modal carries one boundary line: "trust only decides whether to load resources this directory provides; the `config.toml` narrowing exception always applies; tool permissions, sandbox, and approval never change."

The modal also lists the MCP servers this directory loads when trusted: one row each with name, `command` plus `args` (pre-`${VAR}`-expansion text), and `env` key names (no values). Skipped http, sse, and ws entries mark "skipped". The modal also lists the project hooks it loads: one row each with event, `matcher` (raw; empty or `*` shows `*`), and `command` (raw). A trusted directory shows no popup.

**`radian exec` (line mode)**: the same startup, the same options, and the same server list ask in terminal question shape. `radian exec` never registers `ask_user`, but this is the host own startup question.

**Non-interactive and no TTY**: **default distrust**, with one line stating the fix (`project resources not loaded (untrusted): pass --trust to load <path>, or run radian interactively`). This line covers project skills. The `config.toml` narrowing still applies. Project `.mcp.json` works the same: without `--trust` it never reads and never starts any process, and the startup line states it as-is (`[mcp] project .mcp.json not loaded (untrusted): …`). No `defaultProjectTrust: "always"` style default pass exists. Automation passes explicit `--trust`.

**Command-line override** (highest priority): `--trust` and `--no-trust` **cover only this process and never write disk**. Persistence happens only from interactive choice or explicit `radian trust` and `/trust`. Subagent children inherit the parent trust decision: a trusted parent `general` child starts with `--trust` and loads the same project `.mcp.json`. An `explore` child starts with `--no-config-mcp` and reads no implicit MCP sources (neither `[mcp]` nor project files), even with a trusted parent.

**How to change**:

- TUI: `/trust` (no arguments shows the current directory decision and source), `/trust status|on|off|forget`.
- CLI: `radian trust status|on|off|forget|list`.
- **Effect timing**: commands change **disk state**. Loaded resources in the current session never hot-unload. `/trust off` means "next startup never loads"; the current session prints one line. Restart the session for immediate tighten. A mid-session `.mcp.json` change likewise takes effect at next startup. Loading is boundary-effective configuration, not live state (ADR 0004).

### T4 Relation with tiers, approval, and `--yes`

Four things stay independent:

| | Controls what | Decided by what |
|---|---|---|
| **Trust** | Which project resources **load at startup** (except the `config.toml` narrowing) | One explicit user choice, stored in `trust.json` |
| **Tier** | What tools **can touch** (files, network, devices) | Startup parameters and configuration, default `full` (ADR 0043) |
| **Approval, auto, `--yes`** | Whether some `ask` asks a human | Profile plus `--approval`, `--yes`, and `/auto` |
| **`ToolRuleSet` and sandbox** | Actual execution boundary | Profile, narrowed only by hard-deny |

- `full` never means trusted: one run with `--profile full` never permanently trusts some cloned project. `full` and trust are fully independent; `full` still asks.
- `--yes` and auto **never** affect trust: trust is a load decision, not an approval.
- Trust never widens permissions: even with a trusted directory, its skill commands still pass `ToolRuleSet`, tier, and sandbox. `allowed-tools` stays unwired.

### T5 Path details

- **Normalization**: store and lookup keys both first `canonicalize` (resolve symlinks, drop `.` and `..`) then take the absolute path. One directory symlinks share one decision.
- **Subdirectories**: look upward for the nearest saved decision. A trusted `<repo>` auto-covers `<repo>/sub`. A separately saved `false` for `<repo>/sub` overrides a `true` for `<repo>` (nearest wins).
- **Keys are directory paths**, not git common dirs: trust binds "what resources this directory holds". Worktrees each hold their own checkout. Ancestor inheritance already covers subdirectories.
- **A moved or renamed directory**: the normalized path changes, so saved keys no longer match. It asks again. No inode or git-remote tracking follows moves.
- **A symlinked workspace**: stores and checks by the real path.

### T6 Verification

Only temporary directories are used. Coverage: decision parsing (command line beats nearest ancestor beats default; symlinks, subdirectories, moved directories, true and false overrides); record reads and writes (atomic, sorted, 0600 and 0700, bad JSON wholly rejected, missing means empty); triggers (bare `.radian/` never triggers; `.agents/skills/<x>/SKILL.md` at any level from workspace to repository root, `.radian/config.toml`, `<workspace>/.mcp.json`, or `<workspace>/.radian/hooks.json` triggers; `.radian/skills` never triggers; `.radian-sandbox/` never triggers); untrusted project skills never load (CLI tests whether they enter the system prompt), while `config.toml` tier narrowing still applies; untrusted `.mcp.json` never starts any process with a startup-line note, `--trust` loads it; a trusted then changed `.mcp.json` loads new content directly at restart with no new question; untrusted `.radian/hooks.json` never loads and never runs with a startup-line note, `--trust` loads it, and a trusted then changed file loads directly at restart the same way; trust popup and line-mode question list servers to load (name, raw command, env key names, no values; skipped entries marked) and hooks to load (event, `matcher`, `command`); project-entry credential variables expand empty (user-level entries as usual), and `inherit_env` credential variables are removed; six-option TUI and CLI interaction; non-interactive default distrust plus one line; `--trust` and `--no-trust` override with no disk write; `/trust` and `radian trust` change disk state and never hot-change the current session; sandbox `trust_unreadable`; each write produces `ProjectTrustChanged`.

## Constraints and Costs

- **One more startup friction**: first startup in a directory with project resources asks one more question.
- **Symlinks share decisions**: a symlink to a trusted directory auto-trusts (from normalization). The attack surface is "one who can already write symlinks".
- **Moves and renames re-ask**.
- **Trust records per directory, not per git project**: worktrees and multiple checkouts of one repository each trust once.
- **The gate never guarantees safety**: trusted resources can still hold malicious text (prompt injection). Safety still rests on sandbox, tiers, and approval. Docs and startup notices never hint "trusted means safe".
- **Cost of directory-trust-is-total**: a trusted project brings a new server through `git pull`, and next startup runs it directly with no new question.
- **Credential variable table**: project entries expand a fixed constant table in code to empty (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENCODE_API_KEY`, `OPENCODE_GO_API_KEY`, plus common third-party provider keys: `DEEPSEEK_API_KEY`, `GEMINI_API_KEY`, `GOOGLE_API_KEY`, `OPENROUTER_API_KEY`, `GROQ_API_KEY`, `XAI_API_KEY`, `MISTRAL_API_KEY`). That table keeps user shell keys away from project servers, built-in or not, all inside. It uses no pattern match. Adding a built-in provider syncs the table.
