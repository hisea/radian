# ADR 0046: `radian config import-claude` (Claude Code One-Shot Import)

Status: accepted

## Background

Radian owns two one-shot translation precedents (`radian config import-pi`, `radian theme import`): code translates deterministically, never through models. Claude Code private formats import only, and runtime takes only Radian own formats (same principle as ADR 0029 section 5). Mappings live only in import commands. Runtime never takes Claude formats.

## Decisions

The command is `radian config import-claude`, symmetric with `import-pi` (`ConfigCommand::ImportClaude`, same dispatch). The `radian` binary shares the same grammar through `radian_exec::ConfigCommand` (ADR 0032). Sources land at their own level. Explicit sources: `--claude-home <dir>` (user-level `~/.claude/` tree) with `--project <dir>` (project dir). Defaults never read. At least one is required. Unnamed user dirs never read.

### Mappings (only fidelity-safe ones)

User level (`--claude-home`):

- `commands/*.md` → `~/.agents/skills/<name>/SKILL.md`: frontmatter gains `name` (file name). Missing usable `description` fills a placeholder and names it in reports. Body `$ARGUMENTS` stays as-is (ADR 0025 S3).
- `skills/` → writes only one `[skills] extra_paths` row in `config.toml` pointing at it, never copies files. ADR 0025 S1 notes `~/.claude/skills` reads only when listed.
- `settings.json` `hooks` → `[hooks]` (ADR 0045 subset: `PreToolUse`/`PostToolUse`/`Stop`/`PermissionRequest` plus the observe-only `StopFailure`/`Notification`, `type = "command"`, no `if`/`http`/`prompt`/`agent`/`context: fork`; `async`/`asyncRewake` are carried on `PostToolUse` and `Stop`, accepted as inert on the two observe-only events, and still dropped with one report line on `PreToolUse`). The hook's `args` array is carried over unchanged, so an exec-form hook stays exec-form (ADR 0045 §4); an `args` that is not an array of strings drops that hook with one report line. A `StopFailure` matcher is translated through ADR 0045 §6's one table from Claude's category names to radian's kinds; an entry that does not map is dropped with one report line. A `Notification` entry survives only for `permission_prompt`, `idle_prompt`, `*` or empty; Claude's other notification types drop with one report line. Tool names translate `Bash`→`shell`, `Edit`→`patch`, `Write`→`write`, `Read`→`read` (plus `Glob`→`glob`, `Grep`→`grep`). Unknown names stay as-is with reports (never match before renames, never take effect before renames). Reports hint script `.tool_input.file_path` renames to `.path` (`old_string`→`.old`, `new_string`→`.new`).
- `.claude.json` `mcpServers` → `config.toml` `[mcp]` (ADR 0008 §8): only stdio entries run (`type` missing or `stdio`, no `url`); each keeps only `command`/`args`/`env`. Remote entries drop with one report line each, and so does every foreign key (`disabled`, `autoApprove`, `timeout`, and similar). A literal secret in `env` is never copied and never shown: under an ordinary name it becomes a `${VAR}` expansion with a suggested name (the variable to set in the shell); under a sensitive name (`api_key`, `token`, and the rest of ADR 0008 §3.2 rule 1, where the reader refuses any string) the entry moves to `inherit_env` instead.
- `CLAUDE.md` → `~/.radian/AGENTS.md` (character for character).

Project level (`--project`):

- `.claude/commands/*.md` → `<project>/.agents/skills/<name>/SKILL.md` (same user-level shaping).
- `.claude/skills/<name>/SKILL.md` → `<project>/.agents/skills/<name>/SKILL.md` (character-for-character copies. Missing frontmatter `name`/`description` normalizes with reports. Commands-occupied same names skip with reports).
- `.mcp.json` → never translates: reports state trust-gate handling.
- `.claude/settings.json` `hooks` → `<project>/.radian/hooks.json` (ADR 0045 §1): the same subset and the same tool-name translation as the user `[hooks]` import. Off-subset entries drop with one report line each.

### Never imports (one report row per class)

- Off-subset hooks: `if`, `async`/`asyncRewake` on `PreToolUse` or `PermissionRequest`, non-`command` types such as `type: "http"`, `context: fork`, unknown event names (`SessionStart` and similar), broken entries, bad `timeout`, and an `args` that is not an array of strings. `async` and `asyncRewake` on `PostToolUse` and `Stop` import (ADR 0045 §5).
- Rest: agents, plugins, LSP, `.claude/rules` (project level)/`rules/` (user level), `CLAUDE.md` neighbors such as `CLAUDE.local.md`, memory, sessions. Context files (besides `CLAUDE.md`) skip import gates but always count as untrusted input (ADR 0036). This command only moves whole user-level files, never discovers context.

### Drafts and writes

- Dry runs: config drafts print to stdout, skill files with `AGENTS.md` bodies append as `# --- <target> ---` sections. Reports (mappings, skips, todos) print to stderr. All drafts first pass formal readers (config parsers with `[hooks]`, skill name/`description` rules). Failing drafts never print and never write disk.
- `--write` never overwrites existing files. Config merges through `edit_preserving` format-preserving merges (ADR 0008 §5, same `/theme` write-back rules). Comments, order, unknown keys stay byte for byte. `[skills] extra_paths` appends only unlisted paths. `[hooks]` appends not-yet-existing matcher entries per event after existing entries. `[mcp]` appends not-yet-existing server ids in the form the file already uses (`[[mcp.servers]]` when it has those, else `[mcp.mcpServers.<name>]`); an existing id skips with one report row. Existing entries skip. Keys needing existing-content changes to land never write, reporting one row per key. Missing files create through the same path. The merged text passes the formal config parser and the MCP loader before anything writes. Files reading newer `schema_version` are read-only: whole writes stop with errors, writing nothing.
- `AGENTS.md` with each `SKILL.md` target creates through `create_new` (0600), never replacing anything. Existing targets skip with one report row each. Rows name paths, pointing at dry-run output. Other targets write as usual.
- The project hooks file creates through the same new-file path when absent, and otherwise gains only entries it does not already have (already-present entries report one row each, unmergeable shapes report per event, invalid JSON reports and stays untouched). Nothing it holds is ever replaced or dropped. Written files read back through the project hooks loader.
- Project-level file writes first print ADR 0031 T1 trust-gate hints: next startups ask as usual.
- Config parent dirs build at 0700. Config write-backs reread and check.

### Safety

- Never reads credential files (`auth.json`, `.credentials.json`): never opens, never stats, never touches them in shared dirs. Credentials never migrate, and reports point at `~/.radian/credentials/auth.json` (ADR 0026 D2).
- Never copies secret literals: suspected secret values in mcp env (sensitive key names or secret shapes) enter neither drafts nor reports. Reports give `${VAR}` references (or `inherit_env` entries under sensitive names) with suggested var names (such as `SVC_SVC_KEY`), never showing source values.
- Never runs commands, never touches networks. Translation logic lives only in this command.

## Verification

`crates/radian-host/tests/claude_migrate.rs`: user-level plus project-level fixture dirs (mode-0000 credential files, one plaintext secret, hooks with `async`/`if`/`type: "http"`/`context: fork`, unknown events). Built drafts pass formal readers (config parsers, `[hooks]`, the MCP loader, skill discovery). Reports list each non-portable item. Read records hold no credential file names. Plaintext appears only as refs. Merge write-back tests: existing configs (comments, unknown keys, existing hook entries) stay byte for byte with imported entries appended (whole-file-write mutations fail). Double imports add nothing the second time (dedup-dropped mutations fail). Existing `AGENTS.md`/`SKILL.md` skip with row-by-row reports, other targets write as usual (overwrite mutations fail). Read-only configs fail whole writes with nothing written. Named tests: credential reads (`credential_files_are_never_read`), literal copies under sensitive names (inherit_env, value never shown) and under ordinary names (`${VAR}` refs), missed `async` reports (`the_drop_report_names_every_non_portable_item`), stdio/remote/foreign-key/secret servers (`user_mcp_servers_translate_with_drops_reported_and_secrets_referenced`; literal-writing mutations fail), the `[[mcp.servers]]` merge form (`new_servers_follow_the_existing_servers_form`; always-`mcpServers` mutations fail), existing server ids (`an_existing_server_id_is_skipped_and_reported`), project hook translation (`project_hooks_translate_with_drops_reported`; untranslated-matcher mutations fail), a hook's `args` carried unchanged into the config draft, the project hooks draft and both written files (`a_hook_args_array_is_carried_over_unchanged`; dropped-`args` mutations fail), project hooks merges and invalid files (`an_existing_project_hooks_file_only_gains_missing_entries`; overwrite mutations fail). `crates/radian-exec/tests/cli.rs` owns end-to-end tests: dry runs, `--write` merges, second `--write` idempotence, `--force` refused as unknown flags.

## Constraints and Costs

- Merges only append: conflicting shapes in existing hand-written configs (such as non-list `hooks.<Event>`) stay as-is, and report rows leave fixes to human hands. `--write` offers no "import wins" overwrite.
- Unknown matcher names keep source text: unrenamed imports never match, and report rows catch them.
- `.mcp.json` never translates: the runtime reads it when the project is trusted.
