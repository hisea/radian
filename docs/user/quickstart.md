---
description: Install radian, connect a model, and finish your first task.
---

# Quickstart

Follow this page top to bottom.
You go from nothing to one finished task.

## Install

Pick one way to install.

With Homebrew:

```sh
brew install hisea/tap/radian
```

From a release tarball.
Download `radian-<version>-<target>.tar.gz` and its `.sha256` file from the releases page.
Check the file first, then unpack it:

```sh
shasum -a 256 -c radian-<version>-<target>.tar.gz.sha256
tar -xzf radian-<version>-<target>.tar.gz
./radian --version
```

From source with Cargo:

```sh
cargo install --git https://github.com/hisea/radian radian-tui
```

On Linux the sandboxed tier needs `bubblewrap`.
Install it first, for example with `sudo apt install bubblewrap` on Debian and Ubuntu.

## Connect a model

Radian needs a model endpoint and a stored key.
Run the login wizard:

```sh
radian login
```

Pick your provider from the numbered list.
Type the key when asked.
The key never echoes.
Radian checks the key with one read-only request before it saves anything.
The key lands in `~/.radian/credentials/auth.json`.
Nothing else in that file changes.

You can also keep the key in an environment variable.
Store `$OPENAI_API_KEY` as the key value instead of the literal key.
Radian resolves the reference on every run, so the secret stays out of the file.
See `providers` for the full login, endpoint, and model rules.

Every run names three things: `--provider`, `--model`, and `--base-url`.
The provider picks the wire format: `openai-compatible` or `anthropic`.
The model is a bare name, never `provider/model`.
The base URL is everything before the provider path.
Omit `--base-url` to use the provider public endpoint.

A worked OpenAI-compatible session:

```sh
export OPENAI_API_KEY=sk-...
radian exec --provider openai-compatible --model gpt-5 -p "summarize Cargo.toml"
```

A worked Anthropic session:

```sh
radian exec --provider anthropic --model claude-opus-4-6 --max-tokens 16384 -p "summarize Cargo.toml"
```

Anthropic needs `--max-tokens` because its API requires the field.
The default is 16384.
The flag has no meaning for `openai-compatible` requests and is refused there.

## First session

Open the full-screen interface:

```sh
radian
```

You see an input box and an empty transcript.
Type a task and press Enter.
Radian reads files, searches code, and edits with built-in tools: `read`, `write`, `patch`, `glob`, `grep`, `shell`, `job`, `memory_search`, `ask_user`, `skill`, and `subagent`.
Each tool call shows in the transcript with its arguments and its result.
Watch one run: ask for a summary of the project, and follow each `read` as it prints.

Press Esc to cancel a running turn.
Answer `y` to allow a tool question once, `a` to allow it for the session, anything else to deny it.
Press Ctrl-C twice to quit.
The session file stays on disk.

## Resume a session

List this workspace sessions:

```sh
radian exec --list
```

Resume one by id:

```sh
radian exec --resume <id> -p "continue where we left off"
```

In the full-screen interface, pass `--resume <id>` to reopen it there.
A goal that was active comes back paused; type `/goal resume` to continue it.

## Run one headless task

`radian exec -p` runs one task and exits.
Stdout carries only the model text, so you can pipe it.
Status and errors go to stderr.
The exit code tells how the turn ended: `0` for done, `3` for an exhausted budget, `4` for an unfinished answer.
See `cli` for every flag and every exit code.

```sh
radian exec --provider openai-compatible --model gpt-5 -p "list every TODO in src/"
```

## Where files live

Radian keeps its state under `~/.radian/`.
Set `RADIAN_HOME` to move the whole tree.
Sessions live in `~/.radian/sessions`.
Credentials live in `~/.radian/credentials/auth.json`.
The configuration file is `~/.radian/config/config.toml`.
Pass `--session-dir <dir>` or set `RADIAN_SESSION_DIR` to move sessions only.
See `configuration` for the file format and `environment` for every variable.

## Next steps

- Read `commands` to learn the slash commands: `/help`, `/compact`, `/model`, `/sessions`, `/memory`.
- Read `permissions` before you run in an unfamiliar repository: `full` is the default tier, `workspace` sandboxes the run.
- Read `cli` when you need a flag you have not seen here.
- Read `troubleshooting` when a request fails with an auth error or a tool refuses a path.
- Ask radian about radian in plain words: it reads this same manual through `radian docs`.
