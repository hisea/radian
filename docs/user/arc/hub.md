---
description: The hub's repository registry, its subcommands, and the `[hub]` configuration keys.
---

# hub

The hub is one process that manages every repository registered on this machine.
`radian hub` with no subcommand runs its scheduling loop; `radian hub add`, `radian hub remove` and `radian hub list` manage the registry.
The HTTP API, pairing and the request routes are not built yet; until they are, `radian hub` runs the loop alone and says so on its first line.

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

`radian hub` with no subcommand starts the scheduling loop.
It first takes `$RADIAN_HOME/hub/hub.lock` (flock, with its pid inside) for its whole life; a second `radian hub` cannot take it, exits `1`, and names the holder's pid before it reads the registry or touches any repository.
It then prints `radian hub: the HTTP listener is not built yet; the scheduling loop runs alone` and re-reads the registry every [`hub.interval_secs`](#the-hub-configuration).
SIGINT or SIGTERM stops every loop, releases every repository's `runner.lock`, then releases `hub.lock`, and exits `0`.
A change to the registry made by `radian hub add` or `remove` while the hub runs takes effect at its next read, so a new repository is scheduled at the next interval and a removed one releases its lock after its pass in flight.

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
| `0` | success, and a running hub stopped by SIGINT or SIGTERM |
| `1` | a registry error (the file does not parse or breaks a rule, the path is not an arc repository, or the name is unknown or taken), or another hub holds `hub.lock`; the line names its pid |
| `2` | a usage error: a missing argument, an unknown flag, or a `--name` outside the rule |

## The `[hub]` configuration

`[hub]` lives in the user configuration file (`~/.radian/config/config.toml`); see [`configuration`](../configuration.md#hub).
A change takes effect at the hub's next start.
The HTTP listener is not built yet, so `hub.port` is accepted and has no effect in this build.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `hub.port` | integer | `7341` | `1` to `65535` | The HTTP API's port. The API binds `127.0.0.1` only. Inert until the listener lands. |
| `hub.concurrency` | integer | `4` | `1` or more | The machine-wide cap on tasks in `doing` across every registered repository. |
| `hub.interval_secs` | integer | `30` | `5` to `3600` | Seconds between two reads of the registry and two scheduling passes of each repository. |

A value outside its range is a configuration error naming the key and the range.
An unknown key under `[hub]` is skipped and named at startup.

## The loop

Each registered repository gets one loop.
The loop takes that repository's `runner.lock` and holds it for its life, runs the same pass `radian arc run` runs, waits `hub.interval_secs`, and runs another, until SIGINT or SIGTERM.
It is the same loop for every repository: `radian arc serve` no longer exists, and `radian arc run` is still the one-shot pass.

The loops are OS threads, one per repository, not async tasks on one runtime: a pass is synchronous and blocking (it starts and waits for git and gate processes, and it sleeps between passes), so one slow pass must not delay another repository's pass or an HTTP request when the listener lands.
The hub never writes the registry.

A repository whose `runner.lock` another process holds is not scheduled.
The hub's status line names it `read-only` with the holder's pid (or `read-only (holder unknown)` when the lock file carries none), and probes the lock again every interval.
When the holder lets go, the next probe schedules the repository.

The machine-wide [`hub.concurrency`](#the-hub-configuration) cap applies on top of each repository's own [`limits.concurrency`](config.md#configtoml) and flow concurrency.
Before it starts a task, a pass reads the number of tasks in `doing` in every registered repository at that moment and starts the task only when that number is below the cap.
A repository whose store cannot be read is left out of that count, so a broken task file there cannot stop another repository's pass; that repository's own loop reports the error.
The count is read, not cached: two passes in different repositories can each see a free slot and both start one task, so the cap bounds the steady state, not a lock; the next passes see the new count.

A pass error in one repository is written to that repository's [`loop.log`](files.md), and its loop continues at the next interval; the other repositories are unaffected.
As with the old loop, three passes in a row with the same error text stop that repository's loop: it stops the gates it started the way a signal does (workers are left alone), releases its `runner.lock`, and the hub keeps running and scheduling every other repository.
Restart the hub to schedule that repository again.

The status is one line per repository, printed on start and whenever it changes:

```text
hub: 0 in doing, cap 4
radian  /home/me/Code/radian  scheduled
work-tools  /home/me/Code/work-tools  read-only (pid 4242)
```

A running hub follows a rebuilt binary as the old loop did: when a pass sees that the file at its recorded executable path changed and is stable, the hub prints `radian arc: the radian binary changed on disk; restarting the loop with it` once, in the `loop.log` of the repository whose pass saw it, finishes the passes in flight, releases every `runner.lock` and then `hub.lock`, and replaces itself with `exec` of the recorded path.
A loop that sees the same replacement after the first one stops without printing a second line.
The pid does not change, and a running gate and a running worker are left alone.
When `exec` fails, the hub prints one line, takes `hub.lock` again and goes on with the old code.

On SIGINT or SIGTERM every loop starts no new worker and no new gate.
A Ctrl-C in the terminal reaches the hub only: each worker and each gate runs in its own process group, and both treat SIGINT exactly as SIGTERM.
A running worker is left alone: it is a separate process with its own limits and record, and the next pass (or the next `radian arc run`) collects it.
A running gate is signalled and gets `[limits] cancel_grace_secs` before SIGKILL, and so is one whose record is still `started` while it holds the gate lock; the gate forwards the signal to the stage it is running, waits for it, and exits without a record; once it is gone its record becomes `interrupted`, so a gate that did not finish is started again by the next loop as unverified and counts no death, and the verdicts it already wrote are reused, unless it exited before writing any record at all: then the next pass reports it and starts no gate while its inputs are unchanged (see [the gate run](gate.md#the-gate-run)).
Each loop then prints its summary, releases its `runner.lock`, and the hub releases `hub.lock` last and exits `0`.

## Pairing

Not yet.

## Routes

Not yet.
