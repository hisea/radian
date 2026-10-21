---
description: Start radian, open a project, and ask for help.
---

# Quickstart

Run `radian` with no arguments to open the full screen interface.
Run `radian exec` to use the line interface in the same terminal.
Run `radian exec -p "your task"` to run one task and exit.
Read output from `radian exec -p` from stdout.
Read status lines from stderr.

Omit `--workspace` to use the current directory as the workspace.
Pass `--workspace <DIR>` to use another directory as the workspace.
Pass `--model <name>` and `--provider <name>` when no default model is set.
Store a key once with `radian login`.
Ask radian how to use radian in plain words.

Radian ships these built in tools: `read`, `write`, `patch`, `glob`, `grep`, `shell`, `job`, `memory_search`, `ask_user`, `skill`, `subagent`.
The set in one session depends on what the session mounts.
A headless session with no workspace mounts only a small set.
Ask for `/help` in the full screen interface to see commands.
Ask a question about radian itself to read the built in manual.
Run `radian docs` in a terminal to print the same manual.
