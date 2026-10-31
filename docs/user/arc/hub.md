---
description: The hub's repository registry, its subcommands, and the `[hub]` configuration keys.
---

# hub

The hub is one process that manages every repository registered on this machine.
This build has the registry only: `radian hub add`, `radian hub remove` and `radian hub list`.
The scheduling loop, pairing and the HTTP routes are not built yet.

## The registry file

`$RADIAN_HOME/hub/repos.toml` (default `~/.radian/hub/repos.toml`) lists the repositories.
The hub directory is `0700` and the file is `0600`.
A missing file is an empty registry.

```toml
[[repos]]
name = "radian"
path = "/home/me/Code/radian"

[[repos]]
name = "work-tools"
path = "/home/me/Code/work-tools"
```

| Key | Type | Rule |
|---|---|---|
| `name` | string | 1 to 48 characters from `a-z`, `0-9` and `-`; unique. |
| `path` | string | an absolute, canonical path to a directory with a `.radian/arc/`; unique. |

Entries are listed in file order.
Every write goes through a temporary file and an atomic rename, so a process that re-reads the file sees either the old document or the new one, never a partial file.
A file that does not parse, a name outside its rule, a path that is not an arc repository, and a duplicate name or path are errors that name the file and the line, or the path.

## `radian hub`

`radian hub` with no subcommand prints `the hub loop and the API are not built yet` and exits `3`.

### `radian hub add <PATH> [--name <NAME>]`

Registers the repository at `PATH`: a directory that has a `.radian/arc/`.
`PATH` is canonicalized first, and the registry stores the canonical path.
Without `--name`, the name is the directory's base name.
A name outside the rule, a duplicate name, a duplicate path (a symlink to an already registered repository included), and a path that is not an arc repository are refused, and the file does not change.
It prints `added <name>  <path>`.

| Flag | Value | Default | What it does |
|---|---|---|---|
| `--name` | `NAME` | the directory's base name | The name to register. 1 to 48 characters from `a-z`, `0-9` and `-`. |

### `radian hub remove <NAME>`

Removes the repository named `NAME` and prints `removed <name>`.
An unknown name is refused and nothing changes.

### `radian hub list [--json]`

Prints one line per repository, in file order: `name  path` (two spaces).
With `--json`, it prints one JSON array of `{ "name": ..., "path": ... }`.

| Flag | Value | Default | What it does |
|---|---|---|---|
| `--json` | a flag | off | Print the JSON array instead of one line per repository. |

### Exit codes

| Code | Meaning |
|---|---|
| `0` | success |
| `1` | a registry error: the file does not parse or breaks a rule, the path is not an arc repository, or the name is unknown or taken |
| `2` | a usage error: a missing argument, an unknown flag, or a `--name` outside the rule |
| `3` | `radian hub` with no subcommand: the loop and the API are not built yet |

## The `[hub]` configuration

`[hub]` lives in the user configuration file (`~/.radian/config/config.toml`); see [`configuration`](../configuration.md#hub).
The reader validates these keys now; the hub that uses them is not built yet, so they have no effect in this build.
When it runs, a change takes effect at the hub's next start.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `hub.port` | integer | `7341` | `1` to `65535` | The HTTP API's port. The API binds `127.0.0.1` only. |
| `hub.concurrency` | integer | `4` | `1` or more | Tasks in `doing` across every registered repository. |
| `hub.interval_secs` | integer | `30` | `5` to `3600` | Seconds between two scheduling passes. |

A value outside its range is a configuration error naming the key and the range.
An unknown key under `[hub]` is skipped and named at startup.

## The loop

Not yet.

## Pairing

Not yet.

## Routes

Not yet.
