---
description: The built-in file tools: `read`'s text, directory and image branches with its limits and error codes, and `grep`'s arguments, defaults and caps.
---

# Tools

The model calls built-in tools for files, search and commands.
Every tool name, argument and result is visible in the transcript.
`read`, `write`, `patch`, `glob` and `grep` work inside the workspace;
`read`, `grep` and `glob` also read the read-only roots described in `permissions`.
A workspace path is a relative path, or an absolute path inside the workspace;
see `permissions` for the exact rule.
`shell` and `job` exist only when the session has a sandbox.

## `read`

`read` opens one path:

| Argument | Type | Default | What it does |
|---|---|---|---|
| `path` | string, required | — | A file, a directory, or an image. A workspace path, as `permissions` defines it; absolute or `~` for a read-only root. |
| `max_bytes` | integer, 1 to 1048576 | `65536` | Maximum bytes of text to read. |
| `offset` | integer, 1-based, negative counts from the end | `1` | First text line to read. |
| `limit` | integer, at least 1 for text and directory reads | `500` | Maximum text lines to read. For a directory it caps entries instead, up to `5000`. Ignored for an image. |

A text read prefixes each line with its file line number and `": "`.
That prefix is not file content; remove it before you reuse the text in `patch`.
A directory read lists `kind`, size and name rows, sorted by name.
`radian://docs/<page>.md` reads this manual from memory.
A path that names a directory reads its `index.md` (`radian://docs/arc/`).

### Images

An image is PNG, JPEG, GIF or WebP, judged by the file's magic number and never by its extension.
BMP and every other format are not images: `read` treats them as binary text and fails with `not_utf8`.
An image read returns two parts: a text line `Read image file [image/png] 1200×800`, and the image part.
Whether the model receives that image depends on the provider route:

- Anthropic sends the image block inside the tool result.
- OpenAI-compatible and OpenAI Responses send a tool result's text only. On those routes the model receives `[image/png image omitted]`, not the image. The image part is still in the transcript and the session file.
- An image in a **user message** (`@path`, a dropped path, a pasted clipboard image, or `exec --image`) is sent by every route: Anthropic as an image block, OpenAI-compatible as `image_url`, and OpenAI Responses as `input_image`.

When the image's long side is over 1568 px, the text line adds that the provider scales it down.
Radian never resizes or re-encodes the file itself.

The limits are constants, not settings:

| Limit | Value |
|---|---|
| Raw image bytes | 5 MiB (5242880 bytes) |
| Each image side | 8000 px |

`offset` and `limit` have no meaning for an image: `read` ignores them and returns the whole image.
When either is given, the text line adds `offset and limit ignored: an image is read whole`.
An image over either limit fails with `image_too_large`, and the message names the limit.
A read that is not valid UTF-8 fails with `not_utf8`.

When the current model's catalog entry says its input modalities have no `image`, `read` returns only the text line and adds
`[Current model does not support images. The image will be omitted from this request.]`.
No image part is sent, on any route.
A model that is not in the catalog, or whose entry has no input modalities, is treated as taking images.

### Read roots

The same path rules apply to text and images.
See `permissions` for which paths count as workspace paths.
Absolute and `~` paths read the cargo registry sources, the toolchain source trees, each skill's own directory, and this manual.
In the `full` tier they also read any other path except the deny set.
`write`, `patch` and shell working directories never leave the workspace.
See `permissions` for the tiers and the deny set.

## `grep`

`grep` searches file contents with a Rust regex and reports each match as `path:line:text`.
It locates lines; it does not read a file for you: read a range around a hit with `read` and its `offset` and `limit`.
Context lines are shown as `path-line-text`, and `--` separates non-contiguous groups.

| Argument | Type | Default | What it does |
|---|---|---|---|
| `pattern` | string, required | — | Rust regex to search for in each line. |
| `path` | string | the workspace root | A directory or file to search, in the forms `permissions` defines. Several whitespace-separated paths are searched in one call and share the caps. |
| `glob` | string | — | Search only files whose workspace-relative path matches this glob. `**` crosses directories; `*` does not. |
| `case_insensitive` | boolean | `false` | Ignore case. |
| `context` | integer, 0 or more | `0` | Context lines to show before and after each match. Default 0 is the normal case: every context line costs tokens. Values above 30 are cut to 30. |
| `max_matches` | integer, 1 to 5000 | `50` | Maximum matches to return across the whole search. A value above 5000 fails with `grep_limit_exceeded`. |
| `output` | string | `content` | `content` prints each match line; `files` prints one `<path>: N matches` line per matching file; `count` prints only the totals. |

`context` only shapes `content` lines; `files` and `count` never show them.
The per-file cap is 50 matches: a file with more ends its output with `[<path>: more than 50 matches in one file]`.
A result is capped at 16384 bytes (16 KiB), notes included.
The match caps and the byte cap apply in all three `output` modes.

Past a match cap the output ends with `[truncated: showing the first N matches]`.
Past the byte cap, context lines are dropped first to keep every match line; only if the match lines alone do not fit are whole match lines cut, in order.
The output then ends with a note whose first line is `[truncated: output hit 16384 byte limit; ...]`, naming what was dropped and how to narrow the search: a narrower `path` or `glob`, a lower `context`, or `output: "files"` first (`output: "count"` in `files` mode).
A `content` or `files` search with no matches prints ``[no matches for `<pattern>`]``; `count` prints `0 matches in 0 files`.
A file larger than 262144 bytes (256 KiB) is not searched when the search runs over a directory or a `glob`: the result ends with one line `[skipped <n> files over 262144 bytes: <up to five paths>, and <m> more]` (the `and <m> more` part only when more than five were skipped) in all three `output` modes, and the match counts exclude the skipped files.
When every file that could match was skipped, that line is the whole result.
A file named directly as `path` is always searched, whatever its size.

`.gitignore` and `.ignore` files apply from the workspace root down to the search directory, and the global gitignore is not read.
`.git`, symlinks and binary files are skipped.
The read-only roots are the ones described for `read`.
