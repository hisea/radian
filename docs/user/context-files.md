---
description: Which project direction files load, from where, in which order, and their limits.
---

# Context files

The model sees your project directions in every request. Radian reads them
once at session start, outside the sandbox, and prepends them to the system
prompt. They never enter the journal; only the loaded paths and sizes are
recorded.

## Which files load, and in which order

Radian tries five names in each directory, in this order. The first existing
regular file wins.

1. `AGENTS.override.md`
2. `AGENTS.md`
3. `AGENTS.MD`
4. `CLAUDE.md`
5. `CLAUDE.MD`

It looks in the global directory first (`~/.radian/`), then from the workspace
up to the repository root, outer directories first. A workspace outside a
repository reads only the workspace directory itself. The same real file is
read once: a symlink that resolves to an already-read file is skipped. A
file that cannot be read warns once on the startup line, and the session
opens without it.

Write always-on directions here: build commands, test commands, style rules,
repository layout. Never write secrets here: the text goes to the model on
every request.

## Size limits

One file holds at most 64 KiB, counted after `@path` imports expand. Longer
text is cut with a `[truncated: ...]` marker. All files together hold at most
128 KiB. When the total overflows, the outermost whole files are dropped
first: the global file, then the repository root, working inward. The
workspace file is never the one dropped, and a warning names each dropped
file.

## `@path` imports

A `@path` token inside a context file pulls another file's text into it.
The token starts at a line start or after whitespace, and runs to the next
whitespace. It expands relative to the importing file's directory.

```md
# Build

@docs/build.md
```

Imports expand before the size limits apply. Five rules keep imports safe:

- Only `@` outside inline code and fenced code blocks expands. `@` inside
  backticks or fenced blocks stays literal.
- The target must canonicalize inside a boundary: the repository root for
  workspace files, the workspace itself outside a repository, the radian home
  for the global file. Anything outside stays literal.
- The leaf opens with no symlink following, so a file in the project cannot
  point the reader outside the boundary.
- At most four levels expand. A fifth level, and a cycle, stay literal.
- A missing or unreadable target stays literal and is reported.

The startup line shows imports next to their file, and names the tokens that
stayed literal.

## How to check what loaded

The startup screen shows one summary line. It lists each loaded file with
its size, for example `AGENTS.md (2.1 KB)`. An expanded import reads
`CLAUDE.md (2.1 KB, +docs/a.md 3.4 KB)`. A token that stayed literal reads
`, not read: ../x.md`. A truncated file reads `, truncated`. No file reads
`none`.

The session record keeps the loaded paths, sizes and imports. It never keeps
the text. To confirm the exact prompt block, read the file on disk: the
model sees the file's text with imports expanded, wrapped in one labeled
block per file, in the same order the startup line lists.
