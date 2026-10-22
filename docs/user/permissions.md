---
description: Permission tiers, trust, approval, and sandbox behavior.
---

# Permissions

Radian has two permission tiers.
Use `workspace` for sandboxed work.
Use `full` for unsandboxed work.
The default tier is `full`.
The startup line names the active tier.
Switch tiers with `--profile <tier>`.
Switch tiers inside the interface with `/access <tier>`.
A project file can only narrow the tier to `workspace`.
A project value of `full` warns and has no effect.

The `workspace` tier confines writes to the workspace.
The `workspace` tier runs shell commands under the OS sandbox.
The `workspace` tier leaves the network on.
The `workspace` tier keeps the repository write rules active.
The `full` tier runs without a sandbox.
The `full` tier runs without built in prompts.
Deny rules still deny in both tiers.
The sandbox still applies in `workspace` when approval is automatic.

The file tools read the workspace, read-only roots, and more under `full`.
Use a workspace-relative path for everything the file tools do.
Name a read-only root as an absolute path or with `~`, in either tier.
Read-only roots are the cargo registry sources, every toolchain's sources, and each discovered skill's directory.
Name any other absolute or `~` path only under `full`.
The file tools never read the sandbox's deny set, in either tier.
The deny set is, under `$HOME`: `.ssh`, `.aws`, `.gnupg`, `.netrc`, `.npmrc`, `.config/gcloud`, `.docker/config.json`, `Library/Application Support/Google/Chrome`, `Library/Application Support/Firefox`, `Library/Safari`, and `Library/Cookies`.
Under `~/.radian` (or `$RADIAN_HOME`) it is the sessions directory (or the one `--session-dir` names), `memory/`, `credentials/`, `trust/`, and `access/`.
The check follows symbolic links, so a link that points into the deny set is refused too.
A refused read says the path is in the sandbox's deny set.
Under `full` the shell is not limited by the deny set; use `workspace` to keep it from the shell too.
Writes stay in the workspace in every tier.

Trust controls project resources.
A new repository starts untrusted.
Untrusted projects do not load project skills.
Untrusted projects do not load project servers.
Untrusted projects still read context files as data.
Pass `--trust` to trust this run only.
Pass `--no-trust` to distrust this run only.
Neither flag writes to disk.
Confirm trust once to save it.
Show or change the saved decision with `/trust` inside the interface
(`/trust status` shows the current directory's decision and its source;
`/trust on`, `/trust off` and `/trust forget` change disk state only and take effect
at the next start) or with `radian trust status|on|off|forget|list` outside it.
After `/trust on` the next start shows no hint; after `/trust off` the hint says the
project is distrusted.

Approval controls questions.
Use `on-ask` to ask when a rule says so.
Use `always` to ask for every call a rule does not deny.
Use `never` to turn questions into denials.
Use `auto` to answer every question with allow.
Set the policy with `--approval <policy>`.
Set the policy in the file with `[permissions] approval`.
Set the policy for one session with `/auto on` or `/auto off`.
Pass `--yes` to approve every tool call without asking.
Deny rules still deny with `--yes`.
The sandbox still applies with `--yes`.
A value of `auto` in the file has no effect under `full`.

Allow named shell prefixes with `--allow-shell-command "<prefix>"`.
Repeat the flag for more prefixes.
Only a simple command matches.
A compound command needs every part to match.
A compound command with one unmatched part still asks.
