# radian-memory

The ADR 0021 memory storage layer. It is **host-free**: the caller passes the root
directory (the host passes `~/.radian/memory/`); the crate never reads `HOME`, and Core does
not know it exists.

A record is one Markdown file. Writing the file is remembering; deleting it is forgetting.
There is no revision, lifecycle field, event log or search layer.

## Layout on disk

```
<root>/projects.jsonl                    the project registry (identity key → ProjectId)
<root>/global/<name>.md                  a Global record
<root>/global/MEMORY.md                  the Global index
<root>/projects/<ProjectId>/<name>.md    a Project record
<root>/projects/<ProjectId>/MEMORY.md    the Project index
<root>/<scope>/log.jsonl                 an old log, read once by the migration
```

Directories are `0700` and files `0600`. Every record and index write goes through a
temporary file in the same directory, an atomic rename over the path, and then a directory
fsync.

## A record

A record is YAML frontmatter between two `---` lines, then the body. The body keeps its bytes
exactly: a trailing newline and a line that spells `---` included. Every frontmatter scalar is
one line: plain, single-quoted or double-quoted. The writer leaves a value plain when that is
unambiguous and double-quotes anything else (a value with `: `, a leading or trailing space, a
quote at its start, or a control character), so a hand-written quoted value and a written
record read back the same. The frontmatter is a block-form subset of YAML: `provenance` is
written as an indented block (`provenance:` then `  session_id: ...`), and a flow mapping such
as `provenance: {session_id: s}` is refused with a must-be-a-map error. A quoted `last_used`
is a string, so `last_used: "null"` is refused as a non-RFC-3339 timestamp rather than read
as never-read; only plain `null` or `~` means never read.

The fields are `name` (the file stem; `[a-z0-9-]`, 1 to 64 characters), `description` (one
line), `type` (the closed set `RECORD_TYPES`: `user`, `feedback`, `project`, `reference`),
`source` (the closed set `RECORD_SOURCES`: `model`, `user`, `import`; `add` writes `user`, the
migration writes `import`, and the model sets `model` on its own writes as its prompt tells it),
`created` and `modified` (RFC 3339 UTC; `created` is kept across a write and `modified` is set
on every write), `last_used` (RFC 3339 UTC or absent, which means never read), and an optional
`provenance` map with `session_id` and `turn_id`.

`write` refuses a name outside the rule before it touches a file, and a record's `source` is
fixed at its first write: replacing a file keeps its existing `source` and ignores the
caller's new value, so the caller's `source` is judged only when the file is new. `add` and
the migration set `source` themselves; the model writes the other files directly and sets
`source: model` as its prompt tells it, so the host does not supply that value. `touch_last_used` rewrites only the `last_used` line, or inserts one after
`modified`; every other byte stays. `rename` rewrites only the `name` line; `modified`,
`created`, `source` and the body stay byte for byte. The two closed sets are the slices the
parser, the writer and the docs drift test read.

## The index

Each scope has one index, `MEMORY.md`. Its form is `- [title](file) — description`, one line
per record. The store treats it as plain text the host maintains; `index_lines` parses that
form and reports every line that is not in it with its 1-based number.

## Listing and reading

`list` returns every readable record's frontmatter sorted by `name`, then one
`ListEntry::Unreadable { path, reason }` for each file whose frontmatter does not parse, whose
`type` or `source` is outside its set, or whose `name` is not its file stem. The other records
still list. `read` of such a file is an error naming the path and the line.

## Migration

`migrate_log(scope_dir)` converts one scope's old `log.jsonl` once. When the log exists and
`log.jsonl.migrated` does not, it replays the log with a small private JSONL reader and writes
every Active record as a file: the name is the record's key when it obeys the name rule, else
a slug of the first words of the content made unique with `-2`, `-3`; the description is the
first content line cut at 120 characters; `preference` becomes `user`, `decision` and `fact`
become `project`, `lesson` becomes `feedback`; `User` becomes `user`, `Agent` becomes `model`,
`Import` becomes `import`; `created` and `modified` come from the record, and `provenance`
keeps `session_id`/`turn_id`. Archived and Forgotten records are dropped. It writes the index,
then renames the log to `log.jsonl.migrated`. A second call finds the marker and does nothing.
A crash before the rename leaves the log, and the next call writes the same bytes again: the
writes are idempotent by content, not by modification time.

## The registry

`ProjectRegistry::{register, resolve, relink, projects, identity_keys}` reads and appends
`projects.jsonl`, an append-only file with a single-writer `flock`, one fsync per append and a
torn tail dropped before the next append. `register` is stable per identity key; `relink`
attaches a key to an existing project.

## Public API

- `FileStore::{write, read, read_index, write_index, list, delete, rename, exists,
  touch_last_used, scope_dir, record_path, index_path}` — the record store and its indexes.
- `ProjectRegistry::{register, resolve, relink, projects, identity_keys}` — the registry.
- `migrate_log` — the one-time old-log migration.
- `validate_record_name` — the name rule's message, for a command that validates before it
  writes.
- The closed sets `RECORD_TYPES` and `RECORD_SOURCES`, and `NAME_MAX`/`DESCRIPTION_MAX`.

`MemoryStore`, `MemoryEvent`, `SearchQuery`, `search`, `export` and `parse` are gone.

## Not here

The host wiring (ADR 0021 D6–D8): the commands, the permission rules, configuration, index
injection, the session `MemoryInjected` event, and sandbox read denial. This crate is storage
only.
