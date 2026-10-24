---
description: What radian is and where every manual page lives.
---

# Radian manual

Radian is a coding agent in one binary.
You drive it from a full-screen terminal interface or from one-shot shell commands.
Every session is stored as one file, so you can resume, fork, and export your work.

You are here: this page maps the manual.
Each line names one page and what it covers.

## Start

- `quickstart`: install radian, connect a model, and finish your first task.
- `providers`: store keys, choose a provider and model, and control request options.

## Use

- `commands`: every slash command and `!` shell escape in the full-screen interface.
- `arc`: plan and task orchestration: every subcommand, the lead's MCP server, and the exit codes.
- `arc-gate`: the gate: verification stages, the review stage, failures, retries, the budget, and reconcile.
- `arc-loop`: the scheduling loop: one pass, `loop.log`, `serve`, and its signals.
- `arc-files`: the files arc keeps: `config.toml`, plan and task files, and the `.radian/arc/` layout.
- `arc-mcp`: the lead's MCP server: how a lead registers it, its six tools, and its tool errors.
- `arc-tui`: the TUI: the plans, task and merge pages, every mark and key, and the owner's actions.
- `keybindings`: every key in the full-screen interface, by area, and how to rebind it.
- `sessions`: resume, list, search, fork, export, and attach to sessions.
- `tools`: the built-in file tools, `read`'s text, directory and image branches, its limits and error codes.
- `subagent-and-jobs`: helper sessions and background shell jobs.
- `permissions`: permission tiers, trust, approval, and sandbox behavior.
- `troubleshooting`: fix setup, network, and session problems.

## Configure

- `configuration`: every key the configuration file accepts.
- `environment`: every environment variable radian reads, and the files that change behavior.
- `footer`: every footer item, its parameters and placeholders, and the preset contents.
- `themes`: switch color themes and import themes from files.

## Extend

- `extensions`: which extension mechanism fits which need, where each lives, and a first example.
- `context-files`: which project instruction files (`AGENTS.md` and others) load, from where, in which order, and their limits.
- `hooks`: run your own commands when a tool runs or the model stops.
- `mcp`: add MCP tool servers that run as local processes over standard input and output.
- `skills`: find reusable prompt packages, read them, and run them.
- `memory`: save facts across sessions and search them by keyword.

## Reference

- `cli`: every command, subcommand, flag, and exit code.
- `pi-migration`: draft a Radian configuration from a Pi agent directory.
