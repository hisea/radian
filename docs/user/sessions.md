---
description: Resume, list, search, fork, and export sessions.
---

# Sessions

Each run uses one session file.
The file name is the session id.
Pass `--session-id <VALUE>` to set the id.
Omit it to generate one id per process.
Set `--session-dir <DIR>` to move the files.
The default is `$RADIAN_SESSION_DIR`.
The next default is `~/.radian/sessions`.
Set `$RADIAN_HOME` to move the home directory.

Pass `--resume <id>` to resume one session.
Omit the value in the full screen interface to open the picker.
Use arrow keys to move in the picker.
Type to filter the list.
Press Tab for full text search.
Press Enter to resume.
Press `n` for a new session.
Press Esc to leave without creating a file.
Pass `--list` to print this workspace sessions and exit.
Pass `--pick` to open the picker before opening a session.
Type `/sessions` to list recent sessions inside the interface.
Type `/sessions <query>` to search message text.
Type `/fork` to fork at the last turn boundary.
Run `radian exec --fork <id>` to fork from the command line.
Add `--at <seq>` to fork at an exact sequence number.
Type `/new` to start a new session in the same terminal.
The old session stays saved.
The notice names the resume command.

Type `/export` to export the current session.
The default format is Markdown.
The default path is `<workspace>/radian-<id>.md`.
Add `json` to export the event record.
Add a path to choose the file.
Add `--force` to overwrite an existing file.
Run `radian exec --export <id>` to export from the command line.
Add `--format markdown` or `--format json` to choose the shape.
Add `--out <FILE>` to write to a file instead of stdout.
Add `--force` to overwrite.
Omit `--include-reasoning` to leave model reasoning out.
Omit `--include-provider-payload` to leave raw provider data out.

Pass `--search <TEXT>` to search this workspace sessions.
Each hit prints an id, a time, and a one line part.
Type `/compact <note>` to summarize history now.
Type `/compact` with no note to summarize without guidance.
Type `/context` to show the next request size by category.
Type `/usage` to show provider usage windows.

Run `radian attach <id>` to view a session read only.
No input reaches the session.
Nothing is written.
Add `--follow` to keep reading new events.
Press `q` or Esc to leave the follow view.
