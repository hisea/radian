---
description: Add local model servers over standard input and output.
---

# MCP

MCP adds tools from local servers.
Each server runs as a child process.
Each server speaks over standard input and output.
Radian never sandboxes the server process itself.
Tools from servers ask before running under the `workspace` tier.

Pass `--mcp-config <FILE>` to start servers.
The file holds JSON with a `servers` array.
Each entry holds an `id`, a `command`, and optional `args`.
Repeat runs merge the file with the config file.
An id in both places uses the command line entry.
The merge prints one warning.

Set `[[mcp.servers]]` in the config file for the same shape.
Set `id`, `command`, `args`, and `inherit_env` per entry.
Set `[mcp.mcpServers.<name>]` for the shared form.
The shared form uses `command` plus `args` or a `url`.
Remote entries warn and never start.
Shared entries inherit `HOME` by default.
Set `inherit_env` to change the allowed names.

Type `/mcp` to open the MCP panel page.
The page lists servers and their tools.
Server text results use the same 64 KiB cap as file reads.
Long results are cut with a marker that names the full size.
The stored record holds the cut text.
