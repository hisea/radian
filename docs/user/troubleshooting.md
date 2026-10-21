---
description: Fix common setup, network, and session problems.
---

# Troubleshooting

Run `radian login` again when requests fail with an auth error.
Run `radian logout <provider>` to remove a bad key, then log in again.
Check `--base-url` when the endpoint is custom.
Check `--provider` when the wire format is wrong.
Use `openai-compatible` for most endpoints.
Use `anthropic` only for Anthropic endpoints.
Pass `--offline-catalog` when the network blocks the catalog fetch.
Pass `--context-window <TOKENS>` when the catalog has no entry.

Check the workspace root when file tools refuse a path.
Relative paths stay in the workspace.
Absolute paths work only under the registry and tool chain roots.
Use `read` for registry and tool chain files.
Check `.gitignore` when `glob` or `grep` skips a file.
Check `.git` when a search misses versioned files.
The tools never follow a symlink.
Read the target path directly.

Check the tier when a command is refused.
The `workspace` tier confines writes to the workspace.
Switch to `full` only for a trusted project.
Check trust when project skills are missing.
Confirm the trust prompt once.

Find crash logs under `~/.radian/cache/diagnostics`.
Files stay for 7 days.
Total size stays under 50 MiB.
Old files leave first.
A write failure never stops the agent.
Find session files under `~/.radian/sessions`.
Set `$RADIAN_SESSION_DIR` to move them.
Set `$RADIAN_HOME` to move the whole home tree.
Run `radian exec --search <TEXT>` to find a lost session.
Run `radian attach <id>` to read a session without changing it.
Run `radian docs <topic>` to read the built in manual page for a topic.
