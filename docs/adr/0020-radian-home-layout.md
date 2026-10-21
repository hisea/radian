# ADR 0020: Radian Home Layout

Status: accepted

## Decisions

### 1. One root directory

macOS and Linux both use `$HOME/.radian/`. No separate system-data and cache directories exist. No `XDG_*` reads exist. Environment variable `RADIAN_HOME` can override the whole root (for tests and special deployments; must be an absolute path). `--session-dir` and `RADIAN_SESSION_DIR` still override only the session directory.

The directory carries mode 0700 and files carry 0600 on first use (ADR 0013 permission rules).

### 2. Layout

```text
~/.radian/                 0700
  config/config.toml       configuration (ADR 0008)                  sandbox-readable
  cache/                   0700
    catalog/models.json    models.dev catalog cache (ADR 0018)       sandbox-readable
    diagnostics/           panic logs and similar (ADR 0013)         sandbox-readable
  sessions/                session JSONL and index.db (ADR 0006)     sandbox-unreadable
  memory/                  memory store (ADR 0021)                   sandbox-unreadable
  credentials/auth.json    credentials (ADR 0026)                    sandbox-unreadable
  trust/trust.json         project trust records (ADR 0031)          sandbox-unreadable
  themes/<name>.toml       theme files (ADR 0029)                    sandbox-readable
  skills/<name>/SKILL.md   user-level skills (ADR 0025)              sandbox-readable
  access/access.json       per-project tier records (ADR 0038)       sandbox-readable
  run/<pid>.sock           control socket (ADR 0043, non-task verbs) sandbox-readable
```

`RADIAN_HOME` override moves all subdirectories with it. `--session-dir` moves only the session directory.

### 3. Sandbox read scope

- **Readable**: `config/`, `cache/`, `themes/`, `skills/`, `access/`, `run/`. These hold user UI data, model-read text, or process coordinates. They carry no credentials. `$HOME/.agents/skills/` (Agent Skills standard location, ADR 0025) lives outside `$RADIAN_HOME` and outside the denylist, so the sandbox can read it. It never joins the layout tree above.
- **Unreadable**: `sessions/`, `memory/`, `credentials/`, `trust/`, plus the actual session directory that `--session-dir` or `RADIAN_SESSION_DIR` points at. Only Radian own tools and commands reach them.
- The `full` tier has no sandbox and ignores this scope (chosen explicitly by the user).

The host first creates `sessions/`, `memory/`, `credentials/`, and `trust/` (0700), then hides them. Linux bwrap can only hide existing paths. `full` has no sandbox.

### 4. Sandbox temp directory inside the workspace

The sandbox creates `.radian-sandbox/` inside the workspace (macOS `TMPDIR`, Linux `.radian-sandbox/home`). Under `$HOME` it never mixes with `~/.radian/`. `.gitignore` syncs. Discovery logic ignores it.

### 5. Old locations

Data at old locations (`~/Library/Application Support/radian/sessions`, `$XDG_DATA_HOME/radian/sessions`) **never moves automatically**: when startup finds `.jsonl` at an old location but none in `~/.radian/sessions/`, it prints one copyable `mv` command. It never moves user data itself (never touches user data unasked). Old catalog-cache and diagnostics paths likewise stay unread.

### 6. Project-level directories

`<workspace>/.radian/` holds only `config.toml` (ADR 0008 project-level narrow configuration; its narrowing applies regardless of trust). Project-level skills live in `.agents/skills/` directories (ADR 0025; from the workspace up to the repository root, loaded only when trusted). They are ordinary files inside the workspace. The `workspace`-tier sandbox reads them as usual. The denylist never lists them separately. `.radian-sandbox/` is a sandbox temp directory inside the workspace, next to `.radian/`. It is not a project resource.

## Verification

`radian-host::home` pure-function tests cover `RADIAN_HOME` override, the default `$HOME/.radian`, and each subdirectory name. CLI tests cover sessions under `RADIAN_HOME` landing in `sessions/`, `RADIAN_SESSION_DIR` priority, and the old-location one-line notice without move. The `radian-sandbox` `radian_home` escape matrix covers `sessions`, `memory`, `credentials`, and `trust` unreadable plus `config`, `cache`, `themes`, and `skills` readable, plus a moved session directory unreadable.
