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
- **A comparison measures the parameters only if the arms send the same ones.**
  `AB_CAPTURE=1` records what each arm actually sent (see
  [Request capture](#request-capture)) and `capture_check.py` names any field
  that differs, instead of trusting a reading of the code.

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
| `ab.sh` | The selected task set (default `core-2026-10`): each radian binary named on the command line, plus pi 1.4.0 and opencode 1.19.4, all started at once. Samples load. | `<label> <radian-bin-name>...` |
| `ab-pi.sh` | Re-runs only the pi arm of an existing label, against that label's saved task set. Moves a previous `pi/` and `pi.log` to `pi.failed-start*` first. | `<label>` |
| `ab-system.sh` | One radian binary twice: the built-in role answer versus a role file passed as `--system`. Copies the role file to `<label>/role.txt`. Samples load. | `<label> <radian-bin-name> <role-file>` |
| `run.sh` | One radian binary at `RADIAN_BENCH_BINARY`, pi and opencode, all started at once, with `-n 4` each. Extra arguments go to Harbor (for example `-i fix-git`). | `<label> [harbor args...]` |
| `audit_images.sh` | Inspects every cached Terminal-Bench task image for answer files and git history. Writes its report to stdout. | none |
| `abstat.py` | Per-arm counts from one label: started, finished, pass, timeouts, agent minutes. | `<arm-dir>...` (for example `$HOME/tb2/jobs/<label>/pi`) |
| `scan_leaks.py` | Scans each trial's agent logs for references to the benchmark sources, solution or test files. Exits 1 on any hit. | `<jobs-dir>...` |
| `radian_system.py` | Harbor agent: `radian_agent.RadianAgent` with `--system` added from `RADIAN_AB_SYSTEM_FILE`. | imported by Harbor, not run directly |
| `opencode_go.py` | Harbor agent: the stock opencode agent with `OPENCODE_API_KEY` passed through, and no `nodejs`/`npm` install on glibc images. | imported by Harbor, not run directly |
| `pi_opencode_go.py` | Harbor agent: the stock pi agent with `OPENCODE_API_KEY` passed through. | imported by Harbor, not run directly |
| `capture.sh` | Proxy bookkeeping for the request capture: one `mitmdump` per arm, its CA file, and stopping it again. Sourced by the three scripts, inert while `AB_CAPTURE` is unset. | sourced, not run directly |
| `capture_addon.py` | The mitmproxy addon: appends one line per model request to `$AB_CAPTURE_OUT`. | run by `mitmdump -s`, not directly |
| `capture.py` | The mixin the three agents inherit: uploads the arm's CA, trusts it, adds the proxy variables, writes `capture.json`. | imported by the agents, not run directly |
| `capture_check.py` | What each arm of a capture run actually sent, per field and per trial. Exits 1 when an expected-equal field differs. | `<label-dir> [--expect-equal f1,f2,...]` |

Each agent arm uses the model `opencode-go/deepseek-v4.1-flash` with thinking
high (opencode: variant high, web search off). A radian arm also runs with
`--profile full` inside the task container.

## Request capture

An A/B only measures the agents if the arms send the same model parameters. The
capture records what each arm actually sent, so that claim is checked instead of
assumed. It changes no agent configuration: the provider, endpoint, model name
and the parameters on the command line are untouched, and the request is the
same one the arm would send anyway — it only goes through a local proxy first.

`AB_CAPTURE=1` makes `smoke.sh`, `ab.sh` and `ab-system.sh`

1. refuse to start unless `mitmdump` is on `PATH` (one line on stderr);
2. start one `mitmdump --mode regular --listen-host 0.0.0.0 --listen-port <port>
   -s capture_addon.py --set confdir=<label>/capture/<arm>/confdir` per arm, in
   arm order, from `AB_CAPTURE_PORT`;
3. wait for that proxy's CA file before starting the arm;
4. pass `--ak capture_port=<port> --ak capture_ca=<host path>` to that arm's
   Harbor agent;
5. stop every proxy after the last arm, and again on the way out (a failed arm
   included).

Inside the container the agent's mixin (`capture.py`) uploads the CA to
`/tmp/harbor-ab-capture/ca.pem`, copies the image's own bundle with the CA
appended to `/tmp/harbor-ab-capture/ca-bundle.crt`, and registers the CA with
`update-ca-certificates` when the image has that tool. The proxy is the
container's default gateway (`/proc/net/route`; `ip` is not installed in every
task image) on the arm's port. `NO_PROXY=localhost,127.0.0.1` keeps the
container's own services off the proxy; everything else in that container's
`HTTP(S)_PROXY` environment, the task's own `curl` calls included, goes through
mitmproxy and is forwarded unchanged (only model requests are recorded).

One capture line per model request is appended to `<label>/capture/<arm>.jsonl`
(any request whose path ends in `/chat/completions`, `/responses` or
`/messages`) and holds:

- `ts`, `arm`, `client_ip`, `method`, `host`, `path`, `status` (null when the
  request failed before a response);
- from the JSON body only: `model`, `stream`, every top-level key whose name
  contains `reason` or `think` (`reasoning_effort`, `thinking`, `reasoning`, ...)
  with its value, `max_tokens`, `max_completion_tokens`, `max_output_tokens`,
  `temperature`, `top_p`, `stream_options` (a field the body does not send is
  `null`, so "one arm sends it" shows up as a differing value);
- `tools`: count, the compact-JSON character count of the array, how many have
  `strict: true`, and the sorted names;
- `system`: the character count and the first 12 hex of the SHA-256 of the
  system text (a `system` role message, an `instructions` field or a top-level
  `system`, each as a string or as a list of text blocks) — never the text;
- `messages`: count, how many are assistant turns, how many of those carry
  `reasoning_content` or `reasoning`;
- `body_bytes`.

No header value, no API key and no message, tool-schema or system text is ever
written. A request from a client outside `AB_CAPTURE_CLIENTS` is refused with a
403 and writes no line.

`capture.py` also writes `<trial agent log dir>/capture.json` with the
container's own IP (`/proc/net/fib_trie`: the first non-loopback local address,
preferring the trie block that contains the gateway), the gateway and the port,
which is how a captured `client_ip` is mapped back to the trial that sent it.

### Which variable each client reads

| Agent | Proxy | CA trust |
|---|---|---|
| radian | `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY` — verified: every provider client is built with reqwest 0.13.5 (`Cargo.toml`/`Cargo.lock`) with the `system-proxy` feature and no `.proxy()`/`.no_proxy()` call, so reqwest adds the system matcher, which reads those three (`hyper-util` `ProxyMatcher::from_env`) | `SSL_CERT_FILE` — verified: reqwest's `rustls` feature verifies through `rustls-platform-verifier` 0.7.1, which on Linux loads `rustls-native-certs` 0.8.4, which reads `SSL_CERT_FILE` (and replaces the system store with it) |
| pi 1.4.0 | `NODE_USE_ENV_PROXY=1` plus `HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` — Node reads them (documented since v22.21.0, also v24); Harbor installs pi's Node through `nvm install 22`, so the current 22.x. Whether pi's own HTTP client is Node's global `fetch` (what the variable patches) is **unverified**: pi's source is not in this repository. An image whose Node is older than 22.21 ignores the variable | `NODE_EXTRA_CA_CERTS` — Node reads it; that pi's client honours it is **unverified** |
| opencode 1.19.4 | `HTTPS_PROXY`, `HTTP_PROXY`, `NO_PROXY` — verified in Bun's `fetch` documentation ("Without a proxy option, Bun reads `HTTP_PROXY`, `HTTPS_PROXY` and `ALL_PROXY`"); the npm package installs a native Bun build, so `NODE_USE_ENV_PROXY` is inert for it (that variable is Node's) | **unverified**: Bun's documentation covers neither `NODE_EXTRA_CA_CERTS` nor `SSL_CERT_FILE` here. `update-ca-certificates` puts the CA in the system store, which is the path a Linux client normally reads, and the CA bundle is exported through `SSL_CERT_FILE` for every client that reads it |

What proves the capture arrived is the data, not this table: an arm whose
`capture/<arm>.jsonl` stays empty, or trials without a `capture.json`, are the
signal that a client ignored the variables. The live run on a Linux host is the
only place that is checked.

### Reading a capture

```sh
python3 benchmarks/terminal-bench/ab/capture_check.py $HOME/tb2/jobs/<label>
```

Per arm it prints the number of requests and, for every captured field, the
distinct values with counts — once for the first request of each trial and once
for all requests — with every `capture.json` IP mapped to its trial. Then a
`DIFFERENCES` block for the `--expect-equal` fields (default
`model,reasoning_effort,thinking,max_tokens,stream_options`): each field whose
value set is not the same in every arm. An arm whose proxy was ignored captured
no request, so its empty value set differs from an arm that sent values. Exit 1
means at least one such field, or that no arm captured a request at all; exit 0
means every arm sent the same values; exit 2 means the label has no capture to
read.

For an `ab-system.sh` label a reader passes every field but `system` — the
arms differ in exactly that one input. `messages`, `tools` and `body_bytes`
follow the conversation, so they differ as soon as the two arms take different
paths; the `first request of each trial` block is where the arms stay
comparable, and it is printed for exactly that reason.

```sh
python3 benchmarks/terminal-bench/ab/capture_check.py $HOME/tb2/jobs/<label> \
  --expect-equal model,reasoning_effort,thinking,max_tokens,stream_options,messages
```

`mitmdump` is a host requirement of `AB_CAPTURE` only: `uv tool install
mitmproxy` (or any install that puts `mitmdump` on `PATH`). Nothing here imports
mitmproxy; the addon is a script that `mitmdump` runs.

## Choosing the tasks

`tasks/` holds one file per task set. A file has one Terminal-Bench task name
per line; a `#` starts a comment that runs to the end of the line, and blank
lines are ignored. A task name matches `^[a-z0-9][a-z0-9-]*$`; a set may not be
empty, and it may not repeat a name.

| File | What it holds |
|---|---|
| `tasks/core-2026-10.txt` | The default set, chosen from the cal1 calibration run: the first five tasks separated the agents' pass rate, and the last five are tasks every agent passed with a stable generated-token gap, for reasoning-volume comparisons. |
| `tasks/ab3.txt` | The ten tasks that ab1, ab3 and absys1 ran, in their order. Kept unchanged so those labels stay reproducible. |
| `tasks/calibrate-2026-10.txt` | The five tasks that separated the agents, plus ten medium-difficulty candidates, for the cal1 calibration run. |

`AB_TASKS` selects the set for `ab.sh`, `ab-system.sh` and `ab-pi.sh`:

- A bare name (no `/`) means `tasks/<name>.txt` next to the scripts, so
  `AB_TASKS=core-2026-10` and the default `core-2026-10` are the same file.
- A value with a `/` is a path to a task set file, absolute or relative to the
  directory the script runs in.

The default is `core-2026-10`. A run meant for a decision uses `AB_K=5`. An
unknown bare name, a missing path, a set with no task names, a duplicate name,
or a name that does not match the pattern stops the script before Harbor starts,
with one line on stderr naming the problem. One shared helper, `task_set.py`,
resolves and validates the list for all three scripts.

Each of the three scripts writes the resolved list it used to
`<label>/tasks.txt`, one task name per line, with no comments. `ab-pi.sh` is the
exception to selection: when `<label>/tasks.txt` already exists it re-runs that
label's list. If `AB_TASKS` is also set, it must resolve to the same list (the
same task names in the same order) or `ab-pi.sh` refuses to run, so one label
never mixes sets. Without the file, `ab-pi.sh` resolves `AB_TASKS` (or
`core-2026-10`) and writes it.

## Environment variables

Every script also reads `HOME` (the `$HOME/tb2` root) and the shell scripts
prepend `$HOME/.local/bin` to `PATH` so `harbor` resolves.

| Variable | Read by | Default | Effect |
|---|---|---|---|
| `AB_TASKS` | `ab.sh`, `ab-system.sh`, `ab-pi.sh` | `core-2026-10` | The task set: a bare name under `tasks/`, or a path to a file. See [Choosing the tasks](#choosing-the-tasks) for the format and the refusals. |
| `AB_N` | `ab.sh`, `ab-system.sh`, `ab-pi.sh` | `2` in `ab.sh` and `ab-pi.sh`; `3` in `ab-system.sh` | Passed as `-n` (trials per arm). `run.sh` ignores it and passes `-n 4`; `smoke.sh` passes `-n 1`. |
| `AB_K` | `ab.sh`, `ab-system.sh`, `ab-pi.sh` | `2` in `ab.sh` and `ab-pi.sh`; `3` in `ab-system.sh` | Passed as `-k`. `run.sh` and `smoke.sh` pass no `-k` and use Harbor's default. |
| `RADIAN_AB_SYSTEM_FILE` | `radian_system.py` | unset | Path to a UTF-8 role file. When set, the agent appends `--system <quoted text>` to the command; when unset, the command is unchanged. `ab-system.sh` sets it to `<label>/role.txt` for its concise arm and unsets it for its base arm, so a value exported by the caller cannot silently give both arms a `--system`. |
| `RADIAN_BENCH_BINARY` | `run.sh` | `$HOME/.cache/radian-musl/target/release/radian` | Host path of the static radian binary under test; its `.sha256` sidecar must exist beside it. |
| `AB_CAPTURE` | `smoke.sh`, `ab.sh`, `ab-system.sh` | unset | Any non-empty value records every arm's model requests through a local `mitmdump`. Unset: the scripts run exactly the commands they have always run (no proxy, no extra `--ak` argument). See [Request capture](#request-capture). |
| `AB_CAPTURE_PORT` | `smoke.sh`, `ab.sh`, `ab-system.sh` | `18080` | Port of the first arm's proxy; each further arm takes the next port, in arm order. `smoke.sh` has one arm. Read only when `AB_CAPTURE` is set. |
| `AB_CAPTURE_CLIENTS` | `capture_addon.py` | `172.16.0.0/12` | Comma-separated CIDR list of clients the proxy records; a request from any other address gets a 403 and no line. Read by the addon at start-up, so the scripts need no change. |
| `AB_CAPTURE_OUT` | `capture_addon.py` | set by the scripts | The file the addon appends its lines to; the scripts set it per arm to `<label>/capture/<arm>.jsonl`. The arm name in each line is the file's stem. Not an input: the scripts always set it, and a proxy started without it fails at start-up. |

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
  capture/        # AB_CAPTURE only: <arm>.jsonl (one line per model request), <arm>/confdir/ (that arm's proxy state and CA), <arm>/mitmdump.log
  <arm>/          # Harbor output for that arm (one directory per binary name, pi, opencode)
  <arm>.log       # Harbor stdout and stderr for that arm
  role.txt        # ab-system.sh only: the role file handed to the concise arm
```

`capture.json`, which maps a captured `client_ip` to its trial, is written into
each trial's agent log dir under `<arm>/`, next to `radian-binary.json` for a
radian arm.

`<arm>` is the binary name given to `ab.sh`/`ab-system.sh`, or `pi`, `opencode`,
`radian`, `base` or `concise`. `done` is append-only, so a relaunch such as
`ab-pi.sh` adds a second `pi` line and the first one stays as history.

## Test

The scripts' checks need no Harbor, Docker or model:

```sh
python3 benchmarks/terminal-bench/tests/test_ab_scripts.py
python3 benchmarks/terminal-bench/tests/test_ab_capture.py
python3 benchmarks/terminal-bench/tests/test_ab_capture_scripts.py
```

`test_ab_scripts.py` runs `bash -n` on every `ab/*.sh`, compiles every `ab/*.py`
and the task set helper, checks every file under `ab/tasks/`, and refuses a host
address, a key prefix or the old host-only import path in any file under `ab/`.
It drives `radian_system.py` with a stub agent to prove it appends `--system
<quoted text>` when the file is set and renders the command unchanged when it is
not. It also drives `ab.sh`, `ab-system.sh` and `ab-pi.sh` with a stub `harbor`
on `PATH` and a temporary `HOME`, checking the `-i` list, `-k`, `<label>/tasks.txt`,
the `done` lines, the refusals, and the `ab-pi.sh` saved-list rule.

`test_ab_capture.py` checks the addon's summary against three fixture bodies, the
client allow list, the mixin against a fake container, and `capture_check.py` on
the fixture label under `tests/fixtures/capture/`. Two groups skip instead of
failing: the 403 hook test needs `mitmproxy`, and the tests that drive the real
agents need Harbor's Python (like `tests/test_radian_agent.py`):

```sh
<home>/.local/share/uv/tools/harbor/bin/python benchmarks/terminal-bench/tests/test_ab_capture.py
```

`test_ab_capture_scripts.py` runs the three scripts with a stub `harbor` and a
stub `mitmdump`: one proxy per arm on consecutive ports, each arm's own
`--ak capture_port`/`--ak capture_ca` pair, no proxy process left behind, and —
without `AB_CAPTURE` — the exact Harbor arguments the scripts produce today.
