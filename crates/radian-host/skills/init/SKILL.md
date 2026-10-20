---
name: init
description: Initialize project instructions for this repository. Use when asked to create or update AGENTS.md, to document how to build and test the project, or to set up a new checkout.
---

# Initialize AGENTS.md

Write (or complete) the repository's `AGENTS.md`: what this project is, how to build and
test it, and the conventions a coding agent must follow here.

## 1. Read the repository first

- List the top-level structure and read the build files (`Cargo.toml`, `package.json`,
  `Makefile`, `go.mod`, `pyproject.toml`, whatever this repo actually has).
- Read the existing docs that describe workflow (`README.md`, `CONTRIBUTING.md`, similar).
- If an `AGENTS.md` (or `CLAUDE.md`) already exists, read it: only add what is missing,
  never overwrite or delete what is there.

## 2. Verify the commands by running them

- Find the build, test, and lint commands from the build files and docs, then **actually
  run** the cheapest ones that cover the workflow (e.g. the default build, the test suite
  or its narrowest relevant subset, the formatter check).
- A command counts as verified only if you ran it in this session and saw it succeed.
- Prefer read-only and quick commands; do not run anything destructive, networked, or
  long-running without asking the user first.

## 3. Write the file

- Keep it short and imperative: facts about this repo, not generic advice.
- The result must stay inside the context-file limit: one file may hold at most **64 KiB**
  (ADR 0036). If the content threatens to exceed that, cut generic background first.
- End with a short section that lists which commands were verified by running them and
  which were only read from files and remain unverified.
