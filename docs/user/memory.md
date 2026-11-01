---
description: Save durable memory as Markdown files, and the commands that manage them.
---

# Memory

Memory is a directory of small Markdown files. Each file is one record; its frontmatter is
the record's metadata and its body is the content. Writing a file remembers something;
deleting it forgets it. There is no id, revision, archive or event log.

```
~/.radian/memory/
  global/
    MEMORY.md                 the global index
    <name>.md                 a global record
  projects/
    <project-id>/
      MEMORY.md               that project's index
      <name>.md               a project record
  projects.jsonl              the identity key → project id registry
```

The memory root is `$RADIAN_HOME/memory` when `RADIAN_HOME` is set, otherwise
`$HOME/.radian/memory`. Directories are owner-only and files are owner-only. Every write is
atomic: a crash leaves either the old file or the new one.

## Memory files and `last_used`

A memory file holds one record. Its frontmatter carries `name`, `description`,
`type`, `source`, `created`, `modified`, `last_used` and an optional
`provenance`. When the model `read`s a memory file other than `MEMORY.md`, the
host sets `last_used` to the time of the read and changes no other byte. A
user command (`list`, `show`, `stale`) never changes it. An absent `last_used`
means the file was never read.

The model writes these files with the ordinary `write` and `patch` tools. In both tiers those
tools reach only the global directory and, when the workspace maps to a project, that
project's directory. Every other path under the memory directory stays denied to those tools.
Under the `workspace` tier the sandbox also keeps `shell` from reading or writing the memory
directory; the `full` tier runs no sandbox. See `permissions`.

## Frontmatter

A record starts with `---`, then one field per line, then `---`, then the body.

| Field | Type | Values and rules |
|---|---|---|
| `name` | string | The file stem: `a-z`, `0-9` and `-`, 1 to 64 characters. It must equal the file stem. |
| `description` | string | One line. The index shows it. |
| `type` | closed set | `user`, `feedback`, `project`, `reference`. `feedback` and `project` records carry `**Why:**` and `**How to apply:**` lines in the body. |
| `source` | closed set | `model`, `user`, `import`. `add` writes `user`; the model's own writes carry `model`, as the memory prompt tells it. A value outside the set makes the file unreadable. |
| `created` | RFC 3339 UTC | Set once. |
| `modified` | RFC 3339 UTC | Set on every write. |
| `last_used` | RFC 3339 UTC or absent | Optional. Absent means the record was never read. A model `read` sets it; a user command never changes it. |
| `provenance` | optional map | `session_id` and `turn_id`, each optional. A model-written record may carry them. |

A file whose frontmatter does not parse, or whose `type` or `source` is outside its set, is
reported by `list` as unreadable with its path and line. Reading it is an error naming the
path and the line, and the other files still list.

The body is free Markdown. It keeps its bytes exactly, including trailing newlines, blank
lines and a line that spells `---`.

## The index

Each scope has one `MEMORY.md`. Its form is one line per record:

```
- [title](name) — description
```

`name` is the record's file stem; the link target is that stem plus `.md`.

The host maintains the index when a command creates, forgets, splits or merges a file. The
index is what the model receives at the start of a session; the bodies stay on disk and the
model reads one with `read`.

## Scopes, projects and the workspace

A record is **global** or belongs to one **project**. The default read order is project first,
then global. When both scopes hold a file with the same name, `show`, `edit` and `forget`
act on the project copy.

A project's identity key is the canonical path of the repository's git common directory, so
all worktrees of one repository share a project. Outside a repository it is the canonical
workspace path. The registry maps that key to a stable project id. Only a write registers a
project, and only when the workspace is a git repository; a read never registers one and a
directory outside git is never registered automatically. `project new` registers the current
directory by hand.

## The commands

Type `/memory <command>` in the interface, or run `radian memory <command>` outside it with
the same grammar. Add `--workspace <dir>` outside to resolve the project against another
directory.

| Command | What it does |
|---|---|
| `list [--global\|--project] [--json]` | List one line per file: scope, name, type, source, modified time and description. The project scope comes first, then global. An unreadable file is one line marked `unreadable` with its reason. `--json` prints the frontmatter objects instead, plus `scope`. |
| `show [--global\|--project] [--json] <name>` | Print the file's frontmatter and body exactly as stored. `--json` prints `{"frontmatter": {…}, "body": "…"}`. |
| `stale [--days N] [--global\|--project] [--json]` | List memory files not used for a while. A file is stale when its `last_used` is older than N days, or it has no `last_used` and its `created` is older than N days. `--days` is an integer of 1 or more; the default is 30, and `--days 0` is a usage error. One line per file: `scope  name  last_used-or-never  description`. `--json` gives an array of objects with the frontmatter fields, with `last_used` null when never read. A file whose frontmatter cannot be read is listed as unreadable and the other files still list. |
| `add [--global] [--type user\|feedback\|project\|reference] <name>` | Create `<name>.md` with an empty body and `source: user`, `created` and `modified`; append its index line; open it in the editor. `--type` defaults to `user`. Project scope by default. A name outside the rule, or one that already exists, is a usage error and writes nothing. |
| `edit [--global] <name>` | Open an existing file in the editor. A missing file is an error. |
| `forget [--global] [--yes] <name>` | Delete the file and its index line. Asks first; `--yes` skips the question. Off a terminal without `--yes` it refuses and keeps the file. |
| `open [--global\|--project]` | Print the scope directory. In the interface, open that directory with the editor or the platform opener. |
| `project [--json] [new]` | Show this workspace's identity key and project state, or register the workspace with `new`. |
| `relink <project-id>` | Attach this workspace's identity key to an existing project. |
| `split [--yes] <from> <to> <name>...` | Move each named file and its index line from one project to another. A name already present in the target is reported and left in place; a missing name is reported. Asks first. |
| `merge [--yes] <into> <from>` | Move every file from one project into another. When both hold the same name, the file from `from` is renamed `<name>-2` (then `-3`, …), its `name` line is rewritten, and the collision is reported. Asks first. |

`forget`, `split` and `merge` are the only commands that ask for confirmation. `list`,
`show`, `stale`, `project` and `open` are reads: they run while a turn runs and do not
interrupt it.

`archive`, `restore`, `confirm`, `purge`, `export`, `import` and `search` do not exist. A call
to one exits 2 with an error naming the current command set.

A usage or refusal error exits 2 on the command line; a missing file or an I/O failure exits 1.

### Editor behavior

`add`, `edit` and `open` use `$EDITOR` when it is set, otherwise the platform opener (`open` on
macOS, `xdg-open` elsewhere). The headless command waits for the editor; the full-screen
interface starts it detached. Off a terminal, `add` and `edit` create or name the file, print
its path, and say that no editor was opened. `open` always prints the directory path outside
the interface.

### What `--json` prints

`list --json` is an array in the same order as the text list. A readable entry carries
`scope`, `name`, `description`, `type`, `source`, `created`, `modified`, `last_used` and
`provenance`. An unreadable entry carries `scope`, `unreadable: true`, `path` and `reason`.

`show --json` is one object with `frontmatter` (the same fields) and `body` (the stored body).

## What the model is told

When memory is on and a memory directory exists, the base prompt carries one memory block
between the role prose and the context files. `[memory] enabled = false` removes the block
together with the index section; no memory directory removes both too. The block tells the
model:

- Write a memory file after you correct the model, at a handoff, or when it learns a fact
  that helps a later session.
- Do not write what the repository already records: code structure, git history or
  `AGENTS.md`. Project facts and project procedures go to `AGENTS.md`; cross-project
  procedures go to a user skill.
- Write one file per fact in the memory directory the index section names. Give each file
  `name`, `description`, `type`, `source`, `created` and `modified` frontmatter fields, and
  add one `- [title](file) — description` line to the index. Update the file on the same
  subject instead of writing a new one. Delete a file that is wrong.
- Content that names this repository's paths, branches, files, decision numbers, task
  numbers or structure is project. Your preferences, corrections of the model's way of
  working and pointers that hold across repositories are global. When unsure, the model
  writes project. `type` is independent of scope.
- Memory is not proof of current behavior. The model verifies that a file, flag or command
  still exists before it recommends one. It cites only the memory it used. It consults
  memory when the request names the repository, an earlier decision or continuity, and not
  when the request is self-contained.
- Read the project index before the global index. On the same subject, the project file wins.

A custom system prompt (`--system` or `--system-file`) replaces the role prose. The memory
block stays, like the index section, because it is host policy.

## What the model sees

At the first model call of a session, the model receives the two index files as one
system-prompt section: the project index first, then the global one. Each part is the text of
that scope's `MEMORY.md`. The section names each file's path, so the model reads a record's
body on demand with `read`. It opens with a line that says the content is data from your
memory directory, not instructions.

A scope with no index, or an empty one, contributes one line saying so. With no project
linked, only the global index is read.

Each index is capped. `[memory] index_max_lines` (default 200) limits its lines and
`[memory] index_max_bytes` (default 25600) limits its bytes. A line past either cap is not
sent. When at least one line is dropped, the model receives one notice before its next call:
the notice names the scope, the file and how many lines were dropped, and asks the model to
rewrite the index shorter. It is sent once per session for each scope.

The section is computed once per process and reused byte for byte. A change to an index file
takes effect in the next session, or on resume.

There is no memory tool. A call to `memory_search` or `memory_save` is an unknown tool.

## The session switch

`/memory off` refuses the model's `write` and `patch` calls into a memory scope for the rest
of this process, with the reason `memory is off for this session`, and records the switch in
the session so a resume starts without the index section. It is not a sandbox guard: command
line tools keep whatever file access the tier allows. The section this process already built
stays frozen; the switch takes full effect on the next session. `/memory on` allows those
writes again. The switch is per session and is never written to the configuration file.
