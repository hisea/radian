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

Trust controls project resources.
A new repository starts untrusted.
Untrusted projects do not load project skills.
Untrusted projects do not load project servers.
Untrusted projects still read context files as data.
Pass `--trust` to trust this run only.
Pass `--no-trust` to distrust this run only.
Neither flag writes to disk.
Confirm trust once to save it.

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
