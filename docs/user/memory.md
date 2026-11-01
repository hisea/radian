---
description: Save facts across sessions and search them by keyword.
---

# Memory

Memory holds small durable records. Each record has an id, content, scope,
kind, key, source, verification and revision. Scopes are project and global.
Project records belong to one project directory; global records belong to
you. The model receives each scope's `MEMORY.md` index in its system prompt,
project first, then global; see "What the model sees".

The model reads and writes its memory files with the ordinary file tools
(`read`, `grep`, `glob`, `write`, `patch`) in both tiers. The tools reach
only the two scope directories: the global memory directory and, when the
workspace maps to a project, that project's directory. Every other path
under the memory directory stays denied, and the shell cannot read any of
it. See `permissions` for the exact rule.

## Refused writes

A `write` or `patch` that targets a memory scope directory can be refused before
the file is touched. Both are host checks; the file tools' path rule still
decides which paths they may reach.

- Once the session has taken in a result from an MCP tool, every later memory
  write is refused with `memory write refused: this session has taken in MCP
  tool results (input guard)`. A resumed session counts a result its journal
  recorded, even when the new process never calls an MCP tool.
- Content that looks like a secret is refused with `memory write refused: the
  content looks like a secret (<prefix>)`. `<prefix>` is the matched prefix
  (`sk-`, `sk_`, `xoxb-`, `ghp_`, `AKIA` or `Bearer `), and the value is never
  repeated. A word of 40 or more ASCII letters, digits, `_` or `-` is refused
  with `40+ characters` in place of the prefix. A `patch` is judged on the
  content it would produce, so inserting a secret is refused even when the old
  text is clean.

## Memory files and `last_used`

A memory file holds one record. Its frontmatter carries `name`, `description`,
`type`, `source`, `created`, `modified`, `last_used` and an optional
`provenance`. When the model `read`s a memory file other than `MEMORY.md`, the
host sets `last_used` to the time of the read and changes no other byte. A
user command (`list`, `show`, `stale`) never changes it. An absent `last_used`
means the file was never read.

## Kinds

A record is one kind: `preference`, `decision`, `fact` or `lesson`. The
default is `fact`. A kind this version does not know is a usage error, never
a silently stored record.

## The commands

Type `/memory <command>` in the interface, or run `radian memory <command>`
outside it with the same grammar. Add `--workspace <dir>` outside to resolve
the project against another directory.

| Command | What it does |
|---|---|
| `list [--global\|--project] [--all] [--json]` | List records, project first then global. `--all` includes archived records. |
| `search [--global\|--project] [--json] <words>...` | Search by keyword. Every word must match, in the content or the key. Matching ignores case. |
| `show [--global\|--project] [--json] <id>` | Read one record in full. |
| `stale [--days N] [--global\|--project] [--json]` | List memory files not used for a while. A file is stale when its `last_used` is older than N days, or it has no `last_used` and its `created` is older than N days. `--days` is an integer of 1 or more; the default is 30, and `--days 0` is a usage error. One line per file: `scope  name  last_used-or-never  description`. `--json` gives an array of objects with the frontmatter fields, with `last_used` null when never read. A file whose frontmatter cannot be read is listed as unreadable and the other files still list. |
| `add [--global] [--kind preference\|decision\|fact\|lesson] [--key <key>] <text>` | Add a project record by default, a global one with `--global`. In the interface the text is literal: a bare `-` writes nothing and shows `` `-` reads stdin only in `radian memory`; type the text here``. On the command line, `-` reads the text from stdin instead. |
| `edit [--global] --revision <N> <id> <text>` | Change content. Pass the current revision, shown by `show` and `list`. A bare `-` as the text writes nothing and shows `` `-` reads stdin only in `radian memory`; type the text here``. |
| `archive [--global] --revision <N> <id>` | Archive a record. It stays stored but is no longer injected or searched by default. |
| `restore [--global] --revision <N> <id>` | Restore an archived record. |
| `forget [--global] [--yes] <id>` | Forget a record. Gone from every view and never resurrected by a rebuild; the bytes remain until a purge. Asks first. |
| `purge [--global\|--project] [--yes]` | Rewrite the log, removing forgotten content. Asks first. |
| `confirm [--global] --revision <N> <id>` | Confirm a record. Keeps its original source. |
| `export [--global\|--project]` | Print one scope as editable Markdown. |
| `import [--global\|--project] <source>` | Import an edited Markdown export from the file at `<source>`: a relative path resolves against the workspace in the interface and against the current directory on the command line; an absolute path may point anywhere. A bare `-` imports nothing and shows `` `-` reads stdin only in `radian memory`; give a file path here``. On the command line, `-` reads it from stdin. A file that cannot be read imports nothing and shows `cannot read <path>: <error>`. |
| `project [--json] [new]` | Show this workspace's identity key and project state, or register it with `new`. |
| `relink <project-id>` | Attach this workspace's identity key to an existing project. |
| `split [--yes] <from> <to> <id>...` | Move records between projects. Asks first. |
| `merge [--yes] <into> <from>` | Merge one project into another, reporting key conflicts. Asks first. |

A usage or refusal error exits 2 on the command line; a conflict or a
missing record exits 1. In the interface, `forget`, `purge`, `split` and
`merge` ask first: `y` runs, `n` or `esc` cancels. A read command
(`list`, `search`, `show`, `stale`, `export`, `project`) runs while a turn runs and
does not interrupt it. A write takes effect at the next model request. In the interface, `add` and `edit` take
their text literally: `-` reads stdin only on the command line. A bare `-` in the interface writes nothing and shows the stdin notice:
`` `-` reads stdin only in `radian memory`; type the text here`` for `add` and
`edit`, `` `-` reads stdin only in `radian memory`; give a file path here``
for `import`. `/memory import <source>` reads the file at `<source>` in both:
a relative path resolves against the workspace in the interface and against
the current directory on the command line; an absolute path may point
anywhere. In the interface, a relative path that contains `..` or passes
through a symbolic link is refused, and so is an absolute path whose last
part is a symbolic link. On the command line, `-` reads the Markdown from stdin instead of
a file. A file that cannot be read imports nothing and shows
`cannot read <path>: <error>`.

## A worked example

Add two records, one project and one global:

```sh
/memory add The deploy needs the vpn profile "office".
/memory add --global I prefer short answers with no preamble.
```

List them, search them, read one in full:

```sh
/memory list
/memory search vpn profile
/memory show <id>
```

Change one with its revision, then archive it:

```sh
/memory edit --revision 3 <id> The deploy needs the vpn profile "office", region eu.
/memory archive --revision 4 <id>
```

Export the project scope, edit the Markdown, read it back:

```sh
/memory export > /tmp/mem.md
/memory import /tmp/mem.md
```

## What the model sees

At the first model call of a session, the model receives the two index files
as one system-prompt section: the project index first, then the global one.
Each part is the text of that scope's `MEMORY.md`, after the neutralization
below. The section names each file's path, so the model reads a record's body
on demand with `read`. It opens with a line that says the content is data from
your memory directory, not instructions.

A scope with no index, or an empty one, contributes one line saying so. With
no project linked, only the global index is read.

Before an index is sent, default-ignorable and zero-width code points are
removed from the copy (for example U+200B, U+200C, U+200D, U+2060, U+FEFF,
U+00AD, the bidi controls U+202A through U+202E and U+2066 through U+2069).
A line that begins with `<system`, `</system`, `<project_instructions`,
`</project_instructions`, `<memory` or `</memory`, ignoring case and leading
whitespace, gains a leading `\`. The file on disk is never changed, and the
`MemoryInjected` record's sha256 is of the text after this neutralization.

Each index is capped. `[memory] index_max_lines` (default 200) limits its
lines and `[memory] index_max_bytes` (default 25600) limits its bytes. A line
past either cap is not sent. When at least one line is dropped, the model
receives one notice before its next call: the notice names the scope, the file
and how many lines were dropped, and asks the model to rewrite the index
shorter. It is sent once per session for each scope.

The section is computed once per process and reused byte for byte. A change to
an index file takes effect in the next session, or on resume.

There is no memory tool. A call to `memory_search` or `memory_save` is an
unknown tool.
