---
description: The arc commands: an overview, the subcommand list, and the exit codes.
---

# arc

arc is a plan and task orchestrator built into the same `radian` binary.
It keeps a plan and its tasks as files in the repository, runs each task as a separate headless `radian` worker, and merges only what passes a fixed gate.

This build has the storage layer, the subcommands that read and write it: `init`, `add`, `list`, `show`, `adopt`, `confirm`, `auto`, `priority`, `pause`, `start`, `reverify`, `cancel`, `reset-budget`, `merge`, `reject`, `run`, and `serve`.
It also has `radian arc mcp`, the MCP server a lead writes plans and tasks through, the attempt runner behind `start`, and the gate and merge behind `start`, `merge` and the loop.
`radian arc serve` is the scheduling loop and `radian arc run` is one pass of it; a bare `radian arc` opens the TUI, where you see the plans and tasks and act on them (see [`arc/tui`](tui.md)).

The commands are in [`plans and tasks`](planning.md), [`running`](running.md), [`the owner's commands`](owner.md) and [`hooks`](hooks.md); the merge is in [`integration`](integration.md), the loop in [`loop`](loop.md), the gate in [`gate`](gate.md), [`gate-review`](gate-review.md) and [`gate-failures`](gate-failures.md), the MCP server in [`mcp`](mcp.md), the TUI in [`tui`](tui.md), the flows in [`flows`](flows.md), the worker in [`worker`](worker.md), and the files in [`files`](files.md) and [`config`](config.md).

## `radian arc`

`radian arc` with no subcommand opens the TUI; its pages and keys are in [`arc/tui`](tui.md).
Without setup, `radian arc` and every subcommand except `init` refuse with a line that names `radian arc init` and exit `3`; without a terminal, `radian arc` exits `2`.

## The subcommands

`radian arc init`, `radian arc add plan` and `radian arc add task`, `radian arc list`, `radian arc show` and `radian arc adopt` are in [plans and tasks](planning.md); `radian arc start`, `radian arc cancel`, `radian arc reset-budget` and `radian arc reverify` in [running](running.md); `radian arc confirm`, `radian arc auto`, `radian arc priority`, `radian arc pause`, `radian arc merge` and `radian arc reject` in [the owner's commands](owner.md); `radian arc run` and `radian arc serve` in [the loop](loop.md); `radian arc mcp` in [mcp](mcp.md); and the `[hooks]` table in [hooks](hooks.md#hooks).

## `radian arc run`

`run` is one pass of the scheduling loop: what a pass does, its output, its exit codes and `loop.log` are in [`arc/loop`](loop.md#radian-arc-run).

## `radian arc serve`

`serve` is the scheduling loop: `--interval`, its output, the signal handling and the repeated-error stop are in [`arc/loop`](loop.md#radian-arc-serve).

## `radian arc mcp [--claude-experimental]`

`radian arc mcp` is the stdio MCP server the lead writes plans and tasks through. Registering it, its tools and its tool errors are in [`arc/mcp`](mcp.md).

| Flag | Value | Default | What it does |
|---|---|---|---|
| `--claude-experimental` | — | off | Push task transitions to a Claude Code channel. The details are in [`arc/mcp`](mcp.md#the-claude-experimental-flag). |

## Exit codes

Every subcommand uses these codes.

| Code | Meaning |
|---|---|
| `0` | success |
| `1` | an error |
| `2` | a usage error |
| `3` | the state does not allow the operation (this includes a file changed outside arc, and arc not being set up) |
| `4` | a lock is held by another process |

Code `2` covers a bad flag or an argument value outside a closed set; a slug, an id, or a stage name that the rules refuse is also `2`.
A closed output pipe (`radian arc list | head`) is a normal stop: exit `0`.

## The TUI

The TUI never holds `runner.lock` and runs no pass; when it opens it starts `radian arc serve` if no loop holds the lock, and quitting it stops nothing.
The header and ticker, the log view, the plans, board, task and merge pages, every mark and key, and the owner's actions are in [`arc/tui`](tui.md).

## Not built yet

The gate stages `mutation` and `ci` are not built: verification refuses a profile or task that lists one, and the task stays in `review.verifying`.

## See also

- `cli` for every other `radian` command.
- `environment` for the files radian reads, including this one.
- `arc/worker` for the worker itself: its session, brief and limits, and another agent as the worker.
