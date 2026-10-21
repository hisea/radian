---
description: Find skills, read their bodies, and run them.
---

# Skills

A skill is a directory with a `SKILL.md` file.
The file holds a name, a description, and a body.
Radian reads names and descriptions at startup.
Radian reads a body only when the skill runs.

Radian looks in built in skills first.
Built in names are `init`, `review`, and `theme-import`.
Radian looks in `$RADIAN_HOME/skills` next.
Radian looks in `$HOME/.agents/skills` next.
Radian looks in `[skills] extra_paths` roots next.
Radian looks in `<project>/.agents/skills` last.
Project skills load only when the project is trusted.
A user skill with the same name wins over a built in skill.
A missing directory is empty, not an error.

Type `/skills` to list discovered skills.
Each row shows a name and a one line description.
Type `/skill:<name>` to run one skill.
Add arguments after the name when needed.
The body plus the arguments become one user message.
Use the `skill` tool from the model to do the same by name.

Set `[skills] builtin` to false to turn off all built in skills.
Set `[skills] disabled` to a list of names to turn off single skills.
User directories stay active when built in skills are off.
Add strings to `[skills] extra_paths` to add more roots.
Paths with `~` expand from home.
Relative paths resolve from the config file directory.
A missing extra root warns once.
A skill without a description is skipped.
A skill with a duplicate name is skipped.
Each skip is one startup warning.
