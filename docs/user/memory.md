---
description: Save facts across sessions and search them by keyword.
---

# Memory

Memory holds small durable records. Each record has an id, content, scope,
kind, key, source, verification and revision. Scopes are project and global.
Project records belong to one project directory; global records belong to
you. Every model request carries the matching records, project first, then
global.

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
(`list`, `search`, `show`, `export`, `project`) runs while a turn runs and
does not interrupt it. A write takes effect at the next model request. In the interface, `add` and `edit` take
their text literally: `-` reads stdin only on the command line. A bare `-` in the interface writes nothing and shows the stdin notice:
`` `-` reads stdin only in `radian memory`; type the text here`` for `add` and
`edit`, `` `-` reads stdin only in `radian memory`; give a file path here``
for `import`. `/memory import <source>` reads the file at `<source>` in both:
a relative path resolves against the workspace in the interface and against
the current directory on the command line; an absolute path may point
anywhere. On the command line, `-` reads the Markdown from stdin instead of
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

One record prints on two lines: the first line carries `id`, `revision`,
`scope`, `kind`, `key`, `source` and `verification`, and the content follows.
The model uses the `memory_search` tool. It passes `query` to search by
keyword or `ids` to read records in full, never both. `scope` is one of
`project`, `global` or `all`; the default `all` searches project first,
then global. `limit` caps the count. New matches in later turns arrive as a system message.

Results arrive ordered by scope (project first), trust (your records before
the model's), match, recency and id. A low-trust note marks model-written
content the tool returns, so the model treats your records as the ones to
trust.

Set `[memory.injection] additions = false` to turn those follow-up notes
off. The fixed block of records in the prompt stays. `[memory.injection]
addition_budget` caps one turn's addition at 500 tokens by default.
