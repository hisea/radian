# radian quickstart (macOS)

This folder (`radian-<version>-<target>.tar.gz`) holds the `radian`
full-screen TUI and its `rad` alias — two binaries, no runtime to install,
nothing else needed. This build is for macOS on Apple silicon.

Contents: `radian`, `rad`, this `README.md`, `LICENSE-MIT`, `LICENSE-APACHE`.

## Install

From a downloaded folder:

```sh
sh install.sh radian-0.1.0-aarch64-apple-darwin.tar.gz
```

Or straight from a URL:

```sh
sh install.sh https://example.invalid/radian-0.1.0-aarch64-apple-darwin.tar.gz
```

This puts `radian` and `rad` into `~/.local/bin` (no sudo needed). To use
another directory, pass `--prefix`:

```sh
sh install.sh --prefix ~/bin radian-0.1.0-aarch64-apple-darwin.tar.gz
```

`install.sh` checks the tarball against a `SHA256SUMS` file next to it when
one is present, and never touches anything outside the install directory.
If `~/.local/bin` is not on your `PATH`, it says so — add this to your shell
startup file and restart the shell:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

The binary is not notarized, so macOS may quarantine it after a download. If
macOS refuses to run it, clear the flag:

```sh
xattr -d com.apple.quarantine ~/.local/bin/radian ~/.local/bin/rad
```

There is no auto-update: to upgrade, download the newer bundle and install it
the same way, after checking its `SHA256SUMS`.

## First run

```sh
radian
```

The first start creates a starter config at
`~/.radian/config/config.toml` and opens the full-screen TUI.

- **No model yet:** the TUI opens anyway. Press `/model` to pick a provider
  and model, which is written back to the config file.
- **Model given but not logged in:** e.g.
  `radian --provider openai -m gpt-5` without a stored credential exits
  (code 2) with `no credential for 'openai'`. Log in first (below), then
  retry — it does not enter the TUI in this case.

## Login

In a terminal (login needs a terminal; it never accepts a key from a pipe):

```sh
radian login
```

With no argument this shows a numbered list: the built-in providers, ChatGPT
sign-in via browser or via device code (for machines without a browser), and
a custom endpoint (name, protocol, base URL, key). Or go straight there:

```sh
radian login chatgpt        # browser sign-in with your ChatGPT subscription
radian login chatgpt-device # device code instead of a browser
radian login openai         # store an API key
```

A typed API key is verified with one request before it is stored in
`~/.radian/credentials/auth.json` (file mode `0600`); the key never appears
in output. Inside the TUI the same flow is `/login` (e.g. `/login openai`).

## Where your data lives

Everything is under `~/.radian/` (mode `0700`; set `RADIAN_HOME` to move it
elsewhere):

- `config/config.toml` — settings
- `credentials/auth.json` — your keys and tokens (never shared, never printed)
- `sessions/` — your conversation files
- `memory/` — durable memory
- `trust/trust.json` — per-project trust choices
- `cache/`, `themes/`, `skills/`, `access/` — caches, themes, skills,
  per-project tier choices

## Permission tiers

Two tiers, picked with `--profile` (or saved per project):

- `full` (the default): unsandboxed, no built-in prompts. File tools and
  shell commands run as you.
- `workspace`: sandboxed — file writes stay inside the workspace and shell
  commands run confined.

Example: `radian --profile workspace`.

## Uninstall

Delete the two binaries:

```sh
rm ~/.local/bin/radian ~/.local/bin/rad
```

Your data stays in `~/.radian/`. To remove that too:

```sh
rm -rf ~/.radian
```
