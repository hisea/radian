# Benchmarking Radian on Terminal-Bench (Harbor)

This directory is Radian's Terminal-Bench adapter: a Harbor custom agent that
puts the current commit's `radian` binary into task containers and scores it.
It is **not** part of the Rust workspace and adds no Python dependency beyond
Harbor itself (the adapter and its command tests are stdlib-only).

Pinned Harbor release: **0.24.0** (see `pyproject.toml`
`[tool.radian-bench]`). The adapter speaks the 0.24.0 agent API
(`harbor.agents.model_connection`, `harbor.agents.installed.base`); newer
Harbor releases may rename these modules.

## Files

| File | Description |
|---|---|
| `radian_agent.py` | Harbor agent: uploads the binary + CA bundle, writes `auth.json`, runs `radian exec --output jsonl -p <instruction>` |
| `radian_command.py` | Stdlib-only command builders (provider selection, `auth.json` rendering, static-binary check, shell quoting, redaction) |
| `build-static.sh` | Builds the static musl binary the task containers need |
| `tests/test_radian_command.py` | Builder unit tests (no Harbor dependency) |
| `tests/test_radian_agent.py` | Agent unit tests against the real Harbor 0.24.0 package |
| `pyproject.toml` | Descriptive metadata only (agent import path, Harbor pin) |

## Key design (why the score measures the agent loop)

- **The score measures the agent loop, not our sandbox.** Default
  `--profile full`: the task container is already the isolation boundary, so a
  nested sandbox would only measure whether the host allows nested sandboxing.
  Radian's own sandbox has its own escape-matrix tests in
  `crates/radian-sandbox`.
- **The key only ever lives in `$RADIAN_BENCH_API_KEY`.** Harbor resolves the
  trial key from that variable (`MODEL_CONNECTION` lists it in
  `api_key_envs`, tried before Harbor's own provider defaults, so it works for
  any provider slug including `opencode-go`). The command line and the uploaded
  `auth.json` name the variable; neither ever holds the value (ADR 0013).
- **Provider routing.** A model name whose prefix is a radian built-in
  descriptor id (`opencode-go`, `anthropic`, `openai`, `openai-compatible`,
  `chatgpt` — `crates/radian-host/src/provider.rs`) runs as
  `radian exec --provider <id> --model <rest>` with no `--base-url`, and
  `auth.json` gets `{"<id>": {"type": "api_key", "key": "$RADIAN_BENCH_API_KEY"}}`.
  An explicitly configured endpoint (a `*_BASE_URL` trial env var, e.g. a
  proxy) always runs as the generic `openai-compatible` provider with
  `--base-url`, including proxied Anthropic traffic — radian's `anthropic`
  provider takes no `--base-url` (ADR 0026 D0). Note `chatgpt` is OAuth-only,
  so an `api_key` entry cannot log into it; the prefix still routes there and
  radian reports the credential error.
- **CA bundle.** Task images based on `ubuntu:24.04` / debian slim often ship
  no system CA certificates, and then radian's HTTP client fails before the
  first request. `install()` uploads the host bundle (option `ca_bundle_path`,
  default `/etc/ssl/certs/ca-certificates.crt`) next to the binary and refuses
  a missing bundle with a clear error. `run()` probes the container with
  `test -s /etc/ssl/certs/ca-certificates.crt` (present **and** non-empty: an
  empty placeholder breaks TLS exactly like a missing file) and sets
  `SSL_CERT_FILE` to the uploaded bundle only when the probe fails. The
  variable is process environment, so radian's child commands (the shell tool,
  `curl` in tasks) inherit it while it is set.
- **Unattended.** Every trial runs
  `--yes --no-trust --offline-catalog`; `auth.json` is 0600 under a 0700
  `RADIAN_HOME`; sessions and stdout/stderr land in the agent logs dir
  (`radian-sessions/`).
- **Usage reporting.** After the logs sync back, `populate_context_post_run`
  sums the trial's `radian-sessions/*.jsonl` — each call's
  `ModelUsageRecorded` plus the summarizer's `CompactionUsageRecorded` — into
  Harbor's token/cost fields, with fee micros converted to USD. A counter no
  record reported stays `None` ("not reported", never 0). `run()` deliberately
  leaves the context empty: Harbor only calls the backfill while it is empty.
- **Outcome reporting.** Every trial runs `--output jsonl`, so stdout ends
  with a `Result` record (ADR 0044): `run()` reads its `exit_code` and
  `stop_reason` into the trial facts and the backfilled metadata, instead of
  inferring the outcome from the exit code alone (turn outcomes that used to
  share exit 1 — failed, over budget, unfinished — now report 1/3/4 with
  their reason named).
- **auth.json ownership.** `auth.json` goes up through Harbor's
  `_upload_config_text`, which chowns it to the container's default agent user
  (plus 0600): a 0600 root-owned file would be unreadable to radian under a
  non-root agent user. It still holds only the `$RADIAN_BENCH_API_KEY`
  reference, never the value.
- **Turn timeout.** An explicit `--turn-timeout-secs` always wins; otherwise,
  when the adapter-side exec timeout (`timeout_sec`) is set, the turn timeout
  is that deadline minus 60 s (floor 60), so radian wraps up and writes its
  journal before the kill. The task's own agent timeout cannot feed this:
  Harbor applies it as an external wait around `run()` and never passes it to
  the agent object.
- **Reproducible.** `install()` refuses a dynamically linked binary (pure-Python
  ELF `PT_INTERP` check — the task images cannot satisfy a glibc loader) and
  records the binary sha256 plus `radian exec --version` in
  `radian-binary.json`. A fixed `--context-window` skips the models.dev
  lookup.

## Unit tests (no Harbor run, no model, no Docker)

```sh
python3 benchmarks/terminal-bench/tests/test_radian_command.py   # stdlib only
<home>/.local/share/uv/tools/harbor/bin/python benchmarks/terminal-bench/tests/test_radian_command.py
<home>/.local/share/uv/tools/harbor/bin/python benchmarks/terminal-bench/tests/test_radian_agent.py
PYTHONPATH=benchmarks/terminal-bench <home>/.local/share/uv/tools/harbor/bin/python -c \
  "from harbor.utils.import_path import import_class; print(import_class('radian_agent:RadianAgent'))"
```

The agent tests import the real Harbor 0.24.0 package; the fake environment is
only a process double for container exec/upload. Trial orchestration is proven
by the smoke below, not by these tests.

## Smoke (needs Docker + Harbor + a model key)

```sh
# 1. Build the static Linux binary (task containers are Linux; the host glibc
#    does not fit them). Prints the binary path and sha256; fails unless
#    `file` reports a static link.
benchmarks/terminal-bench/build-static.sh
BIN=~/.cache/radian-musl/target/release/radian
SHA=$(cut -d' ' -f1 "$BIN.sha256")

# 2. Install this box's Harbor release (pinned: 0.24.0).
export PATH="$HOME/.local/bin:$PATH"
uv tool install harbor==0.24.0

# 3. Export the trial key WITHOUT printing it (never echo/env/set -x it).
#    The smoke uses the opencode-go key with model deepseek-v4.1-flash.
export RADIAN_BENCH_API_KEY="$(python3 -c 'import json,os;print(json.load(open(os.path.expanduser("~/.radian/credentials/auth.json")))["opencode-go"]["key"])')"

# 4. Run 3-5 easy tasks (repeat -i per task, -n 2 at most). Harbor resolves the
#    key from the exported variable; PYTHONPATH puts radian_agent on the import path.
PYTHONPATH=benchmarks/terminal-bench timeout 5400 harbor run \
  -d terminal-bench@2.0 \
  -i fix-git -i prove-plus-comm -i constraints-scheduling \
  -a radian_agent:RadianAgent \
  -m opencode-go/deepseek-v4.1-flash \
  --ak binary_path="$BIN" --ak binary_sha256="$SHA" \
  -n 2 -o /tmp/tbench-smoke/jobs -y
```

After the run, confirm the key leaked nowhere (this prints only a count, never
the key):

```sh
grep -rlF -- "$RADIAN_BENCH_API_KEY" /tmp/tbench-smoke/jobs | wc -l   # must be 0
```

Results land under the jobs dir (`-o`): per-trial `result.json` carries the
reward (0/1 — task pass rate does not matter for the smoke, only that the
verifier ran and recorded one), the agent logs carry `radian-binary.json`,
`radian-stdout.txt`, `radian-stderr.txt` and `radian-sessions/` (radian's
native JSONL journal — read that when a trial needs debugging, not the
trajectory converters). Success means the infrastructure works on every chosen
task: binary uploaded and runs, key reaches radian, radian talks to
opencode-go, the verifier runs and a reward is recorded, with no agent
setup/exec exception. Record per task: name, base image, reward, radian exit
code, model calls, wall time, and any error.
