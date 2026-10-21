---
description: Every environment variable radian reads, and what each controls.
---

# Environment

Radian reads the variables below.
A command flag always wins over a variable; a variable always wins over the configuration file; the file always wins over a built-in default.
`cli` names the flag for each row, `configuration` names the file key.

## Home and paths

| Variable | What it does |
|---|---|
| `RADIAN_HOME` | Overrides the whole `~/.radian` root. Only an absolute path counts. Unset or relative: `HOME/.radian` is the root. Sessions, credentials, config, cache, and memory all move with it. |
| `HOME` | The base of the default root, of `~` expansion in file paths, and of `$HOME/.agents/skills` discovery. Unset or empty: anything needing a home refuses with a `no RADIAN_HOME or HOME` error. |
| `RADIAN_CONFIG` | The configuration file path. Below `--config`, above the default file. An explicit path here is never created on first start. |
| `RADIAN_SESSION_DIR` | The session directory. Below `--session-dir`, above the default. Empty counts as unset. |
| `PATH` | Never read for lookup. Passed to sandboxed children as part of the fixed passthrough list, gated by `[sandbox] allow_env` rules. |

## Credentials

| Variable | What it does |
|---|---|
| `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` | Conventional key names. Store `$NAME` or `${NAME}` as the `auth.json` key value and radian resolves it at runtime: the entry `env` object first, then the process environment. Any valid environment name works, not just these two. A missing variable is an error naming the provider and the name. |
| `RADIAN_BASE_URL`, `RADIAN_KIND` | Fields of an `auth.json` entry `env` object, written by `login` for custom endpoints. They select the endpoint and the wire protocol for a named provider. Radian does not read them from the process environment. |

Keys come only from `auth.json`: there is no key flag and no key file field.
`!command` key values are refused and never executed.
See `providers` for login and routing.

## Catalog and model

| Variable | What it does |
|---|---|
| `RADIAN_CATALOG_URL` | Overrides the models.dev catalog URL. Below `--offline-catalog` (which skips the fetch) and `--catalog-provider` (which picks the lookup name). The cache lives 24 hours and the fetch times out after 3 seconds. |
| `SHELL` | Reported to the model as a session fact (`shell:`), or `sh` when unset. Nothing else reads it. |
| `HOSTNAME` | The footer host name when set and non-empty. Otherwise radian runs `hostname`. |

## Terminal

| Variable | What it does |
|---|---|
| `NO_COLOR` | A non-empty value forces the `mono` theme. Wins over the file and over `/theme`. |
| `COLORTERM` | `truecolor` or `24bit` turns on truecolor rendering. Any other value stays in 256 colors. |
| `RADIAN_KEYBOARD_ENHANCEMENT` | `0` or `false` never probes the terminal; `1` or `true` pushes the enhancement flags without asking. Unset: radian asks the terminal (up to a 2 second wait on a terminal that never answers). |

## Sandboxed children

Sandboxed commands start from a fixed passthrough list (`PATH`, `HOME`, `LANG`, `TERM`, `TMPDIR`, `USER`, `LOGNAME`, `SSL_CERT_FILE`, `SSL_CERT_DIR`, the six `HERDR_*` names) plus the `[sandbox] allow_env` names.
Everything else, including every key, is removed.

| Variable | What it does |
|---|---|
| `RADIAN_SESSION_ID`, `RADIAN_CWD` | Set on footer `command` contributors: the session id and the workspace path. Radian itself never reads them back. |
| `RADIAN_CHILD_HEADERS` | Internal. Carries a parent `--header` values to a `subagent` child. Removed from the environment at startup. Never set it by hand. |

## Herdr

| Variable | What it does |
|---|---|
| `HERDR_ENV` | `1` means this process runs inside a herdr pane. Anything else disables reporting. Set by herdr, not by you. |
| `HERDR_PANE_ID`, `HERDR_SOCKET_PATH` | The pane coordinates reporting uses. Both must be non-empty. Set by herdr. |
| `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`, `HERDR_BIN_PATH` | Pane coordinates that only pass through to sandboxed children. |
| `RADIAN_HERDR` | `0` turns reporting off for one run. |

## Diagnostics

| Variable | What it does |
|---|---|
| `RADIAN_SANDBOX_PROBE_FAIL` | `1` makes the sandbox probe fail on purpose. The run refuses instead of degrading. Exercises the refusal path. |
| `RADIAN_TURN_COMMIT_DELAY_MS` | Milliseconds between a turn's finish and its journal commit. Exercises fork timing. Has no effect on normal runs. |
| `RADIAN_TRACE_PHASES` | Set to measure startup phases. Prints a phase trace with timings. Has no effect on normal runs. |

## Child processes you start

MCP servers and hook commands receive the environment their own rules describe, not this table.
MCP servers inherit `HOME` by default plus the `inherit_env` names; `${VAR}` and `${VAR:-default}` in `command`, `args`, and `env` expand from the process environment.
Hook commands run under the session sandbox when one is enforced.
See `mcp` and `configuration` for those rules.
