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

When a named provider refuses a protocol it still answers another way, Radian tries the other protocols and remembers the one that works (see `providers`). A candidate is skipped only on the same refusal statuses (HTTP 400, 404, 405, 415 or 422 before streaming); any other failure from it, such as a 401, 403, 429, 5xx, a transport error or a stream that already started, ends the probe and the error is that candidate's error. When every protocol is refused, the error ends with `tried <protocol> (HTTP <status>), … Set the protocol: [[providers.<name>.models]] match = "<model>" kind = "<…>"`; a candidate that ended the probe carries the same line with its own error. Set exactly that rule in `~/.radian/config/config.toml` (or the file `--config` names), or set `[[providers.<name>.models]]` for the model and start again. A rule that later fails names itself and the file: fix its `kind`, or delete the rule and let Radian resolve the model again. A configuration that is read-only (a newer `schema_version`) or cannot be written still uses the probed protocol for the session, and the notice says `not saved: <reason>`.

At startup, a `[providers.<name>]` table with no `kind` and no `base_url` adds to the `auth.json` entry of that name, so with no such entry it is refused before any model runs, even when the run selects another provider. The line names the table and says it adds to an `auth.json` provider that does not exist, ending with `add kind and base_url, or log in to it`; the process exits 2, like every other config refusal. Add the two keys to make a full declaration, or create the entry with `radian login <name>`.

Check the workspace root when file tools refuse a path.
Relative paths stay in the workspace.
Absolute paths work only under the registry and tool chain roots.
Use `read` for registry and tool chain files.
Check `.gitignore` when `glob` or `grep` skips a file.
A file over the size cap a directory or `glob` search skips is named in the result; `tools` gives the cap.
Check `.git` when a search misses versioned files.
The tools never follow a symlink.
Read the target path directly.

Check the tier when a command is refused.
The `workspace` tier confines writes to the workspace.
Switch to `full` only for a trusted project.
Check trust when project skills are missing.
Confirm the trust prompt once.
See `permissions.md` when a git hook or filter does not run in `shell` or `!`.
See `subagent-and-jobs.md` when a command wants a terminal or reads stdin.

Find crash logs under `~/.radian/cache/diagnostics`.
Files stay for 7 days.
Total size stays under 50 MiB.
Old files leave first.
A write failure never stops the agent.

When an image shows only its `[image: …]` line, press `Ctrl-P` and read the `images:` line of
the startup information: it names the protocol the startup probe picked and the cell size it
measured. `half blocks` means the terminal answered no graphics query, or the answer arrived
after the probe's two-second wait.
Inside tmux or herdr radian turns on `allow-passthrough` for the pane; a terminal outside it
that cannot pass the query through still gets half blocks.
The same line stays when the image data itself cannot be decoded: the transcript then keeps the
placeholder and draws nothing else.
See `commands.md` for which terminals draw which protocol.

Find session files under `~/.radian/sessions`.
Set `$RADIAN_SESSION_DIR` to move them.
Set `$RADIAN_HOME` to move the whole home tree.
Run `radian exec --search <TEXT>` to find a lost session.
Run `radian attach <id>` to read a session without changing it.
Run `radian docs <topic>` to read the built in manual page for a topic.
