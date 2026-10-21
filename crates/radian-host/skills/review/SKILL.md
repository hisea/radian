---
name: review
description: Review code changes with parallel subagent reviewers and a ship/no-ship verdict. Use when asked to review uncommitted changes, a branch, or a commit.
---

# Review the changes

Target: $ARGUMENTS (empty means the uncommitted changes; otherwise a branch name or a commit).

Report only, never edit code. No `/review` top-level command exists; this skill is the
whole feature.

## 1. Get the diff

Use `shell` to take the diff for the target: uncommitted changes by default (`git diff`
plus `git status` for untracked files), a branch or a commit when one is named. Group the
diff by file.

## 2. Review in parallel

In **one reply**, start several **`general`** `subagent` calls, one per file or file group,
so they run in parallel. Do not use `explore`: it has no `shell` and cannot run `git`.

- Each subagent gets its files' diff hunks and returns at most **4 000 characters** of
  findings, each finding graded **P0** (blocks ship: bug, breakage, security hole),
  **P1** (should fix before ship), **P2** (suggestion), or **P3** (nit), with file and line.
- Tell each subagent to stay within the limit: over-long conclusions are truncated.

## 3. Merge and verdict

Merge the subagents' findings, drop duplicates, keep the P0–P3 grades with file and line,
and end with a one-line verdict: `ship` or `no-ship` with the reason.
