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
| `ab.sh` | 10 fixed tasks: each radian binary named on the command line, plus pi 1.4.0 and opencode 1.19.4, all started at once. Samples load. | `<label> <radian-bin-name>...` |
| `ab-pi.sh` | Re-runs only the pi arm of an existing label. Moves a previous `pi/` and `pi.log` to `pi.failed-start*` first. | `<label>` |
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

## Environment variables

Every script also reads `HOME` (the `$HOME/tb2` root) and the shell scripts
prepend `$HOME/.local/bin` to `PATH` so `harbor` resolves.

| Variable | Read by | Default | Effect |
|---|---|---|---|
| `AB_N` | `ab.sh`, `ab-system.sh`, `ab-pi.sh` | `2` in `ab.sh` and `ab-pi.sh`; `3` in `ab-system.sh` | Passed as `-n` (trials per arm). `run.sh` ignores it and passes `-n 4`; `smoke.sh` passes `-n 1`. |
| `AB_K` | `ab-system.sh` | `3` | Passed as `-k`. `ab.sh` and `ab-pi.sh` pass a fixed `-k 2`; `run.sh` and `smoke.sh` pass no `-k` and use Harbor's default. |
| `RADIAN_AB_SYSTEM_FILE` | `radian_system.py` | unset | Path to a UTF-8 role file. When set, the agent appends `--system <quoted text>` to the command; when unset, the command is unchanged. `ab-system.sh` sets it to `<label>/role.txt` for its concise arm. |
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
  done            # one line per finished arm: "<arm> rc=<code>" (smoke: "rc=<code>")
  loadavg.log     # UTC timestamp plus 1/5/15-minute load, every 30 s (ab.sh, ab-system.sh)
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

It runs `bash -n` on every `ab/*.sh`, compiles every `ab/*.py`, refuses a host
address, a key prefix or the old host-only import path in any file under `ab/`,
and drives `radian_system.py` with a stub agent to prove it appends
`--system <quoted text>` when the file is set and renders the command unchanged
when it is not.
