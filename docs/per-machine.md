# Per-machine files

Variants, templates and hooks. Back to the [README](../README.md).

## Variants

A repo file can carry one brace block in its final name component, such as `monitors{host=laptop}.conf`. It targets `monitors.conf` and applies only where its conditions hold. Any other brace text is a literal file name.

- Keys: `host`, `user`, `profile`, `distro`, `like` (any `ID_LIKE` token), `os` (`linux`, `macos`, `wsl`), `arch`. Join conditions with `+`. `key!=value` negates.
- Flags: `tmpl` renders the file as a template; `age` stores it encrypted. Example: `.zshrc{profile=work+os=linux}`, `hosts{age}.yml`.
- Ranking: the variant with the most specific positive key wins (`host` beats `user` beats `profile` and so on down to `arch`), and a longer list of matching keys beats a shorter one. A variant with any condition beats the plain file. Anything still tied is ambiguous: `status` reports it and `apply` skips it.

`dotman add --for host=laptop <file>` creates the variant name for you.

## Templates

A `{tmpl}` file is rendered with minijinja in strict mode: an undefined variable is an error that names the file and the variable. There is no `include` and no command execution.

Variables: `host`, `user`, `distro`, `os`, `arch`, `home`; the lists `profiles` and `like`; `vars` (the `[vars]` table of the machine config); and `env`, which holds only the names listed in `[template] env` of the machine config. The allowlist is empty by default.

dotman never captures a local edit into a template. A local edit to a rendered file is reported as template drift, and the run exits 3. Fix it with `dotman edit <path>`, which writes the new rendering over the file when the source changed, or discard the local change with `dotman apply --take-repo <path>`.

## Hooks

The `.dotman.toml` that dotman writes has an empty `[hooks]` table. Add entries to it that map a glob over managed paths to a shell command, for example `".config/hypr/**" = "hyprctl reload"`. A second `[hooks]` header is a TOML error. After `apply`, `sync` or `edit` writes a matching path into `$HOME`, the command runs once through `/bin/sh -c`, with `$HOME` as the working directory and its output on stderr. A command may span several lines in a `"""` string. A carriage return, escape or other control character, other than tab and newline, is refused, and so is a bidi control or an invisible format character, such as U+202E or U+200B, in a glob or a command: no hook runs, none is approved, and the run exits 1.

A hook table needs approval on each machine. The prompt shows every command, and `--approve-hooks` answers it. The approval is the table itself, so a changed command or glob needs approval again, while spacing and order in the file do not. An approval saved by an older dotman, which stored a hash, is asked for again. Approved hooks run in unattended `sync`; a table that changed after a pull is skipped with a warning until you approve it again. A hook held back because its table was not approved runs once when the table is approved. The warning prints the whole table, and that table is what the printed `--approve-hooks` approves. If the table changed since that warning, `--approve-hooks` approves nothing: it shows the new table and exits 3, and the same command run again approves the table it showed. With no such warning recorded, `--approve-hooks` approves the current table, printing it first. `init <url>` runs hooks only with `--approve-hooks`, and `--no-hooks` runs none. Approval covers the command text, not the scripts it calls.
