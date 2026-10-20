# radian-permissions

The permission layer of ADR 0012: `ApprovalPolicy` + `ToolRuleSet`, implemented as one
`agent_core::Hooks`. It decides **allow / ask / deny** for a tool call; it does not enforce
anything. `SandboxMode` (the execution boundary) is a separate layer and is never widened
here — approval cannot turn a sandbox refusal into a run.

`agent-cli` and `radian-tui` both wire this layer in as their `Hooks` implementation: their
`--profile` / `--approval` / `--allow-shell-command` flags build one of its `Profile`s and
write every decision to the session journal. The `radian-runtime` driver is not itself a host.

## Rules

A rule is `(tool matcher, resource matcher) -> allow | ask | deny`.

- Tool matcher: `Any`, `Name("read")`, `Prefix("server__")`, or `Mcp` (matches the
  `server__tool` alias form; an approximation — a name containing `__` for another reason
  also matches).
- Resource matcher: `Any`; `PathUnder(prefix)` for a file path; `CommandPrefix(["git",
  "status"])` for a shell command; `Name("server__tool")` for an MCP alias / any
  parameterless tool.
- The resource is taken from the call: `command` argument if present, else `path`, else the
  tool name.

**Priority: `deny` > `ask` > `allow`; no rule matching means `ask`.** So an `ask` rule beats
an `allow` rule when both match, and the default is conservative.

The memory tool (ADR 0021 D5) follows this rule: `memory_search` is `allow` in both tiers.
There is deliberately no agent tool for archive/forget/purge/confirm/save/revise (D5: the
model has no write path).

Renamed tools (ADR 0043 decision 3) are judged under their **new** name
(`rule::canonical_tool_name`): a call to `git_status` is checked as `git_diff`. A config rule
written against an old name therefore never matches; the alias can never bypass a `deny` on
the new tool.

`ask_user` (ADR 0024 Q6) is `allow` in **all three** tiers: it writes nothing and runs
nothing, so it never prompts. It still goes through the ordinary hook path, and its answer is
data — it grants no permission for the tool calls the model makes afterwards.
(`ask_user_is_allowed_in_every_tier` in `src/profile.rs`.)

`skill` (ADR 0025 S3) is `allow` in **all three** tiers for the same reason: it only reads a
`SKILL.md` that discovery already accepted, so **a skill is not a permission** — reading one
grants nothing. (`skill_is_allowed_in_every_tier` in `src/profile.rs`.)

## Shell matching (the part most likely to hide a hole)

This is a deliberate conservative approximation, **not a shell parser**.

- An `allow`/`ask` command rule matches only a **simple command**: every character must be in
  `[A-Za-z0-9_./:=+@,-]` or a space/tab, and the command is split on whitespace and matched
  token-wise (`git status` matches `git status --short`, not `git statusfoo`). Any
  metacharacter or structure — `;` `&` `|` `` ` `` `$` `$(` `>` `<`, a newline, quotes,
  backslash, globs, `( )` `{ }` `[ ]`, `~`, `!`, `#` — makes the command not simple, so
  `allow` does not apply and the call falls through to `ask`. The bypass list
  (`shell::METACHARACTERS`) is exercised by
  `shell_metacharacters_are_never_allowed`; over-asking is the intended failure mode.
- A `deny` command rule is matched conservatively over the whole composite command: the
  command is split on separators and the contents of quotes/backticks/subshells, and any
  segment whose token matches (or whose file name matches, so `/bin/rm` counts) denies the
  whole command.

**What this cannot see, and does not claim to:** shell aliases, `eval`, `.`/`source`, a
called program that starts its own shell, and a nested command inside quotes (the outer
command is non-simple, so it asks, but the nested text is not parsed). The real execution
boundary is the sandbox; this layer only asks and denies on top of it.

**A program's own options can run code, and the character allowlist cannot see it** (review
note, 2026-10-12). Every character in these is allowlisted, so each is a "simple command":

- `git -c core.fsmonitor=/tmp/x status` — git runs `/tmp/x` on `status`;
- `git -c core.sshCommand=/tmp/x fetch` — git runs it to connect;
- `cargo build` — runs every dependency's `build.rs`; `npm install`/`make` run scripts.

The token-wise prefix match is what keeps a **narrow** rule safe: `allow git status` does not
match `git -c core.fsmonitor=… status`, because the second token is `-c`, not `status`. A
**broad** rule such as `allow git` or `allow cargo` matches all of the above and therefore
allows arbitrary code. **Write `allow` rules at the subcommand level, never at the bare
program name,** and treat build tools as code execution by design — the sandbox, not this
layer, is what contains them.

## Paths

`PathUnder` matches the **resolved** path: relative paths are joined to the permission
workspace, symlinks in the existing part are resolved, and `.`/`..` are removed lexically. A
link or a `..` therefore cannot move a call from one rule to another
(`rules_match_resolved_paths_so_dotdot_and_symlinks_cannot_bypass`).

## Approval, grants, and sources

- The host injects an `Approver` (an `Arc<dyn Approver>`). It is raced against
  `HookContext.cancellation`, so a waiting approval is interrupted, never hung
  (`cancellation_interrupts_the_approver`).
- **"Allow for this session"** is real-time state (ADR 0004): after it is granted, the next
  matching call is allowed without asking — no turn boundary. It lives only in the
  `PermissionHooks` instance, so a new session (what resume is) no longer has it
  (`a_session_grant_applies_to_the_next_call_and_not_after_resume`). It is never written into
  a profile or the JSONL.
- A turn whose source is not `User` cannot turn an `ask` into an `allow`, and cannot borrow a
  session grant; an `ask` from such a turn is denied. An unset source is treated the same way
  (fail safe). Explicit `allow`/`deny` **rules** still apply to every source — they are the
  host's policy, not a borrowed user permission
  (`a_non_human_source_cannot_ask_or_borrow_a_grant`).
- Internal errors fail closed: a failed approver returns `Err` (Core denies) and still writes
  one policy denial (`an_internal_error_denies`).

### The current turn source (a Core gap, worked around)

`HookContext` has **no** source field. The host sets it per turn with
`PermissionHooks::set_turn_source(Some(InputSource::User))` before `start_turn`. This is a
host obligation and it is **not** concurrency-safe for a shared instance serving several
turns: one instance is one session. If Core later adds the turn source to `HookContext`, the
setter becomes unnecessary. Until then, forgetting to set it fails closed (non-human).

## Audit

Every decision emits exactly one `PermissionResolved` (ADR 0006 §5.9) through an injected
`AuditSink`; `approver` is `policy` for a rule/grant decision and `user` for an answered
prompt (`each_decision_emits_exactly_one_permission_resolved`). The `RuntimeEvent` write side
and the typed read side live in `radian-session`
(`permission_resolved_round_trips_and_is_not_replayed`): `critical = false`, and it is not a
transcript record, so it never appears in `journal_records`/`replay`.

## Tests

| # | Test | File |
|---|---|---|
| 1 | priority deny > ask > allow, default ask | `tests/rule.rs` |
| 2 | metacharacter bypass list never allowed | `tests/rule.rs` |
| 3 | a deny segment denies the whole composite command | `tests/rule.rs` |
| 4 | session grant applies next call, not after resume | `tests/hooks.rs` |
| 5 | cancellation interrupts the approver | `tests/hooks.rs` |
| 6 | non-human source cannot ask or borrow a grant | `tests/hooks.rs` |
| 7 | internal error denies | `tests/hooks.rs` |
| 8 | one `PermissionResolved` per decision, right approver | `tests/hooks.rs` |
| 9 | `PermissionResolved` round trip, not replayed | `radian-session/tests/permission_event.rs` |
| 10 | preset behaviour (workspace asks MCP, full allows) | `tests/rule.rs` |
| 11 | resolved-path matching (`..`, symlinks) | `tests/rule.rs` |
| 12 | `ask_user` is fixed `allow` in both tiers (ADR 0024 Q6) | `src/profile.rs` |

## Not verified / limits

- The shell approximation is exactly that; see the list above. Do not read a passing test as
  a proof that a given command string is safe.
- Only the modelled tool argument shapes (`command`, `path`, else the name) are matched; a
  tool that carries its resource elsewhere would fall to `Name` (and thus typically to `ask`).
- The `Mcp` matcher is `name.contains("__")`, not a registry lookup.
- The layer is wired into `agent-cli` and `radian-tui` (see the top of this file), but those
  hosts only ever produce `InputSource::User` turns; the non-human paths are covered by the
  unit tests in `tests/hooks.rs`, not end to end.
