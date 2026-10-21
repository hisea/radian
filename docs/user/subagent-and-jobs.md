---
description: Run helper sessions and background shell jobs.
---

# Helpers and jobs

Use the `subagent` tool to run one helper session.
The call blocks until the helper finishes.
The call returns the last assistant text.
Long text is cut at 4000 characters and marked.
The result names the helper session id.
The helper starts with a blank history.
The first input is the `task` text.

Set `agent` to `explore` for read only work.
The `explore` helper mounts only `read`, `glob`, and `grep`.
Set `agent` to `general` for full work.
The `general` helper mounts the parent tools without `subagent`.
Omit `agent` to run `general`.
A helper never mounts `subagent`.
Helpers can run in parallel from one reply.
A parent in automatic mode approves helper questions at once.
A parent outside automatic mode denies helper questions.
The denial is noted in the helper result.
The helper inherits the parent tier and can only narrow it.
Set `[model.smol]` to give `explore` a cheaper model.
Omit it to let `explore` follow the parent route.
A half set cheap route warns and is ignored.

Use the `shell` tool to run commands.
Use the `job` tool to manage background commands.
A foreground command past `[shell] auto_background_secs` moves to the background.
The default threshold is 60 seconds.
Set it to 0 to turn the move off.
The call returns at once with a job id.
Use `job` with `op: read` to read output.
Use `job` with `op: wait` to wait for completion.
Use `job` with `op: kill` to stop the process group.
The same registry holds retained foreground output.
A truncated foreground result stays readable through its id.
Old entries leave when the registry fills.

Type `/jobs` to open the jobs page.
Type `/jobs kill <id>` to stop one job.
Type `/jobs kill all` to stop every job.
Run `!<command>` to run a command in the tier shell.
Run `!!<command>` to keep its output out of model context.
Ask the model through `ask_user` when you own the trade off.
The question tool only appears when a human can answer.
