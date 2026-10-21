---
description: Where radian reads configuration and what each file controls.
---

# Configuration

Radian reads one user configuration file.
The file path is `--config <FILE>` when passed.
The file path is `$RADIAN_CONFIG` when set.
The file path is `~/.radian/config/config.toml` otherwise.
Set `$RADIAN_HOME` to move the whole home directory.
The file uses TOML.
The file must set `schema_version = 1`.
Radian ignores unknown keys.
Radian never writes this file except when you run an explicit write command.

Set `[model]` `provider` to choose the wire format.
Set `[model]` `base_url` to choose the endpoint.
Set `[model]` `model` to choose the default model name.
Set `[model]` `retries` to bound internal retries on transient errors.
Set `[model.smol]` `provider` and `model` to choose the cheap route.
Only the file search tool named `explore` reads the cheap route.
Set `[permissions]` `profile` to `workspace` or `full`.
Set `[permissions]` `approval` to `on-ask`, `always`, `never`, or `auto`.
Command line flags always win over the file.
Set `[tui]` `theme` to a theme name.
Set `[tui]` `mouse` to true or false.
Set `[tui]` `input_style` to `lines` or `box`.
Set `[tui.display]` `mermaid` to true or false.
Set `[tui.footer]` to choose the footer layout.
Set `[tui.widgets]` to add rows above or below the input box.
Set `[[mcp.servers]]` to add a local command server.
Set `[mcp.mcpServers.<name>]` to add a shared form server.
Set `[skills]` `builtin` to false to turn off built in skills.
Set `[skills]` `disabled` to turn off single skills by name.
Set `[skills]` `extra_paths` to add more skill directories.
Set `[memory.injection]` `additions` to control follow up memory notes.
Set `[shell]` `auto_background_secs` to control when a foreground command moves to the background.
Set `[control]` `enabled` to false to turn off the local control socket.
Set `[integrations]` `herdr` to false to turn off status reports.
Set `[compaction]` `thinking` to choose the summary call level.
Set `[sandbox]` `allow_unix_sockets` to allow named sockets.
Set `[sandbox]` `allow_env` to allow named variables into sandboxed children.

A project may hold `<workspace>/.radian/config.toml`.
That file can only narrow the permission tier.
A value of `full` there warns and has no effect.
Trust the project before its skills and local servers load.
