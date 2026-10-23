---
description: The built-in file tools, with `read`'s text, directory and image branches, its limits and its error codes.
---

# Tools

The model calls built-in tools for files, search and commands.
Every tool name, argument and result is visible in the transcript.
`read`, `write`, `patch`, `glob` and `grep` work inside the workspace;
`read`, `grep` and `glob` also read the read-only roots described in `permissions`.
`shell` and `job` exist only when the session has a sandbox.

## `read`

`read` opens one path:

| Argument | Type | Default | What it does |
|---|---|---|---|
| `path` | string, required | — | A file, a directory, or an image. Workspace-relative, or absolute or `~` for a read-only root. |
| `max_bytes` | integer, 1 to 1048576 | `65536` | Maximum bytes of text to read. |
| `offset` | integer, 1-based, negative counts from the end | `1` | First text line to read. |
| `limit` | integer, at least 1 | `500` | Maximum text lines to read. For a directory it caps entries instead, up to `5000`. |

A text read prefixes each line with its file line number and `": "`.
That prefix is not file content; remove it before you reuse the text in `patch`.
A directory read lists `kind`, size and name rows, sorted by name.
`radian://docs/<page>.md` reads this manual from memory.

### Images

An image is PNG, JPEG, GIF or WebP, judged by the file's magic number and never by its extension.
BMP and every other format are not images: `read` treats them as binary text and fails with `not_utf8`.
An image read returns two parts: a text line `Read image file [image/png] 1200×800`, and the image part.
Whether the model receives that image depends on the provider route:

- Anthropic sends the image block inside the tool result.
- OpenAI-compatible and OpenAI Responses send a tool result's text only. On those routes the model receives `[image/png image omitted]`, not the image. The image part is still in the transcript and the session file.
- An image in a **user message** (`@path`, a dropped path, or `exec --image`) is sent by every route: Anthropic as an image block, OpenAI-compatible as `image_url`, and OpenAI Responses as `input_image`.

When the image's long side is over 1568 px, the text line adds that the provider scales it down.
Radian never resizes or re-encodes the file itself.

The limits are constants, not settings:

| Limit | Value |
|---|---|
| Raw image bytes | 5 MiB (5242880 bytes) |
| Each image side | 8000 px |

`offset` or `limit` on an image fails with `image_offset`.
An image over either limit fails with `image_too_large`, and the message names the limit.
A read that is not valid UTF-8 fails with `not_utf8`.

When the current model's catalog entry says its input modalities have no `image`, `read` returns only the text line and adds
`[Current model does not support images. The image will be omitted from this request.]`.
No image part is sent, on any route.
A model that is not in the catalog, or whose entry has no input modalities, is treated as taking images.

### Read roots

The same path rules apply to text and images.
Relative paths stay in the workspace.
Absolute and `~` paths read the cargo registry sources, the toolchain source trees, each skill's own directory, and this manual.
In the `full` tier they also read any other path except the deny set.
`write`, `patch` and shell working directories never leave the workspace.
See `permissions` for the tiers and the deny set.
