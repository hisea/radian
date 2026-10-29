# ADR 0012: Permission Sources

Status: accepted

## Background

Permission is three orthogonal objects:

```text
PermissionProfile = SandboxMode + ApprovalPolicy + ToolRuleSet
```

This ADR fixes what each layer controls, what cannot widen what, what Core provides and does not provide, and the real sandbox boundary. Tier names and current network behavior live in ADR 0016. The default tier is `full` (ADR 0043).

## Decisions

### P1: Three layers and what cannot widen what

- **`SandboxMode` is the real execution boundary** (a forced constraint on what a process, file, or network access can touch). **Approval cannot widen it**: approval only decides "does this command ask a human". It never turns a sandbox-denied operation into a pass.
- **`ApprovalPolicy` decides whether a matched rule action pauses for human confirmation**. A "grant once" or "always allow for this session" grant is only a temporary grant at the rule layer. An approval is one-shot. It never replays across resume.
- **`ToolRuleSet` matches `allow`/`ask`/`deny` for tool plus resource, command, or MCP name**. Rules narrow per profile. Any organization policy can only hard-deny. No project configuration, skill, or "always allow" grant widens it.
- Rule order is: deny rules first, then user rules, then the host built-in read-only shell table, then the tier-wide shell rule. A call that hits the read-only table never prompts under any approval policy. `approval = always` also skips review for it. Read-only commands still pass when `ask` in a headless turn is treated as deny. `approval = never` behaves the same. A `git diff`, `git log`, or `git show` hit rewrites parameters. It appends `--no-ext-diff --no-textconv` automatically after the subcommand. This matches the host git call caliber in ADR 0039 decision 3: an external diff or textconv command named by a model-writable `.gitattributes` never runs.

The tiers are `workspace` and `full` (ADR 0016). With no narrower CLI, configuration, project, or user source, TUI and `radian exec` default to `full` (ADR 0043). A switch to `full` from another tier inside a session needs explicit human confirmation. The model cannot switch tiers by itself. `full` is unlimited automatic execution: its shell and MCP rules are `allow`. It raises no built-in prompt. User-written `deny` and `ask` rules still apply. Deny still runs first. `approval = always` still reviews each call. Safety rests on the user environment: a container, a dedicated OS user, or a switch to `workspace`.

A `workspace` shell call is `allow` inside the sandbox and `ask` across the boundary. When the tier is `workspace`, the call runs inside the sandbox (a probe-success local backend, or the `inherited` backend in ADR 0016), and no `deny` rule hits, the shell call passes directly. `deny` always runs first. MCP in `workspace` is still `ask`. The pass happens inside the sandbox. What passes is only "whether to ask a human". A sandbox-denied operation is still denied. Approval still cannot widen the sandbox (P1 still holds). When the probe fails (no sandbox exists), shell calls still use `ask`. The behavior is fail closed. A model that must cross the boundary passes `outside_sandbox: true` with a reason explicitly, then calls shell again. That call uses `ask`. After approval, only that one call runs outside the sandbox. In a headless session an `ask` no `PermissionRequest` hook answers is still treated as deny (ADR 0045 §2). The runtime never reruns a call outside the sandbox automatically. A `workspace` profile inside the sandbox builds only from an effective `Sandbox` instance. `workspace_write_sandboxed` takes `&Sandbox` and rejects the transparent backend. The instance itself is the proof. No path marks sandbox state with a bare `bool`. A command that the sandbox denies carries a fixed marker in the tool result: `sandbox denied: <category>`. Categories live in `crates/radian-sandbox/LIMITS.md`.

### P2: Boundary-effective configuration versus hook live state

- **A mid-turn user grant of "always allow" is hook live state**. `before_tool_call` reads the live source on each call. It **never** enters the boundary-effective slot (ADR 0004 D6). A slot boundary would delay it to the next turn. That delay violates "immediate".
- Boundary-effective means model route, tier, thinking, and auto. They travel by slot swap. Live state means mid-turn grants and override switches. The hook reads them directly.
- Approval "valid for this call" rests on hook liveness. A host that caches an approval into the profile slot and expects "still valid next turn" violates this rule. It also violates "approvals never replay across resume".
- **"Allow for this session" is hook live state**. One `AllowForSession` records one memory grant, and only inside the granting `PermissionHooks` instance. The next matching call passes directly. A new instance (resume always creates a new instance) lacks it. It enters no profile, no JSONL, and no configuration file. A pass must never come from the project: project configuration only narrows (ADR 0008). A `shell` grant takes effect by command prefix, and only for simple commands. `cargo`, `git`, `npm`, `pnpm`, `yarn`, `go`, and `docker` match "program plus first non-option token" (`cargo test`, `git log`). All others match the program name (`rg`, `make`). A compound command, a wrapper-led command (including `eval`), `source` or `.`, and any other non-simple command take effect only by full string. `git push`, `git reset`, `git clean`, and `git checkout` never take effect by prefix. `rm`, `mv`, `dd`, `chmod`, `chown`, `curl`, `wget`, `ssh`, and `scp` never take effect by prefix. `docker run` and `kubectl` never take effect by prefix. All of them match only by full string. A non-human turn cannot borrow any human grant (ADR 0011 L2 still holds). The one exception is a grant a `PermissionRequest` hook recorded with `updatedPermissions` (ADR 0045 §2): a script, not a human, earned it, so it answers later asks on every turn source, including a non-human one.

### P3: What Core provides and does not provide

- **Provides**: `Hooks::before_tool_call` returns `ToolDecision::{ Allow, Deny{reason}, Rewrite{arguments}, Respond(ToolResult) }`. Calls in one batch run one by one in call order. `Rewrite` parameters validate again, but the hook **never reruns**. **Fail-closed**: a `before_tool_call` error rejects that call. Hook order and journal position are: assistant message journals first, then the hook judges, then dispatch runs (ADR 0002 E3).
- **Does not provide**: permission judgment itself never enters `radian-core`. Core **records no approval decisions** (ADR 0002 E2). Approval audit lives at the runtime layer: `PermissionResolved { request_id, call_id, decision, approver, reason, policy? }`, `critical=false`. It is durable audit, but it carries no state rebuild and never replays across resume.
- A `ToolDecision::Deny{reason}` result is visible to the model. `Respond(ToolResult)` answers directly with the given result.

### P4: Real boundary of the macOS sandbox

**Blocks (macOS 27 / arm64, sandbox-exec plus SBPL)**: writes outside the workspace (absolute paths, `..`, traversal through symlinks), localhost port connects with network off, shared `.git` writes (hooks, config, info, index.lock, worktree pointers, `core.hooksPath`), nested `sh -c` inherits the sandbox, normal reads and writes inside the workspace. SBPL uses "default-deny plus selective allow plus deeper deny subpath overrides shallower allow". Environment filtering is not a sandbox capability: standard variables resolve spawner-side (`env_clear` plus allowlist). MCP child processes follow the same allowlist.

**Cannot block (documented here, or the implementation misjudges)**:

1. **The sandbox can read the whole `$HOME`** (current tier read scope follows the ADR 0020 denylist; outside the denylist it still reads). The environment allowlist has no effect on file content.
2. **`(allow mach-lookup)` is fully open**: `osascript` can run inside the sandbox. The IPC boundary is not yet tightened. No minimal mach allowlist exists.
3. **Already-open file descriptors have no constraint** (sandbox(7) verbatim): the spawner must pass no writable fd to sandbox children.
4. **`sandbox-exec` is deprecated**. SBPL is a private interface and drifts with macOS versions. The mitigation concentrates profile generation and operation names in one sandbox backend place, probes at startup, and honestly disables on failure per the rules.

**Unverified (never claimed as capability)**: versions past macOS 27; a real `cargo build` inside the sandbox; fine-grained `(allow network*)`; Intel and FileVault; alternative mechanisms; time-injection escapes.

### P5: Explicit failure when the backend is unavailable

A sandbox request with no available backend **must fail explicitly, never silently downgrade to unsandboxed execution**. Approval must never impersonate the sandbox. `workspace` shell and MCP open only after the matching platform sandbox backend verifies. Exception: a process already constrained by an outer sandbox follows the outer layer (the ADR 0016 inheritance rule). That is not a downgrade.

### P6: MCP child processes stay unsandboxed

MCP service processes never start under the sandbox. They run with the full user permissions. The startup notice must state plainly that they run outside the sandbox (`MCP servers are NOT sandboxed in this build and run with your full access`). It must never write an unqualified "writes are confined to the workspace". Tests guard this disclosure: deletion fails the tests.

### P7: Audit boundary

- Each approval and each denial writes a durable `PermissionResolved`. The Core E2 narrowing means approvals have no independent record. A host that needs a complete approval audit keeps its own record at the hooks layer.
- MCP lifecycle is absent from the v1 journal (ADR 0006).

## Constraints and costs

- The sandbox can read the whole `$HOME`, and `mach-lookup` is fully open. Therefore the code **must never describe the sensitive-directory denylist as a complete credential-isolation guarantee**. The honest claim is only "outside the workspace is unwritable, sensitive directories are unreadable by default, the IPC boundary is not yet tightened".
- A `workspace` tier with network on (ADR 0016): content readable inside the sandbox can in theory all leave. Main protection becomes the denylist and the workspace write boundary.

## Verification

`crates/radian-core/tests/hooks.rs` covers fail-closed, `Rewrite` without rerun, and in-batch order. `radian-permissions` rule tests cover allow, ask, and deny plus only-narrow. `radian-coding` startup-copy tests assert the MCP disclosure and assert no credential-isolation claim. The `radian-sandbox` escape matrix covers the P4 list.
