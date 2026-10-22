---
description: Every environment variable radian reads, and what each controls.
---

# Environment

Radian reads the variables below.
A command flag always wins over a variable; a variable always wins over the configuration file; the file always wins over a built-in default.
`cli` names the flag for each row, `configuration` names the file key.

## Home and paths

| Variable | Allowed values | Default when unset | What it does |
|---|---|---|---|
| `RADIAN_HOME` | an absolute path only | `$HOME/.radian` | Overrides the whole `~/.radian` root. A relative, empty, or non-Unicode value counts as unset. Sessions, credentials, config, cache, and memory all move with it. |
| `HOME` | any directory | none | The base of the default root, of `~` expansion in file paths, and of `$HOME/.agents/skills` discovery. Anything needing a home refuses with a `no RADIAN_HOME or HOME` error when both are missing or empty. |
| `RADIAN_CONFIG` | any file path | `<root>/config/config.toml` (`$RADIAN_HOME` or `$HOME` root) | The configuration file path. Below `--config`, above the default file. An empty value counts as unset. An explicit path here is never created on first start, and a missing file is an empty configuration. |
| `RADIAN_SESSION_DIR` | any directory path | `<root>/sessions` (`$RADIAN_HOME` or `$HOME` root) | The session directory. Below `--session-dir`, above the default. Empty counts as unset. With no home either, radian refuses and tells you to pass `--session-dir`. |
| `PATH` | any value | inherited | Radian never resolves its own tools through it. The value passes to sandboxed children as part of the fixed passthrough list and to MCP servers. |

## Credentials

| Variable | Allowed values | Default when unset | What it does |
|---|---|---|---|
| `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` | any secret text | none | Conventional key names. Store `$NAME` or `${NAME}` as the `auth.json` key value and radian resolves it at runtime: the entry `env` object first, then the process environment. Any valid environment name works, not just these two. A missing variable is an error naming the provider and the name. Radian reads exactly the names your entries reference; anything else in the environment is ignored. |
| `RADIAN_BASE_URL`, `RADIAN_KIND` | URL text; `openai-compatible` or `anthropic` | none | Fields of an `auth.json` entry `env` object, written by `login` for custom endpoints. They select the endpoint and the wire protocol for a named provider. Radian never reads these names from the process environment. |

Keys come only from `auth.json`: there is no key flag and no key file field.
`!command` key values are refused and never executed.
See `providers` for login and routing.

## Catalog and model

| Variable | Allowed values | Default when unset | What it does |
|---|---|---|---|
| `RADIAN_CATALOG_URL` | any URL | the bundled catalog URL | Overrides the models.dev catalog URL. Below `--offline-catalog` (which skips the fetch) and `--catalog-provider` (which picks the lookup name). The cache lives 24 hours and the fetch times out after 3 seconds. |
| `SHELL` | any program | `sh` | Reported to the model as a session fact (`shell:`). Nothing else reads it. |
| `HOSTNAME` | any non-empty name | the output of `hostname` | The footer host name when set and non-empty. |

## Terminal

| Variable | Allowed values | Default when unset | What it does |
|---|---|---|---|
| `NO_COLOR` | any non-empty value turns it on; empty counts as unset | unset | A non-empty value forces the `mono` theme. Wins over the file and over `/theme`. |
| `COLORTERM` | `truecolor` or `24bit` turns on truecolor; any other value stays in 256 colors | unset | Truecolor rendering switch. |
| `RADIAN_KEYBOARD_ENHANCEMENT` | `0` or `false` never probes the terminal; `1` or `true` pushes the enhancement flags without asking | unset: ask the terminal | Terminal capability probing. Unset: radian asks the terminal (up to a 2 second wait on a terminal that never answers). Any other value counts as unset. |

## Sandboxed children

Sandboxed commands start from a fixed passthrough list (`PATH`, `HOME`, `LANG`, `TERM`, `TMPDIR`, `USER`, `LOGNAME`, `SSL_CERT_FILE`, `SSL_CERT_DIR`, the six `HERDR_*` names) plus the `[sandbox] allow_env` names.
Everything else, including every key, is removed.

| Variable | Allowed values | Default when unset | What it does |
|---|---|---|---|
| `RADIAN_SESSION_ID`, `RADIAN_CWD` | set by radian, never read back | — | Set on footer `command` contributors: the session id and the workspace path. Radian itself never reads them back. |
| `RADIAN_CHILD_HEADERS` | JSON array of `NAME=VALUE`, set by the parent only | empty | Internal. Carries a parent `--header` values to a `subagent` child. Read and removed from the environment at startup. Never set it by hand. |

## Herdr

| Variable | Allowed values | Default when unset | What it does |
|---|---|---|---|
| `HERDR_ENV` | `1` means this process runs inside a herdr pane; any other value disables | unset: disabled | Whether this process reports to herdr. Set by herdr, not by you. |
| `HERDR_PANE_ID`, `HERDR_SOCKET_PATH` | any non-empty value; both are required | unset: no reporting | The pane coordinates reporting uses. Set by herdr. |
| `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`, `HERDR_BIN_PATH` | any value | unset | Pane coordinates that only pass through to sandboxed children. |
| `RADIAN_HERDR` | `0` turns reporting off for one run; any other value is a no-op | unset: report when the pane variables are present | Your switch. `config.integrations.herdr = false` turns it off too. |

## Child processes you start

MCP servers and hook commands receive the environment their own rules describe, not this table.
MCP servers inherit `HOME` by default plus the `inherit_env` names; `${VAR}` and `${VAR:-default}` in `command`, `args`, `cwd`, and `env` expand from the process environment.
Hook commands run under the session sandbox when one is enforced.
See `mcp` and `configuration` for those rules.

## Files that change behavior

| Path | What changes it |
|---|---|
| `~/.radian/config/config.toml` (or `$RADIAN_HOME`, `--config`, `RADIAN_CONFIG`) | Every configuration key. See `configuration`. A missing file is an empty configuration. |
| `~/.radian/credentials/auth.json` | Every stored key. A missing entry refuses the run and points at `radian login`. The file is `0600`; wide permissions warn. |
| `~/.radian/trust/trust.json` | Your remembered per-project trust choices. Confirming the trust prompt writes it. |
| `~/.radian/sessions/` (or `--session-dir`, `RADIAN_SESSION_DIR`) | The session files `--resume`, `--list`, `--search`, `--export`, `--fork`, and `attach` read. |
| `<workspace>/.radian/config.toml` | May only narrow the tier (`[permissions] profile = "workspace"`). Wider values, `[sandbox]` keys, and other keys are refused or skipped by name. |
| `<workspace>/.mcp.json` | Project MCP servers. Loads only when you trust the project; `--trust` or `--no-trust` overrides for one run. Credential variables are blanked wherever they appear in it. |
| `~/.radian/themes/` | Theme files `tui.theme` names. Missing names warn and fall back to `dark`. |
| `<repository>/.radian/arc/config.toml` | arc's settings: the remote, the main branch, the limits, and the gate profiles. Written by `radian arc init`, which keeps an existing file. See `arc`. |
| `<repository>/.radian/arc/plans/`, `tasks/` | The plan and task files every arc subcommand reads. You change them through `radian arc`, or with `radian arc adopt` after a hand edit. See `arc`. |
