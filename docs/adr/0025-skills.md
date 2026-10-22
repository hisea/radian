# ADR 0025: Skills

Status: accepted

## Decisions

### S1 Discovery and roots

- **User level**: two roots, both `SkillSource::User`: `$RADIAN_HOME/skills/<name>/SKILL.md` (default `~/.radian/skills/…`, owned by radian; wins on conflicts) first, `$HOME/.agents/skills/<name>/SKILL.md` (Agent Skills standard location; pi, Codex, opencode, and omp read it together) second. A missing `~/.agents/skills` skips silently. Both are sandbox-readable (ADR 0020).
- **Extra roots**: `[skills] extra_paths = [...]` keeps strings as-is. The host expands `~` and resolves relative paths against the configuration file directory. A missing directory warns once. `~/.claude/skills` reads only when listing.
- **Project level**: `<dir>/.agents/skills/<name>/SKILL.md`, with `dir` from the workspace directory up to the repository root (both ends included; nearer the workspace first; outside a repository only the workspace directory itself; same as pi). It reads only when the project is trusted (ADR 0031 T1). Untrusted roots never scan. Source records as `SkillSource::Project`. `.radian/skills` is no longer a project location (it never loaded; remove it from discovery and trust triggers together).
- **Built-in**: compiled into the binary (`include_str!`), with the same discovery result as user-level skills. Current built-in skills are three: `theme-import`, `init`, `review`.
- **Discovery rules**: recursive scan stops at a directory that holds `SKILL.md`. It ignores `.git`, `.radian-sandbox`, `.radian/arc/worktrees`, and `.radian/arc/gates` (ADR 0048 decision 15). A name conflict keeps the first found and warns. A missing `name` defaults to the directory name (same as Claude Code), then passes `valid_name` check. A missing `description` does not load. `description` validates per the Agent Skills specification. **Validation failure only warns and never blocks startup.**
- **Override and off**: discovery order is user level (`$RADIAN_HOME/skills` then `~/.agents/skills`) then `extra_paths` then project level then built-in. The same name keeps the first winner (user level and `extra_paths` beat project level; project level beats built-in). `[skills] builtin = false` turns off all built-in skills. `[skills] disabled = ["<name>"]` turns off one skill (only built-in; it cannot turn off a same-name user-level, extra, or project skill).

### S2 Context: names plus descriptions stay, bodies load on call

- The system prompt gains **one section** (not a message), assembled by the host: `## Skills` plus each skill `name`, `description`, and **absolute path**. The implementation is an `radian_core::Hooks` that touches only `request.system_prompt`. Core knows no skill.
- **Budget**: soft default **1000 tokens** (ADR 0015 conservative estimate). On exceed it truncates in discovery order and states in the prompt "N more unlisted, use `/skills`". It counts into ADR 0018 `T`.
- **Deterministic**: the same skill set (same files, same order) must produce a byte-identical section.
- **Bodies do not stay resident**: only a call reads them. A `/skill:` expanded user message counts as a normal message under `B` and compaction.
- This section computes once per process, like the memory section (ADR 0036 invariance).

### S3 Call: one built-in tool plus `/skill:`

1. **Model automatic**: built-in tool `skill(name, args?)`, implemented by the host (`radian-host`). It looks up the discovered skill by name, reads `SKILL.md`, strips frontmatter, and returns the body plus one line "relative paths use <skill directory> as base". Each discovered skill directory is a read-only root for `read`, `grep`, and `glob` in both tiers (ADR 0039 decision 4), so the model reads a skill's bundled files with the file tools. Unknown name, read failure, directory traversal (`../`), and body over budget are all **errors in the tool result**, not turn failures.
2. **User explicit**: TUI `/skill:<name> [args]`. The host reads the body and sends `<skill name=…>…</skill>` plus args as **one user message** into the inbox. `/skills` (no arguments) lists discovered skills (name plus description). `/skill:` has TUI completion; candidates are discovered skill names.
3. **Argument fill**: when the body holds `$ARGUMENTS`, each occurrence is replaced by actual arguments (empty when none), with no append. A body without a placeholder stays as-is with append. `$1` style positional placeholders do not replace (only `$ARGUMENTS`). The `skill` tool `args` and `/skill:` share the same rule, except the tool-side append shape is an `Arguments:` line.

### S4 Permissions and sandbox

- **A skill cannot authorize itself**: `allowed-tools` is **not wired** and reads in only as prompt text. Warnings for declaring skills merge to one per discovery (once per session). Skill-bundled scripts still use the `shell` tool and still pass `ToolRuleSet`, tier, and sandbox. A skill is only text, not a new execution channel.
- The `skill` tool itself is `allow` (read-only, no side effects), but its output is **untrusted content**: same class as "read files and MCP returned content". It does not change turn source.
- `skills/` is sandbox-readable: the model must read bodies and the shell must run scripts. It is not in the deny list, same class as `config/` and `cache/`.

### S5 Boundary with Core

**Core knows no skill**: it sees only one section in the system prompt string and one normal tool named `skill`. Discovery, parsing, body reads, prompt assembly, and budget all live in `radian-host`. Changes to `radian-core`: none.

### S6 Relation with memory, `B`, compaction, and fork

- Order: **base prompt, then memory section, then skills section**. The order is fixed and keeps determinism.
- Budgets are independent: memory section default 2000 tokens, skills section default 1000. Both plus the base prompt and tool definitions enter ADR 0018 `T`.
- Compaction does not touch the system prompt, so the skills section stays. A `/skill:` expanded user message summarizes and retains as a normal user message.
- Fork copies **session facts**: a `/skill:` expanded message copies as a normal message. **The available list does not enter history**. A child sees the current skill set, which can differ from the parent time.

### S7 Interaction with `ask_user` and fork

A skill never asks and never changes the ask lifecycle. When a skill text causes the model to ask, the path is still the same tool and pending queue in ADR 0024.

## Accepted Costs

- The skills section stays in the system prompt: it costs tokens even when unused (default limit 1000), and it counts into `B`.
- The model can miss a relevant skill, so the explicit `/skill:` entry must exist.
- A skill body is **untrusted content**: it can carry injected instructions. The boundary is approval and sandbox, not source downgrade.
- Project level reads only when trusted. `~/.claude/skills` reads only on explicit list.

## Verification

`crates/radian-host/src/skills` unit tests cover discovery (normal, missing description, bad frontmatter, illegal name, conflict), `name` default from directory name (illegal directory names still reject), project-level discovery (workspace to repository root, no read above root, empty roots when untrusted, user level beats project level), budget truncation with "N more", byte-identical output for the same input, built-in override with `builtin = false` and `disabled`, and `extra_paths`. `skills/tools.rs` covers body read, unknown name, `../` rejection, over-budget body truncation, `$ARGUMENTS` replacement, and no-placeholder append. TUI pty tests cover `/skill:x`, `/skills`, `/skill:nope`, and `//skill:x`. `radian-permissions` covers fixed `allow` for `skill`. The sandbox matrix covers `skills_readable`. CLI tests cover project skills entering the system prompt when trusted and staying out when untrusted.
