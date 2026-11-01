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

The model writes these files with the ordinary `write` and `patch` tools. In both tiers those
tools reach only the global directory and, when the workspace maps to a project, that
project's directory. Every other path under the memory directory stays denied, and `shell`
cannot read or write any of it. See `permissions`.

## Frontmatter

A record starts with `---`, then one field per line, then `---`, then the body.

| Field | Type | Values and rules |
|---|---|---|
| `name` | string | The file stem: `a-z`, `0-9` and `-`, 1 to 64 characters. It must equal the file stem. |
| `description` | string | One line. The index shows it. |
| `type` | closed set | `user`, `feedback`, `project`, `reference`. `feedback` and `project` records carry `**Why:**` and `**How to apply:**` lines in the body. |
| `source` | closed set | `model`, `user`, `import`. The host sets it at the first write and keeps it afterwards; the model never supplies it. |
| `created` | RFC 3339 UTC | Set once. |
| `modified` | RFC 3339 UTC | Set on every write. |
| `last_used` | RFC 3339 UTC or absent | Optional. Absent means the record was never read. This build does not change it; a hand edit may set it. |
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
| `add [--global] [--type user\|feedback\|project\|reference] <name>` | Create `<name>.md` with an empty body and `source: user`, `created` and `modified`; append its index line; open it in the editor. `--type` defaults to `user`. Project scope by default. A name outside the rule, or one that already exists, is a usage error and writes nothing. |
| `edit [--global] <name>` | Open an existing file in the editor. A missing file is an error. |
| `forget [--global] [--yes] <name>` | Delete the file and its index line. Asks first; `--yes` skips the question. Off a terminal without `--yes` it refuses and keeps the file. |
| `open [--global\|--project]` | Print the scope directory. In the interface, open that directory with the editor or the platform opener. |
| `project [--json] [new]` | Show this workspace's identity key and project state, or register the workspace with `new`. |
| `relink <project-id>` | Attach this workspace's identity key to an existing project. |
| `split [--yes] <from> <to> <name>...` | Move each named file and its index line from one project to another. A name already present in the target is reported and left in place; a missing name is reported. Asks first. |
| `merge [--yes] <into> <from>` | Move every file from one project into another. When both hold the same name, the file from `from` is renamed `<name>-2` (then `-3`, …), its `name` line is rewritten, and the collision is reported. Asks first. |

`forget`, `split` and `merge` are the only commands that ask for confirmation. `list`,
`show`, `project` and `open` are reads: they run while a turn runs and do not interrupt it.

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

`/memory off` refuses every memory write for the rest of this process, with the reason
`memory is off for this session`, and records the switch in the session so a resume starts
without the index section. The section this process already built stays frozen; the switch
takes full effect on the next session. `/memory on` allows writes again. The switch is per
session and is never written to the configuration file.
