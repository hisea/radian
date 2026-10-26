# Terminal-Bench A/B scripts

These scripts run the side-by-side Terminal-Bench comparisons from a host: radian
builds against each other, against pi and opencode, and one binary with and
without a system-prompt file. They used to live only in `$HOME/tb2` on each host;
they now live here so every host runs the same copy from a checkout.

Two rules keep a comparison meaningful:

- **A smoke run comes first.** `smoke.sh` runs one easy task with one trial and
  proves Docker, Harbor and the model key work on that host before a multi-hour
  comparison starts.
- **Every arm of one comparison runs at the same time on the same host.** The
  arms then see the same API conditions. Do not split arms across hosts or start
  them hours apart.

The hosts keep their data outside the checkout: jobs under
`$HOME/tb2/jobs/<label>/` and binaries under `$HOME/tb2/bin/`. The scripts only
read a binary and its `.sha256` sidecar there; nothing is written into the
checkout.

## Running from a commit

Check out the commit on the host, then run the script from that checkout:

```sh
git -C <checkout> fetch
git -C <checkout> checkout --detach <sha>

bash <checkout>/benchmarks/terminal-bench/ab/smoke.sh radian-<sha>
bash <checkout>/benchmarks/terminal-bench/ab/ab.sh <label> radian-<sha>
```

Each script finds `benchmarks/terminal-bench` and `benchmarks/terminal-bench/ab`
from its own location (`$(dirname "$0")`) and puts both on `PYTHONPATH`; it does
not import code from `$HOME/tb2` or from another checkout. The binary under test
still comes from `$HOME/tb2/bin`: build a static one with
[`../build-static.sh`](../build-static.sh), copy it to
`$HOME/tb2/bin/<name>`, and keep the `<name>.sha256` file beside it.

## Scripts

| Script | What it does | Arguments |
|---|---|---|
| `smoke.sh` | One task (`merge-diff-arc-agi-task`), one radian trial, one attempt: checks Harbor, Docker and the key. | `<binary-name> [label]`; the label defaults to `smoke` |
| `ab.sh` | The selected task set (default `ab3`): each radian binary named on the command line, plus pi 1.4.0 and opencode 1.19.4, all started at once. Samples load. | `<label> <radian-bin-name>...` |
| `ab-pi.sh` | Re-runs only the pi arm of an existing label, against that label's saved task set. Moves a previous `pi/` and `pi.log` to `pi.failed-start*` first. | `<label>` |
| `ab-system.sh` | One radian binary twice: the built-in role answer versus a role file passed as `--system`. Copies the role file to `<label>/role.txt`. Samples load. | `<label> <radian-bin-name> <role-file>` |
| `run.sh` | One radian binary at `RADIAN_BENCH_BINARY`, pi and opencode, all started at once, with `-n 4` each. Extra arguments go to Harbor (for example `-i fix-git`). | `<label> [harbor args...]` |
| `audit_images.sh` | Inspects every cached Terminal-Bench task image for answer files and git history. Writes its report to stdout. | none |
| `abstat.py` | Per-arm counts from one label: started, finished, pass, timeouts, agent minutes. | `<arm-dir>...` (for example `$HOME/tb2/jobs/<label>/pi`) |
| `scan_leaks.py` | Scans each trial's agent logs for references to the benchmark sources, solution or test files. Exits 1 on any hit. | `<jobs-dir>...` |
| `radian_system.py` | Harbor agent: `radian_agent.RadianAgent` with `--system` added from `RADIAN_AB_SYSTEM_FILE`. | imported by Harbor, not run directly |
| `opencode_go.py` | Harbor agent: the stock opencode agent with `OPENCODE_API_KEY` passed through, and no `nodejs`/`npm` install on glibc images. | imported by Harbor, not run directly |
| `pi_opencode_go.py` | Harbor agent: the stock pi agent with `OPENCODE_API_KEY` passed through. | imported by Harbor, not run directly |

Each agent arm uses the model `opencode-go/deepseek-v4.1-flash` with thinking
high (opencode: variant high, web search off). A radian arm also runs with
`--profile full` inside the task container.

## Choosing the tasks

`tasks/` holds one file per task set. A file has one Terminal-Bench task name
per line; a `#` starts a comment that runs to the end of the line, and blank
lines are ignored. A task name matches `^[a-z0-9][a-z0-9-]*$`; a set may not be
empty, and it may not repeat a name.

| File | What it holds |
|---|---|
| `tasks/ab3.txt` | The ten tasks that ab1, ab3 and absys1 ran, in their order. It is the default set, so a run that sets no `AB_TASKS` is unchanged. |
| `tasks/calibrate-2026-10.txt` | The five tasks that separated the agents, plus ten medium-difficulty candidates, for one calibration run. The default set is chosen from its results. |

`AB_TASKS` selects the set for `ab.sh`, `ab-system.sh` and `ab-pi.sh`:

- A bare name (no `/`) means `tasks/<name>.txt` next to the scripts, so
  `AB_TASKS=ab3` and the default `ab3` are the same file.
- A value with a `/` is a path to a task set file, absolute or relative to the
  directory the script runs in.

The default is `ab3`. An unknown bare name, a missing path, a set with no task
names, a duplicate name, or a name that does not match the pattern stops the
script before Harbor starts, with one line on stderr naming the problem. One
shared helper, `task_set.py`, resolves and validates the list for all three
scripts.

Each of the three scripts writes the resolved list it used to
`<label>/tasks.txt`, one task name per line, with no comments. `ab-pi.sh` is the
exception to selection: when `<label>/tasks.txt` already exists it re-runs that
label's list. If `AB_TASKS` is also set, it must resolve to the same list (the
same task names in the same order) or `ab-pi.sh` refuses to run, so one label
never mixes sets. Without the file, `ab-pi.sh` resolves `AB_TASKS` (or `ab3`)
and writes it.

## Environment variables

Every script also reads `HOME` (the `$HOME/tb2` root) and the shell scripts
prepend `$HOME/.local/bin` to `PATH` so `harbor` resolves.

| Variable | Read by | Default | Effect |
|---|---|---|---|
| `AB_TASKS` | `ab.sh`, `ab-system.sh`, `ab-pi.sh` | `ab3` | The task set: a bare name under `tasks/`, or a path to a file. See [Choosing the tasks](#choosing-the-tasks) for the format and the refusals. |
| `AB_N` | `ab.sh`, `ab-system.sh`, `ab-pi.sh` | `2` in `ab.sh` and `ab-pi.sh`; `3` in `ab-system.sh` | Passed as `-n` (trials per arm). `run.sh` ignores it and passes `-n 4`; `smoke.sh` passes `-n 1`. |
| `AB_K` | `ab.sh`, `ab-system.sh`, `ab-pi.sh` | `2` in `ab.sh` and `ab-pi.sh`; `3` in `ab-system.sh` | Passed as `-k`. `run.sh` and `smoke.sh` pass no `-k` and use Harbor's default. |
| `RADIAN_AB_SYSTEM_FILE` | `radian_system.py` | unset | Path to a UTF-8 role file. When set, the agent appends `--system <quoted text>` to the command; when unset, the command is unchanged. `ab-system.sh` sets it to `<label>/role.txt` for its concise arm and unsets it for its base arm, so a value exported by the caller cannot silently give both arms a `--system`. |
| `RADIAN_BENCH_BINARY` | `run.sh` | `$HOME/.cache/radian-musl/target/release/radian` | Host path of the static radian binary under test; its `.sha256` sidecar must exist beside it. |

The scripts export these themselves; they are not inputs:

- `RADIAN_BENCH_API_KEY` and `OPENCODE_API_KEY`: the key is read at run time
  from `~/.radian/credentials/auth.json` (`opencode-go.key`) and never stored in
  this directory.
- `PYTHONPATH`: `<checkout>/benchmarks/terminal-bench` plus
  `<checkout>/benchmarks/terminal-bench/ab`, derived from `$0`.

## Job directory layout

```text
$HOME/tb2/jobs/<label>/
  done            # one line per finished arm: "<arm> rc=<code>" (smoke: "rc=<code>"); a failed arm still writes its line
  loadavg.log     # UTC timestamp plus 1/5/15-minute load, every 30 s (ab.sh, ab-system.sh)
  tasks.txt       # the resolved task list the run used, one name per line
  <arm>/          # Harbor output for that arm (one directory per binary name, pi, opencode)
  <arm>.log       # Harbor stdout and stderr for that arm
  role.txt        # ab-system.sh only: the role file handed to the concise arm
```

`<arm>` is the binary name given to `ab.sh`/`ab-system.sh`, or `pi`, `opencode`,
`radian`, `base` or `concise`. `done` is append-only, so a relaunch such as
`ab-pi.sh` adds a second `pi` line and the first one stays as history.

## Test

The scripts' checks need no Harbor, Docker or model:

```sh
python3 benchmarks/terminal-bench/tests/test_ab_scripts.py
```

It runs `bash -n` on every `ab/*.sh`, compiles every `ab/*.py` and the task
set helper, checks every file under `ab/tasks/`, and refuses a host address, a
key prefix or the old host-only import path in any file under `ab/`. It drives
`radian_system.py` with a stub agent to prove it appends `--system <quoted
text>` when the file is set and renders the command unchanged when it is not.
It also drives `ab.sh`, `ab-system.sh` and `ab-pi.sh` with a stub `harbor` on
`PATH` and a temporary `HOME`, checking the `-i` list, `-k`, `<label>/tasks.txt`,
the `done` lines, the refusals, and the `ab-pi.sh` saved-list rule.
