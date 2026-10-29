# radian-permissions

The permission layer of ADR 0012: `ApprovalPolicy` + `ToolRuleSet`, implemented as one
`radian_core::Hooks`. It decides **allow / ask / deny** for a tool call; it does not enforce
anything. `SandboxMode` (the execution boundary) is a separate layer and is never widened
here — approval cannot turn a sandbox refusal into a run.

`radian-exec` and `radian-tui` both wire this layer in as their `Hooks` implementation: their
`--profile` / `--approval` / `--allow-shell-command` flags build one of its `Profile`s and
write every decision to the session journal. The `radian-runtime` driver is not itself a host.

The sandboxed `workspace` profile is built only from an enforced `Sandbox` instance:
`workspace_write_sandboxed` takes `&Sandbox` and refuses a transparent backend, and
`with_sandbox` marks the flag only for an enforced one — the instance is the proof, not
the host's build order, and no bare `bool` can mark a profile sandboxed.

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

Removed tool names are refused as unknown tools: there is no alias mapping, so a rule
written against `git_diff`, `list` or another removed name matches only that literal name
and never affects a live tool.

`ask_user` (ADR 0024 Q6) is `allow` in **both** tiers: it writes nothing and runs
nothing, so it never prompts. It still goes through the ordinary hook path, and its answer is
data — it grants no permission for the tool calls the model makes afterwards.
(`ask_user_is_allowed_in_every_tier` in `src/profile.rs`.)

`skill` (ADR 0025 S3) is `allow` in **both** tiers for the same reason: it only reads a
`SKILL.md` that discovery already accepted, so **a skill is not a permission** — reading one
grants nothing. (`skill_is_allowed_in_every_tier` in `src/profile.rs`.)

`goal` (ADR 0047) is `allow` in **both** tiers for the same reason: it only advances the
session's own goal state and journals it — no workspace file, no command — so prompting
for it would only stall the continuation it serves. (`goal_is_allowed_in_every_tier` in
`src/profile.rs`.)

`job` (ADR 0037 decision 8) is `allow` in **both** tiers for the same reason: it only
reads, waits on or stops the session's own background jobs, and the command was approved
when it started, so reading its output later adds no rights. A user `deny`/`ask` rule for
`job` still wins, and a session whose allow-list gate denies `job` stays refused — the
rule never mounts a tool the tier or session does not offer. (`job_is_allowed_in_every_tier`
in `src/profile.rs`, `tests/job.rs`.)

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
- A compound command (`cargo fmt && cargo test`, `cd sub && cargo test`) is allowed
  only when **every** segment is allowed — by a user `allow` rule or by the read-only
  table below. Any segment without one falls through to `ask`. The command is split
  only at unquoted `&&`, `||`, `;` and `|` (`shell::segments`); any other metacharacter
  or a non-simple segment leaves the whole command at `ask`, exactly as before.
- A segment that starts with a runs-its-arguments program (`xargs`, `env`, `sudo`,
  `nohup`, `time`, `timeout`, `watch`, `sh`, `bash`, `zsh`, `eval`, `exec`, `command`,
  `builtin`, `nice`, `stdbuf` — `shell::WRAPPER_PROGRAMS`) is never allowed by a
  prefix rule or by the table: the rule only sees the wrapper's own tokens, never what
  it will run. `ls | xargs rm` therefore asks. `deny` matching is unaffected and still
  runs first.
- `cd <path>` as a segment is allowed only in the `workspace` tier, only with a user
  `allow` rule, and only when the target resolves inside the workspace (no `..` escape,
  no symlink out) and is an existing directory. In the `full` tier the tier's blanket
  `allow` judges a `cd` segment like any other (git runs the target directory's hooks,
  and there is no sandbox wall around that — `full` trusts the environment). Paths after a `cd` resolve from the `cd` target, not from the workspace
  root: the effective directory is carried through the segments, so `cd sub && cat
  link/passwd` with `sub/link` pointing out asks.
- When a table segment of a compound command would be rewritten (`git diff`/`git
  log`/`git show`), the whole command is rebuilt with that segment rewritten and every
  separator as it was — one `Rewrite` for the whole call (`cargo fmt && git log -p`
  keeps `cargo fmt` byte-for-byte and gains the flags on the `git log` segment).

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

## The built-in read-only table

On top of the rules above, the hook consults a built-in table of read-only shell commands
(`src/readonly.rs`): `git status`/`diff`/`log`/`show`, `git branch` (no arguments or
`--list`), `git rev-parse`, `git remote -v`, `git stash list`, `ls`, `pwd`, `wc`, `cat`,
`head`, `tail`, `stat`, `file`, `du`, `tree`, `rg`, `grep`, `find`, `cargo metadata` and
`cargo tree`. It sits after the user's rules and before the tier's blanket shell rule: a
rule `deny` still wins first, and a named `ask` rule (an explicit user rule) still wins
over the table — only the tier's blanket shell `ask` and the no-match default yield to
it. A hit bypasses the approval policy (`always`/`never`/`auto` never turn it into a
question: read-only commands are not worth reviewing, and a headless turn must not lose
them) and is recorded with `approver = policy`.

A hit needs all three at once:

1. The command is simple (see above): `~`, quotes, `$`, globs and `;` already fall
   through to `ask`.
2. Every non-option token resolves inside the workspace (the same `resolve` as path
   rules: no `..` escape, no leading `/`, no symlink out). For one segment of a
   compound command the resolution starts at that segment's effective directory (the
   workspace, or a `cd` target the earlier segments moved to) and must still land
   inside the workspace. A token that is not a path
   (a pattern, a count, a revision) just has to survive the same check, so plain words
   pass and only real escapes fail.
3. No executable, long-lived or file-writing option, and no unknown option. Each program
   carries its own forbidden list — `find -exec`/`-execdir`/`-delete`/`-ok`/`-fprint*`,
   `-L`/`-follow` anywhere traversal would follow symlinks out, `git -c`/`-C`/
   `--exec-path`/`--git-dir`/`--work-tree`, global `-p`/`--paginate` (it executes the
   pager), `grep`/`rg --pre`/`--pre-glob`, `tail -f`/`--follow`/`--retry`,
   `file -C`/`--compile`, `tree -o`, `git diff/log/show --output`/`--ext-diff`/
   `--textconv` — plus a small safe list; anything else falls through to `ask`.
   Over-asking is the intended failure mode here too.

`git diff`/`git log`/`git show` hits are a `Rewrite`, not an `Allow`: the hook inserts
`--no-ext-diff --no-textconv` right after the subcommand, so a model-writable
`.gitattributes` driver can never make the call execute an external diff or textconv
command (ADR 0039 decision 3). Only those three subcommands take the flags: `git
branch`/`status`/`rev-parse`/`remote`/`stash` reject `--no-textconv` (checked on git
2.53.0), and `rev-parse` echoes it back as a revision instead of accepting it. The
rewritten call is still judged `allow` by policy and audited as a policy `rewrite`.

The option lists were read from each program's `--help` on the dev box, except `rg`,
`tree` and `cargo`, which are not installed there: their lists come from their
documented options.

## Paths

`PathUnder` matches the **resolved** path: relative paths are joined to the permission
workspace, symlinks in the existing part are resolved, and `.`/`..` are removed lexically. A
link or a `..` therefore cannot move a call from one rule to another
(`rules_match_resolved_paths_so_dotdot_and_symlinks_cannot_bypass`). A `PathUnder` prefix
matches by path components, so denying `.git/config` never spills onto `.git/config-other`.

**Git executable entries (ADR 0038 decision 3).** In the `workspace` tier, `write`/`patch`
deny the `.git` entries git can execute: `config`, `config.worktree`, `hooks/...`, each
submodule's `config` and `hooks/...`, and a linked worktree's `gitdir`/`commondir`/
`config.worktree` — one `deny` rule per discovered git directory
(`ResourceMatcher::GitExec`), or a `PathUnder` deny on a worktree pointer file (rewriting
it would repoint git at a gitdir the model controls). The entry list lives in
`radian-sandbox` (`git_exec`) and is shared with the sandbox profile, so the two cannot
drift apart. The rest of `.git` stays writable, and `full` has no such rules by design.

## Approval, grants, and sources

- The host injects an `Approver` (an `Arc<dyn Approver>`). It is raced against
  `HookContext.cancellation`, so a waiting approval is interrupted, never hung
  (`cancellation_interrupts_the_approver`).
- **"Allow for this session"** is real-time state (ADR 0004): after it is granted, the next
  matching call is allowed without asking — no turn boundary. It lives only in the
  `PermissionHooks` instance, so a new session (what resume is) no longer has it
  (`a_session_grant_applies_to_the_next_call_and_not_after_resume`). It is never written into
  a profile or the JSONL.
- **A session grant for a `shell` call widens from the exact command line to the command's
  prefix** — only for a simple command (`shell::session_grant_prefix`). For `cargo`, `git`,
  `npm`, `pnpm`, `yarn`, `go` and `docker` the prefix is the program plus the first
  non-option token (`cargo test`, `git log`, `npm run`); for every other program it is the
  program alone (`rg`, `make`). A compound command, a wrapper first token
  (`shell::starts_with_wrapper`, including `eval`), `source`/`.`, and anything non-simple
  keep exact-string grants (or ask). These never become prefix grants, the exact string
  only: `git push`, `git reset`, `git clean`, `git checkout`, `rm`, `mv`, `dd`, `chmod`,
  `chown`, `curl`, `wget`, `ssh`, `scp`, `docker run`, `kubectl`
  (`shell::NEVER_PREFIX_COMMANDS`). Prefix grants live where session grants live (real-time
  state, per session); they are never written into the project's `config.toml` — an allow
  can never come from the project (`a_project_config_cannot_create_a_prefix_grant`). A
  non-human turn cannot borrow a prefix grant either
  (`a_non_human_turn_cannot_borrow_a_prefix_grant`).
- A turn whose source is not `User` cannot turn an `ask` into an `allow`, and cannot borrow a
  human session grant; an `ask` from such a turn is denied unless a `PermissionRequest` hook
  answers it (ADR 0045 §4). A grant such a hook records with `updatedPermissions` is the one
  exception: it answers later asks on every turn source, because a script, not a human,
  recorded it. An unset source is treated the same way (fail safe). Explicit
  `allow`/`deny` **rules** still apply to every source — they are the host's policy, not a
  borrowed user permission (`a_non_human_source_cannot_ask_or_borrow_a_grant`).
- **`PermissionRequest` hooks** (ADR 0045 §4): the host installs a `PermissionRequestHooks`
  chain with `PermissionHooks::set_permission_request_hooks`. It runs after a call resolves
  to `ask` and no session grant covers it, before the approver and before the non-human
  denial. `Allow { session_grant }` returns `ToolDecision::Allow` (audited as `user`);
  `session_grant` records the same grant "allow for this session" would — the exact key, or
  a `shell` prefix (kept apart from the human grants, so it answers asks on any turn
  source) — so a later matching call runs neither the hook nor the approver. `Deny { reason }`
  returns `ToolDecision::Deny` with that reason, `NoDecision` keeps the ordinary path, and
  `Err` fails closed as `hook_error` (never a prompt). `radian-host`'s `CommandHooks`
  implements the trait; this crate holds only the trait, so it never depends on the host.
- **`Notification` hooks** (ADR 0045 §6): the host installs a `NotificationHooks` chain with
  `PermissionHooks::set_notification_hooks`. It is called at the moment a human is about to
  be asked — after the `PermissionRequest` chain gave no decision and before the approver —
  with the tool name only, and never with arguments. It observes: it decides nothing, and an
  implementation must start its work and return at once so a slow hook cannot delay the
  prompt. `radian-host`'s `CommandHooks` implements it the same way.
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
| 13 | read-only shell table allows without a prompt, incl. under `always`/`never` and non-human turns | `tests/readonly.rs` |
| 14 | paths, options and subcommand shapes outside the table still ask | `tests/readonly.rs` |
| 15 | `git diff`/`log`/`show` rewritten with `--no-ext-diff --no-textconv`, audited as a policy rewrite | `tests/readonly.rs` |
| 16 | `deny` and named user-`ask` outrank the table; user `allow` keeps the git rewrite | `tests/readonly.rs` |
| 17 | compound shell allows only when every segment is allowed (`a && b` asks when `b` has no rule) | `tests/compound.rs` |
| 18 | a wrapper segment (`ls \| xargs rm`) is never allowed by a prefix rule or the table | `tests/compound.rs` |
| 19 | quoted separators do not split; `cd` needs the workspace tier and an inside target; later paths resolve from the `cd` target | `tests/compound.rs` |
| 20 | a table segment in a compound command is rewritten in place, separators kept | `tests/compound.rs` |
| 21 | session grant widens to the command prefix (`cargo test -p b` covered) | `tests/session_prefix.rs` |
| 22 | a bare program grants the program, a subcommand program keeps its subcommand | `tests/session_prefix.rs` |
| 23 | a compound command neither earns nor borrows a prefix grant | `tests/session_prefix.rs` |
| 24 | the never-prefix list keeps exact-string grants | `tests/session_prefix.rs` |
| 25 | a project config cannot create a prefix grant (grants are per-session state) | `tests/session_prefix.rs` |
| 26 | a non-human turn cannot borrow a prefix grant | `tests/session_prefix.rs` |
| 27 | a prefix grant never crosses the sandbox boundary | `tests/session_prefix.rs` |
| 28 | `job` is fixed `allow` in every tier (sandboxed `workspace` too), `ask`/`deny` user rules still win, and `approval = never` keeps it (ADR 0037 decision 8) | `tests/job.rs`, `src/profile.rs` |
| 29 | a `PermissionRequest` hook answers before the approver (and on non-human turns), its deny/error never prompts, rules/grants/auto never fire it, its `session_grant` covers later calls (a `shell` prefix too) on any turn source and dies with the session, and cancellation interrupts it | `tests/hooks.rs` |

## Not verified / limits

- The shell approximation is exactly that; see the list above. Do not read a passing test as
  a proof that a given command string is safe.
- Only the modelled tool argument shapes (`command`, `path`, else the name) are matched; a
  tool that carries its resource elsewhere would fall to `Name` (and thus typically to `ask`).
- The `Mcp` matcher is `name.contains("__")`, not a registry lookup.
- The layer is wired into `radian-exec` and `radian-tui` (see the top of this file). A
  non-human turn can be answered by a host-installed `PermissionRequest` hook; the other
  non-human paths are covered by the unit tests in `tests/hooks.rs`, not end to end.
