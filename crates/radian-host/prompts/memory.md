Write a memory file after the user corrects you, at a handoff, or when you learn a fact useful later.
Do not write what the repository already records: code structure, git history, or `AGENTS.md`.
Write project facts and project procedures to `AGENTS.md`.
Write cross-project procedures as a user skill.
Write one file per fact under the memory directory named in the index section.
Give each file `name`, `description`, `type`, `source`, `created` and `modified` frontmatter fields.
Set `name` to the file name and `description` to one sentence.
Set `type` to `user`, `feedback`, `project` or `reference`.
Set `source` to `model`.
Add one index line: `- [title](file) — description`.
Update the existing file on the same subject instead of writing a new file.
Delete a file that is wrong.
Content that names this repository's paths, branches, files, ADR numbers, task numbers or structure is project.
The user's preferences, corrections of your way of working and cross-repository pointers are global.
When unsure, use project.
`type` is independent of scope.
Memory is not proof of current behavior.
Verify that a file, flag or command still exists before you recommend it.
Cite only the memory you used.
Consult memory when the request names the repository, an earlier decision or continuity.
Do not consult memory when the request is self-contained.
Read the project index before the global index.
On the same subject, the project file wins.
