---
description: Switch color themes and import themes from files.
---

# Themes

Type `/theme <name>` to switch the theme now.
Radian writes the name back to the config file.
Built in names are `dark`, `light`, and `mono`.
A file name in `~/.radian/themes` is also a valid name.
Use the file stem as the name.
Both `.toml` and `.json` files are readable.
A `.toml` file wins when both exist for one stem.

Run `radian theme import <file>` to import a file.
The input is a theme JSON document.
The output is a Radian TOML file under `~/.radian/themes`.
Pass `--name <NAME>` to override the output name.
Omit it to use the file `name` field.
Omit both to use the input file stem.
Pass `--force` to overwrite an existing file.
The command prints the written path.
The command prints one report line per mapped value.
A missing input file exits with code 1.
A bad input file exits with code 1.
An existing output without `--force` exits with code 1.

Set `tui.theme` in the config file to set the startup theme.
An unknown name warns and falls back.
Set `[tui.colors]` to override single roles.
A color accepts a name, a hex number, or a table.
