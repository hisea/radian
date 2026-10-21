# ADR 0038: Mid-Session Tier Switch (`/access`) and `.git` Write Rights

Status: accepted

## Decisions

### 1. `/access workspace|full`

- Values are the two ADR 0016 tier names (`workspace`, `full`).
- **Takes effect at the next tool call** (running commands stay unaffected), and the journal records one `ConfigChanged`.
- A switch to `full` needs explicit human confirm. One confirm panel states full means no sandbox and no approval prompts (`no prompts`). The model never switches itself, and no tool switches.
- The `--profile` startup flag and `permissions.profile` config stay. `/access` is their runtime form.
- The default tier is `full` (ADR 0043).

### 2. Remembers per project

- Confirm offers "this time only" or "this project from now on". Records live in `~/.radian/access/access.json` (the user own directory, project identity per ADR 0021). Records stay **never in repo files**. Else a strange repo ships one `full` row, and its opener runs with no sandbox.
- Project-level `<workspace>/.radian/config.toml` can write `profile = "workspace"`, meaning "this project wants the sandbox". It **only narrows**, and applies trusted or not. Writing `full` is a config error, warns and ignores.
- Both sides take the narrower: user record `full` plus project claim `workspace` yields effective `workspace`. Any side wanting sandbox means sandbox.
- An effective startup tier from records or project claims marks both the startup line and footer. Examples are `full · no sandbox (project)` and `workspace · sandbox on (project)`. One more `/access` switch changes or clears the record.
- First entry to a new repo adds one trust-gate popup (ADR 0031) option "use the sandbox for this project". Picking it writes `access.json`.

### 3. `.git` write rights

Under tier `workspace`, the general session writes `.git`, **but code-executable parts stay read-only**:

- **Writable**: everything under `.git` except the rows below. So commit, branch, checkout, stash, rebase, and merge all run with native git inside the sandbox.
- **Read-only**: `.git/config`, `.git/config.worktree`, `.git/hooks/`, submodule `.git/modules/*/config` with `.git/modules/*/hooks/`, plus worktree pointers and same-kind files in shared common dirs. Linked-worktree per-worktree gitdirs and shared common dirs discover separately, same handling.
- **Unchanged**: git in the sandbox still runs hooks and filters it reads (user-installed, model-unchangeable). Workspace `core.hooksPath` directories (such as `.husky/`) are writable already, and sandbox runs never cross bounds.
- **File tools share the table**. `write`/`patch` are host tools outside the sandbox. Tier `workspace` denies the same discovered entries at the permission layer. Rules are `write|patch` `deny` rules. They share the one list in `radian-sandbox` with the sandbox. Worktree pointer files deny wholesale. Tier `full` releases everything per its tier, this item never governs.

### 4. `subagent` child sessions

`subagent` child sessions inherit the parent tier, narrow only. `.git` rules match the parent. No task-agent whole-read-only rule exists, and no `git_commit` tool exists.

## Constraints and Costs

- Opened `.git` writes let the model rewrite history, delete branches, change the index. These are same-kind "workspace writable" risks in the sandbox. Safety rests on git reflogs and human review, never the sandbox.
- The read-only list enumerates known execution entries. Git execution entries changing means this list syncs too.

## Verification

`radian-sandbox` escape matrix with the `crates/radian-sandbox/LIMITS.md` table: writes to `.git/config`, `.git/hooks/x`, `.git/config.worktree`, submodule configs all deny. `git commit`, `git checkout -b`, `git stash` succeed in the sandbox. Linked-worktree native commits succeed, while hook writes and common-dir config writes deny. TUI pty: a switch to `full` needs confirm. Esc cancel stays unchanged. Remembered choices apply after restart with marks. No repo file ever activates it. `radian-sandbox` `inherited` prose claims none of `.git`, deny-read, or network rows.
