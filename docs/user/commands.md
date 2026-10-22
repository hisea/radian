---
description: Every slash command, shell command, and what happens while a turn runs.
---

# Commands

A line whose first non-space character is `/` is a command. It never reaches
the model, the transcript, or the session journal. `//` is the escape: one
`/` is dropped and the rest is sent as an ordinary message. Arguments split
with shell quoting, so `/memory add "two words"` is one argument. Unbalanced
quotes are an error notice. An unknown command is an error notice, never a
message to the model.

Command output shows as transcript notices. Type `/help` to print the
command list.

Some arguments come from host data, not fixed words. `tab` completes them:
`/model` offers provider and model pairs from the catalog and your logins;
`/login` offers the provider ids (`providers.md` lists them) plus `custom`;
`/logout` offers the providers with a stored credential; `/theme` offers the
built-ins plus your theme files; `/skill:<name>` offers the discovered skill
names (`/skills` lists them with descriptions); `/memory` id arguments offer
record ids. When nothing matches, `tab` does nothing.

While a turn runs, `/queue` still works and `Enter` steers the turn. Everything
else waits or is refused. See "While a turn runs" below.

## Session

| Command | Syntax | What it does |
|---|---|---|
| `/sessions` | `/sessions [query]` | Open the session picker overlay for this workspace. With a query, filter first. Selecting shows a copyable `--resume <id>` line; it never switches sessions in place. Applies at once. |
| `/fork` | `/fork` (no arguments) | Fork this session at its last turn boundary into a child session. Only idle sessions fork. It does not switch to the child; the notice names it. Applies at once. |
| `/new` | `/new` (no arguments) | End this session and open a fresh one in the same workspace. The old session stays resumable. Refused while a turn runs, while jobs run, or after the session ended. Applies at once. |
| `/export` | `/export [md\|markdown\|json] [path] [--force]` | Export this session. `markdown` is an alias of `md`. The default format is `md`; the default path is `<workspace>/radian-<id>.<ext>`. A relative path resolves against the workspace; an absolute path may point anywhere. Refuses to overwrite without `--force`. Applies at once. Example: `/export md notes.md --force`. Related: `/sessions`. |
| `/login` | `/login [provider]` | Open the credential wizard. With a provider id or `custom`, skip the provider choice. `custom` names a new endpoint (name, protocol, base URL, key). Applies at once. |
| `/logout` | `/logout <provider>` (exactly one) | Remove one stored credential. Asks first: `y` removes, `n` or `esc` cancels. Applies at once. |

A resumed session rebuilds the history from the journal's user messages. The
history is this session's submitted lines only; it is not persisted on its
own. A non-empty input survives a clean exit in
`<session-dir>/<session-id>.draft`: `--resume` restores it once and deletes
it. A signal exit saves what was typed. An idle `ctrl-c` clears the input
first, so a `ctrl-c` exit saves the cleared line. A panic writes nothing.

## Model

| Command | Syntax | What it does |
|---|---|---|
| `/model` | `/model <provider>/<model>` or `/model <provider> <model>` | Switch the model. Takes effect at the next turn boundary. Either switches directly or asks first when the switch needs confirmation. Example: `/model openai/gpt-5`. Related: `/thinking`, `/usage`. |
| `/thinking` | `/thinking <level>` (exactly one of `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`) | Set the thinking level. `tab` completes only the levels the current model supports. An unknown level behaves like an unsupported one: the level clamps to what the current model supports. Takes effect at the next turn boundary. Example: `/thinking high`. |
| `/usage` | `/usage` (no arguments) | Show the current provider's usage snapshot, every window of it, one line each. A pure read: no network, works any time. The footer shows the short form of the same snapshot. |
| `/auto` | `/auto on\|off` | Approve tool questions without asking, for this session (`on`), or ask again (`off`). Deny rules and the sandbox still apply. Applies at once. |
| `/yolo` | `/yolo on\|off` | Alias of `/auto` with the same two words. |
| `/access` | `/access [workspace\|full]` | With no argument, show the current tier and where it came from. With a name, switch tiers. The switch takes effect at the next tool call, and `full` asks for confirmation first because it removes the sandbox. Applies at once. Example: `/access workspace`. |

## Context

| Command | Syntax | What it does |
|---|---|---|
| `/context` | `/context [all]` | Show the next request's estimated size by category, plus the reported line, the message count and the compactions. With `all`, list every context file. A pure read; works mid-turn. Applies at once. |
| `/btw` | `/btw [question]` | Ask a side question from the current context. The question is the raw remainder of the line, so quotes survive. Nothing goes into the transcript and no tool runs. The answer streams into an overlay. With no question, reopen the last answer. Only one runs at a time; a second while one runs is refused. `esc` cancels a running question or closes the answer. Example: `/btw why does this test fail?` |

## Goal and loop

| Command | Syntax | What it does |
|---|---|---|
| `/goal` | `/goal <objective>` (up to 4000 characters) | Set a persistent goal that repeats every turn until done or budgeted. Setting a new goal resets the budget to the default: 30 turns or 2 hours, no token cap. The objective is literal text; a bare number or duration is refused. Example: `/goal fix the failing tests`. Related: `/loop`, `/queue`. |
| `/goal` | `/goal` (alone) | Show the goal, its progress, and its budget use. |
| `/goal` | `/goal pause` | Stop continuing and keep the goal. |
| `/goal` | `/goal resume` | Continue a paused goal. A resumed session starts with the goal paused; resume restarts it. |
| `/goal` | `/goal drop` | Forget the goal entirely. |
| `/goal` | `/goal budget [<N> turns\|turn] [<dur>] [<N> tokens\|token]` | Cap the goal. Durations look like `30s`, `5m`, `2h`. With no pairs, show the budget. When a cap runs out the goal stops and names the cap; raise the budget to continue. Example: `/goal budget 10 turns 1h`. |
| `/loop` | `/loop [--every <dur>] [--times N] [--for <dur>] [prompt]` | Repeat the prompt every turn until `--times` or `--for` (default 20 turns or 1 hour). `--every` waits between turns. Flags come before the prompt. A bare `/loop` toggles: when off, it arms and the next message becomes the prompt. A bare number or duration needs its flag. `--times 0`, a zero or unknown-unit duration, an unknown flag, or a flag after the prompt started is a usage notice. Example: `/loop --every 30s --times 5 check the build`. Related: `/goal`. |
| `/queue` | `/queue <text>` (required) | Queue a message for the next turn as a followup, even while a turn runs. Idle, it opens a turn at once. The `@` references expand first, like a send. Example: `/queue check the build next`. Related: `/retry`. |
| `/retry` | `/retry` (no arguments) | Resend the last user message, but only when idle and the previous turn ended in a model error. The text goes as-is, with no `@` expansion. Anything else is a `nothing to retry` notice. |

## Output

| Command | Syntax | What it does |
|---|---|---|
| `/copy` | `/copy [code]` | Copy the last committed assistant message through the clipboard backend. A streaming entry never counts. With `code`, copy only its last fenced block without the fence lines. Applies at once. |
| `/compact` | `/compact [note]` | Summarize the history now and keep the recent part. The driver runs it between turns, so a running turn finishes first, and the result shows as a notice. `ctrl-c` or `esc` cancels an in-flight compaction without committing. Needs a model. Example: `/compact keep the test plan`. |
| `/help` | `/help` (no arguments) | List the commands. Applies at once. |

## Tools and jobs

| Command | Syntax | What it does |
|---|---|---|
| `/jobs` | `/jobs` (alone) | Open the inspector's jobs page. Lists the running background jobs in registry order. Applies at once. |
| `/jobs` | `/jobs kill <id\|all>` | Stop one background job or every running job. Sends SIGTERM to the process group, then SIGKILL after 2 seconds. The model gets a completion message on the busy or idle lane. Example: `/jobs kill all`. Related: `/panel`. |
| `/tools` | `/tools` (no arguments) | Open the inspector's tools page. Applies at once. |
| `/mcp` | `/mcp` (no arguments) | Open the inspector's MCP page. It lists the servers and their tools. Applies at once. Related: `mcp.md`. |
| `/hooks` | `/hooks` (no arguments) | Open the inspector's hooks page. It lists each configured entry and whether its matcher compiles. Applies at once. Related: `hooks.md`. |
| `/panel` | `/panel` (no arguments) | Open the inspector on the last used page. Applies at once. |

## Memory

`/memory` runs the same grammar as `radian memory` outside the interface
(`cli.md` holds the flags). One grammar, one executor: the TUI adds only the
framework and the notices. A read command runs while a turn is running and
does not interrupt it. A write takes effect at the next model request.
`forget`, `purge`, `split` and `merge` ask first: a notice shows the prompt,
`y` runs it and `n` or `esc` cancels. While the prompt is up, keys do not
type into the message box.

| Command | Syntax | What it does |
|---|---|---|
| `/memory list` | `/memory list [--global\|--project] [--all] [--json]` | List records, project first then global. `--all` includes archived records. |
| `/memory search` | `/memory search [--global\|--project] [--json] <words>...` | Search by keyword. Every word must match; matching ignores case. Example: `/memory search vpn profile`. |
| `/memory show` | `/memory show [--global\|--project] [--json] <id>` | Show one record in full. |
| `/memory add` | `/memory add [--global] [--kind preference\|decision\|fact\|lesson] [--key <key>] <text\|->` | Add a record. Project scope by default. The text is literal in the interface: a bare `-` writes nothing and shows `` `-` reads stdin only in `radian memory`; type the text here``. On the command line `-` reads it from stdin. Example: `/memory add The deploy needs the vpn profile "office".` |
| `/memory edit` | `/memory edit [--global] --revision <N> <id> <text>` | Change a record's content. Needs the current revision. A bare `-` as the text writes nothing and shows `` `-` reads stdin only in `radian memory`; type the text here``. |
| `/memory archive` | `/memory archive [--global] --revision <N> <id>` | Archive a record. It stays stored but is no longer injected or searched by default. |
| `/memory restore` | `/memory restore [--global] --revision <N> <id>` | Restore an archived record. |
| `/memory forget` | `/memory forget [--global] [--yes] <id>` | Forget a record. Gone from every view, never resurrected by a rebuild; the bytes remain until a purge. Asks first. |
| `/memory purge` | `/memory purge [--global\|--project] [--yes]` | Rewrite the log, removing forgotten content. Asks first. |
| `/memory confirm` | `/memory confirm [--global] --revision <N> <id>` | Confirm a record. Keeps its original source. |
| `/memory export` | `/memory export [--global\|--project]` | Print one scope as editable Markdown. |
| `/memory import` | `/memory import [--global\|--project] <source>` | Import an edited Markdown export from the file at `<source>`: a relative path resolves against the workspace in the interface and against the current directory on the command line; an absolute path may point anywhere. A bare `-` imports nothing and shows `` `-` reads stdin only in `radian memory`; give a file path here``. On the command line `-` reads it from stdin. A file that cannot be read imports nothing and shows `cannot read <path>: <error>`. |
| `/memory project` | `/memory project [--json] [new]` | Show this workspace's identity key and project state, or register it with `new`. |
| `/memory relink` | `/memory relink <project-id>` | Attach this workspace's identity key to an existing project. |
| `/memory split` | `/memory split [--yes] <from> <to> <id>...` | Move records between projects. Asks first. |
| `/memory merge` | `/memory merge [--yes] <into> <from>` | Merge one project into another, reporting key conflicts. Asks first. |

`radian memory` outside the interface adds `--workspace <dir>` to resolve the
project against another directory. Full detail, including kinds, revisions,
and the `memory_search` tool the model uses, is in `memory.md`.

## View

| Command | Syntax | What it does |
|---|---|---|
| `/theme` | `/theme <name>` (exactly one) | Switch the color theme now and write the name back to the config file. A built-in (`dark`, `light`, `mono`) or a file in `~/.radian/themes/`, named by its stem. Both `.toml` and `.json` files read; a `.toml` file wins when both exist for one stem. An unknown name warns and falls back. Applies at once. Example: `/theme light`. Related: `themes.md`. |
| `/skills` | `/skills` (no arguments) | List the discovered skills, one row per name and one-line description. Applies at once. Related: `skills.md`. |
| `/skill:<name>` | `/skill:<name> [args]` | Run a skill. The name is glued to the command with a colon. The skill body plus the arguments become one user message. Arguments are the raw remainder of the line, verbatim, so quotes survive. A `$ARGUMENTS` placeholder in the body fills with the arguments; without it, they append. Example: `/skill:review --strict`. Related: `skills.md`. |

## Shell commands

`!` and `!!` come before the `/` commands, so `!/bin/sh` is a shell command,
not an unknown command. The rest of the line is the shell command verbatim;
only surrounding whitespace is trimmed. It runs in the tier's shell, under
the sandbox when one is enforced, and it never opens an approval box. A
second `!` while one runs is refused. `esc` cancels a running `!` command.

| Command | Syntax | What it does |
|---|---|---|
| `!` | `!<command>` (required) | Run a shell command. Its output enters the model context. Example: `!make check`. |
| `!!` | `!!<command>` (required) | Run a shell command outside model context. The block draws dimmer so the exclusion reads. Its output never enters the journal or the model context. Example: `!!ls /tmp`. |

An empty command is a usage notice. Ask about radian itself in plain words
to read the built-in manual.

## While a turn runs

A submitted `Enter` on an idle composer opens a turn at once. While a turn
runs, `Enter` steers it instead: the text joins the running turn. `/queue`
always queues as a followup, even mid-turn. The footer's `queued` item shows
the waiting count.

`esc` closes a menu or clears the input first; with an empty input it cancels
the running turn. `ctrl-c` cancels the running turn. A second `ctrl-c`
within 2 seconds on an empty input quits; any other key disarms it. There is
no quit command. A side question (`/btw`) and a read command (`/context`,
`/usage`, `/memory list`, `/memory search`, `/memory show`, `/memory export`,
`/memory project`) run while a turn
runs without interrupting it. Writes take effect at the next turn boundary:
`/model`, `/thinking`, `/access`, `/compact`, `/memory` writes.
