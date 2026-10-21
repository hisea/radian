---
description: Save facts across sessions and search them by keyword.
---

# Memory

Memory holds small durable records.
Each record has an id, content, scope, and revision.
Scopes are project and global.
Project records belong to one project directory.
Global records belong to the user.

Type `/memory list` to list records.
Project records come first.
Type `/memory search <words>` to search by keyword.
Every word must match.
Matching ignores case.
Type `/memory show <id>` to read one record in full.
Type `/memory add <text>` to add a project record.
Type `/memory edit <id> <text>` to change content.
Pass the current revision when editing from the command line.
Type `/memory archive <id>` to archive a record.
Type `/memory restore <id>` to restore an archived record.
Type `/memory forget <id>` to forget a record.
Forgetting asks for confirmation.
Type `/memory purge` to rewrite the log and remove forgotten content.
Purging asks for confirmation.
Type `/memory confirm <id>` to confirm a record.
Type `/memory export` to print one scope as Markdown.
Type `/memory import` to import an edited export.
Type `/memory project` to show project identity.
Type `/memory relink` to attach this workspace to another project.
Type `/memory split` to move records between projects.
Type `/memory merge` to merge one project into another.
Splitting and merging ask for confirmation.

Run the same grammar outside the interface with `radian memory`.
Add `--workspace <DIR>` to resolve the project against another directory.
Use `radian memory list --all` to include archived records.
Use `radian memory import -` to read the edited Markdown from stdin.

The model uses the `memory_search` tool.
It searches by `query` keywords.
It reads full records by `ids`.
Pass `query` or `ids`, never both.
Use `scope` to narrow the search.
The default scope is `all`.
The `all` scope searches project first, then global.
Results arrive ordered by scope, trust, match, recency, and id.
New matches in later turns arrive as a system message.
Set `[memory.injection]` `additions` to false to turn those notes off.
