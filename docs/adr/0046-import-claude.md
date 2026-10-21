# ADR 0046: `radian config import-claude` (Claude Code One-Shot Import)

Status: accepted

## Background

Radian owns two one-shot translation precedents (`radian config import-pi`, `radian theme import`): code translates deterministically, never through models. Claude Code private formats import only, and runtime takes only Radian own formats (same principle as ADR 0029 section 5). Mappings live only in import commands. Runtime never takes Claude formats.

## Decisions

The command is `radian config import-claude`, symmetric with `import-pi` (`ConfigCommand::ImportClaude`, same dispatch). The `radian` binary shares the same grammar through `agent_cli::ConfigCommand` (ADR 0032). Sources land at their own level. Explicit sources: `--claude-home <dir>` (user-level `~/.claude/` tree) with `--project <dir>` (project dir). Defaults never read. At least one is required. Unnamed user dirs never read.

### Mappings (only fidelity-safe ones)

User level (`--claude-home`):

- `commands/*.md` → `~/.agents/skills/<name>/SKILL.md`: frontmatter gains `name` (file name). Missing usable `description` fills a placeholder and names it in reports. Body `$ARGUMENTS` stays as-is (ADR 0025 S3).
- `skills/` → writes only one `[skills] extra_paths` row in `config.toml` pointing at it, never copies files. ADR 0025 S1 notes `~/.claude/skills` reads only when listed.
- `settings.json` `hooks` → `[hooks]` (ADR 0045 subset: `PreToolUse`/`PostToolUse`/`Stop`, `type = "command"`, no `async`/`if`/`http`/`prompt`/`agent`/`context: fork`). Tool names translate `Bash`→`shell`, `Edit`→`patch`, `Write`→`write`, `Read`→`read` (plus `Glob`→`glob`, `Grep`→`grep`). Unknown names stay as-is with reports (never match before renames, never take effect before renames). Reports hint script `.tool_input.file_path` renames to `.path` (`old_string`→`.old`, `new_string`→`.new`).
- `CLAUDE.md` → `~/.radian/AGENTS.md` (character for character).

Project level (`--project`):

- `.claude/commands/*.md` → `<project>/.agents/skills/<name>/SKILL.md` (same user-level shaping).
- `.claude/skills/<name>/SKILL.md` → `<project>/.agents/skills/<name>/SKILL.md` (character-for-character copies. Missing frontmatter `name`/`description` normalizes with reports. Commands-occupied same names skip with reports).
- `.mcp.json` → never translates: reports state trust-gate handling.
- `.claude/settings.json` `hooks` → never imports: reports entry by entry.

### Never imports (one report row per class)

- User `.claude.json` `mcpServers`: never imports. Reports per server.
- Off-subset hooks: `if`, `async` (entry and hook levels both count), non-`command` types such as `type: "http"`, `context: fork`, unknown event names (`SessionStart` and similar), broken entries, bad `timeout`.
- Rest: agents, plugins, LSP, `.claude/rules` (project level)/`rules/` (user level), `CLAUDE.md` neighbors such as `CLAUDE.local.md`, memory, sessions. Context files (besides `CLAUDE.md`) skip import gates but always count as untrusted input (ADR 0036). This command only moves whole user-level files, never discovers context.

### Drafts and writes

- Dry runs: config drafts print to stdout, skill files with `AGENTS.md` bodies append as `# --- <target> ---` sections. Reports (mappings, skips, todos) print to stderr. All drafts first pass formal readers (config parsers with `[hooks]`, skill name/`description` rules). Failing drafts never print and never write disk.
- `--write` never overwrites existing files. Config merges through `edit_preserving` format-preserving merges (ADR 0008 §5, same `/theme` write-back rules). Comments, order, unknown keys stay byte for byte. `[skills] extra_paths` appends only unlisted paths. `[hooks]` appends not-yet-existing matcher entries per event after existing entries. Existing entries skip. Keys needing existing-content changes to land never write, reporting one row per key. Missing files create through the same path. Files reading newer `schema_version` are read-only: whole writes stop with errors, writing nothing.
- `AGENTS.md` with each `SKILL.md` target creates through `create_new` (0600), never replacing anything. Existing targets skip with one report row each. Rows name paths, pointing at dry-run output. Other targets write as usual.
- Project-level file writes first print ADR 0031 T1 trust-gate hints: next startups ask as usual.
- Config parent dirs build at 0700. Config write-backs reread and check.

### Safety

- Never reads credential files (`auth.json`, `.credentials.json`): never opens, never stats, never touches them in shared dirs. Credentials never migrate, and reports point at `~/.radian/credentials/auth.json` (ADR 0026 D2).
- Never copies secret literals: suspected secret values in mcp env (sensitive key names or secret shapes) enter neither drafts nor reports. Reports give ADR 0008 §3 env refs with suggested var names (such as `SECRET_SRV_API_KEY`), never showing source values.
- Never runs commands, never touches networks. Translation logic lives only in this command.

## Verification

`crates/radian-host/tests/claude_migrate.rs`: user-level plus project-level fixture dirs (mode-0000 credential files, one plaintext secret, hooks with `async`/`if`/`type: "http"`/`context: fork`, unknown events). Built drafts pass formal readers (config parsers, `[hooks]`, skill discovery). Reports list each non-portable item. Read records hold no credential file names. Plaintext appears only as refs. Merge write-back tests: existing configs (comments, unknown keys, existing hook entries) stay byte for byte with imported entries appended (whole-file-write mutations fail). Double imports add nothing the second time (dedup-dropped mutations fail). Existing `AGENTS.md`/`SKILL.md` skip with row-by-row reports, other targets write as usual (overwrite mutations fail). Read-only configs fail whole writes with nothing written. Three more named tests: credential reads (`credential_files_are_never_read`), literal copies (`a_literal_secret_is_never_shown_and_becomes_a_reference`), missed `async` reports (`the_drop_report_names_every_non_portable_item`). `crates/agent-cli/tests/cli.rs` owns end-to-end tests: dry runs, `--write` merges, second `--write` idempotence, `--force` refused as unknown flags.

## Constraints and Costs

- Merges only append: conflicting shapes in existing hand-written configs (such as non-list `hooks.<Event>`) stay as-is, and report rows leave fixes to human hands. `--write` offers no "import wins" overwrite.
- Unknown matcher names keep source text: unrenamed imports never match, and report rows catch them.
- `.mcp.json` with project hooks never import: the former never translates, the latter reports entry by entry.
