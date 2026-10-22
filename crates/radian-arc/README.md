# radian-arc

The gate library of arc (ADR 0048). It is a library, not a command: a later slice connects it
to the state machine and the CLI. The only public module is `gate`.

## Configuration shape

arc-store parses `config.toml`; these types are the shape it hands to the gate, and the field
names are the file's names.

```toml
[gate]
profile = "default"
command_timeout_secs = 3600

[gate.profiles.default]
stages = [{ stage = "commands", required = true }, { stage = "human", required = true }]
commands = []        # shell command lines, run in order by the `commands` stage
key_commands = []    # shell command lines, run on the merge candidate by `integration`
```

- `Stage` is the closed set `commands`, `mutation`, `review`, `ci`, `human` plus
  `integration`. `integration` always runs at merge and a profile cannot list it;
  `Profile::validate` rejects it and duplicate stages.
- `effective_stages(profile, extra)` adds a task's extra stages in their order. An existing
  stage merges into its profile entry and can only raise `required`: an extra stage can never
  remove or weaken a profile stage. An extra `integration` entry is ignored.
- `command_timeout_secs` is per command for `commands` and `integration`; `0` waits forever.

## Gate checkout

`GateCheckout::new(repository, stage)` owns
`<repository>/.radian/arc/gates/<stage>/`, a detached linked worktree of the main checkout.
`prepare(sha)` fetches `origin`, resolves `sha` to a full commit (failing when it does not
exist), creates the worktree when it is missing, otherwise detaches it, then runs
`reset --hard` and `clean -ffdx -e target`. Each stage keeps its own `target/` between runs;
trees of different stages never share build state. A path that exists but is not a linked
worktree is refused, never deleted.

Every git command of this crate runs as `git -c core.hooksPath= …` with `GIT_CONFIG_NOSYSTEM=1`
and `GIT_TERMINAL_PROMPT=0`, so a repository `core.hooksPath` never runs a hook for a gate
operation and an unattended fetch fails instead of prompting. The inherited `GIT_CONFIG_COUNT`
and `GIT_CONFIG_PARAMETERS` channels are cleared: they are a caller's injected configuration,
and an injected `core.hooksPath` would otherwise replace the empty one. Configuration files
are unaffected.

## Exclusive stages

`commands` and `mutation` hold `<repository>/.radian/arc/gates/exclusive.lock` for the whole
stage (`ExclusiveLock`, a blocking `flock`). `CommandRun::run` takes it before `prepare` and
drops it after the verdict is written, so a second exclusive stage waits. A crashed process
releases the lock when its descriptor closes.

## `commands`

`CommandRun` prepares the `commands` checkout at `sha`, runs the profile's lines with `sh -c`
in order, and stops at the first failure. Each command gets its own process group, its own log
file (`<attempt>/logs/commands-<n>.log`, absolute when the repository path is), and the
profile's timeout; a timeout sends `SIGKILL` to the whole group and records `exit_code: null`
and reason `timeout`. A non-zero exit records reason `command_failed`.

## Verdict

One verdict per stage per attempt, written once with create-new semantics to
`<repository>/.radian/arc/runs/<task>/attempts/<attempt>/verdicts/<stage>.json`. A second
write for the same path is `GateError::VerdictExists`; a verdict is never changed.

```json
{
  "schema_version": 1,
  "stage": "commands",
  "sha": "0123456789abcdef0123456789abcdef01234567",
  "result": "fail",
  "reason": "command_failed",
  "commands": [
    {
      "line": "cargo nextest run --workspace",
      "exit_code": 101,
      "duration_ms": 4210,
      "log": "/repo/.radian/arc/runs/0001-x/attempts/0001-x-1/logs/commands-01.log"
    }
  ],
  "started_at": "2026-01-01T00:00:00.000000Z",
  "ended_at": "2026-01-01T00:04:12.000000Z"
}
```

`result` is `pass` or `fail`; `reason` is always present and `null` on a pass. An
`integration` verdict also carries `base_sha` (the main it used) and `candidate_sha`, and a
conflicted merge carries `conflict_paths` (sorted). `Verdict::human` builds the owner's `human`
record; it has no commands and carries the owner's note as `reason`.

`valid_verdicts(verdicts, head_sha)` keeps only verdicts for that commit, so a moved branch
invalidates the old set. `required_passed(profile, extra, verdicts, sha)` is true when every
required stage of the effective profile has a passing verdict for that sha; `integration` is
not part of it, because integration runs at merge after the approval.

`verdict_set_digest(verdicts, sha)` is the SHA-256 of the compact JSON array of the counted
verdicts, sorted by stage name, in the struct's field order (the declaration order above).
When a stage appears more than once for the same sha (several attempts), the newest by
`ended_at` counts; `started_at` and then the verdict's own JSON break ties. The digest does
not depend on input order, and a new attempt's verdict for the same sha and stage changes it.

## Integration

`IntegrationRun` fetches `origin`, then compares `main_ref` (after the fetch, for example
`refs/remotes/origin/main`) with the task's `base_sha`:

- still `base_sha`: the candidate is `head_sha`, and the key commands run in the
  `integration` checkout at `head_sha`;
- moved: the checkout goes to the remote main and `git merge --no-ff` creates
  `Merge work/<task> (<short head>)` with the fixed identity `arc <arc@localhost>`. The
  candidate is that merge commit. There is no rebase: a rebase changes the sha the gate
  verified. A conflict aborts the merge and writes a `fail` verdict with reason `conflict`,
  the sorted paths, and no candidate.

The key commands run on the candidate under the same rules as `commands` (order, stop at the
first failure, own timeout, own logs). The verdict records the main it used as `base_sha`.

`merge_ready(repository, main_ref, verdict)` fetches again and is true only for a passing
integration verdict with a candidate whose recorded `base_sha` is still the remote main. A
main that moved after integration makes it false (ADR 0048 decision 6).

## Decisions where the ADR is silent

- A checkout that exists and is not a linked worktree is an error, not a directory to clear.
- `checkout --detach --force` is used before `reset --hard`, so a modified tracked file from a
  previous command cannot block the prepare.
- `0` seconds means no command timeout.
- A merge that fails without conflicted paths writes reason `merge_failed: <git stderr>`; a
  merge that already contains `head_sha` yields the current main as the candidate.
- `GIT_TERMINAL_PROMPT=0` is set with the two variables the ADR names, so an unattended fetch
  cannot block on a prompt.
- The inherited `GIT_CONFIG_COUNT` and `GIT_CONFIG_PARAMETERS` channels are cleared: only the
  helper's own `-c` settings, the repository config and the user/system files apply.
- The newest verdict per stage is what a digest counts, so passing every attempt's verdicts in
  any order is safe.

No user-visible input is added by this slice, so the manual has no page for it.
