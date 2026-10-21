---
description: Find skills, read their bodies, and run them.
---

# Skills

A skill is a directory with a `SKILL.md` file. The file holds a name, a
description, and a body. Radian reads names and descriptions at startup and
shows them to the model as one section of the system prompt. Radian reads a
body only when the skill runs, through the `skill` tool or `/skill:<name>`.

## The skill file

```md
---
name: review
description: Review the current diff for style and missing tests.
---

Read the current diff. List style problems and missing tests.
Use paths relative to this file: `./checks/style.md`.
```

The frontmatter holds the `name` and the `description`. Without a
frontmatter `name`, the directory name is the name. Without a `description`,
the skill is skipped with one warning. The name holds at most 64 characters;
the description at most 1024. An invalid name or a duplicate name is skipped
with one warning each.

The body is Markdown the model follows. `$ARGUMENTS` in the body fills with
the caller's arguments; without the placeholder, the arguments append to the
body verbatim. Relative paths in the body resolve from the skill's own
directory. The body is limited to 64 KiB: a longer body fails the call with
an error naming the size. Set `disable_model_invocation: true` in the
frontmatter to keep a skill out of the system prompt while leaving it
runnable by name.

## Where skills live

Radian looks in this order, and the first name wins:

1. Your skills: `$RADIAN_HOME/skills`, then `$HOME/.agents/skills`. Both
   count as user skills; radian's own directory wins a conflict between the
   two. A missing directory is empty, never an error, and a missing
   `$HOME/.agents/skills` is silent, never a warning.
2. `[skills] extra_paths` roots from the config file. An extra root that
   holds a `SKILL.md` directly is itself one skill. A missing root warns
   once.
3. Project skills: `<project>/.agents/skills`, from the workspace up to the
   repository root. Project skills load only when the project is trusted.
4. Built-in skills, compiled in: `init`, `review`, and `theme-import`. They
   load last, so any skill of your own with the same name wins.

Discovery never descends into `.git`, `.radian-sandbox` or
`.radian-worktrees`. The discovered order is deterministic: the same files
always produce a byte-identical section, within a soft budget of 1000
tokens.

## Running a skill

Type `/skills` to list discovered skills. Each row shows a name and a
one-line description. Type `/skill:<name>` to run one skill, with arguments
after the name when needed. The body plus the arguments become one user
message. The model uses the `skill` tool to do the same by name: `name` is
required, `args` is optional and fills `$ARGUMENTS` the same way.

```sh
/skill:review --strict
```

A name that is not discovered answers `unknown_skill` in the TUI and
`there is no skill named …` from the tool. A body that cannot be read, or is
over the limit, fails the call with the reason.

## Configuring skills

```toml
[skills]
builtin = false
disabled = ["review"]
extra_paths = ["~/work/skills"]
```

`builtin = false` turns off all built-in skills. `disabled` names single
skills to turn off. User directories stay active when built-in skills are
off. `extra_paths` adds more roots: paths with `~` expand from home, and
relative paths resolve from the config file directory.
