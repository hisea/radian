---
description: All slash commands and shell commands in the full screen interface.
---

# Commands

Type `/help` to print the command list.
Type `//` at the start to send a message that starts with `/`.
Type `/queue <text>` to queue a message for the next turn.
Type `/retry` to resend the last message after a model error.
Type `/loop` to repeat a prompt every turn.
Type `/loop --every <time> --times <n> --for <time> <prompt>` to bound the repeat.
Type `/copy` to copy the last assistant message.
Type `/copy code` to copy only its last code block.
Type `/compact` to summarize history now and keep the recent part.
Type `/compact <note>` to add a note to the summary.
Type `/sessions` to list recent sessions for this workspace.
Type `/sessions <query>` to search session text.
Type `/fork` to fork this session at its last turn boundary.
Type `/new` to start a new session here.
Type `/theme <name>` to switch the color theme.
Type `/model <provider>/<model>` to switch the model at the next turn boundary.
Type `/thinking <level>` to set the thinking level.
Type `/usage` to show current provider usage.
Type `/context` to show the next request size estimate by category.
Type `/auto on` to approve tool questions for this session.
Type `/auto off` to ask again.
Type `/yolo` as a short form of `/auto on`.
Type `/access` to show the current permission tier.
Type `/access workspace` to use the sandboxed tier.
Type `/access full` to use the unsandboxed tier.
Type `/export` to export this session as Markdown.
Type `/export md <path>` to write Markdown to a path.
Type `/export json <path>` to write the event record to a path.
Type `/memory <command>` to use durable memory.
Type `/memory list` to list records.
Type `/memory search` to search records.
Type `/memory show` to show one record.
Type `/memory add` to add a record.
Type `/memory edit` to change a record.
Type `/memory archive` to archive a record.
Type `/memory restore` to restore an archived record.
Type `/memory forget` to forget a record.
Type `/memory purge` to rewrite the log and remove forgotten content.
Type `/memory confirm` to confirm a record.
Type `/memory export` to print one scope as editable Markdown.
Type `/memory import` to import an edited Markdown export.
Type `/memory project` to show project identity.
Type `/memory relink` to attach this workspace to an existing project.
Type `/memory split` to move records between projects.
Type `/memory merge` to merge one project into another.
Type `/skill:<name>` to run a skill.
Type `/skill:<name> <args>` to run a skill with arguments.
Type `/skills` to list discovered skills.
Type `/jobs` to open the jobs page.
Type `/jobs kill <id>` to stop one background job.
Type `/jobs kill all` to stop every background job.
Type `/tools` to open the tools page.
Type `/mcp` to open the MCP page.
Type `/hooks` to open the hooks page.
Type `/panel` to open the last used panel page.
Type `/login` to open the credential wizard.
Type `/login <provider>` to skip the provider choice.
Type `/logout <provider>` to remove one stored credential.
Type `!<command>` to run a shell command.
Type `!!<command>` to run a shell command outside model context.
Ask about radian itself in plain words to read the built in manual.
