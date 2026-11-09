# Shell completions and man pages

Everything in this directory except this file, and everything in `man/`, is generated from the clap definitions in `src/cli.rs`. Do not edit it by hand.

```bash
just generate        # or: cargo run -p xtask -- generate
```

The generator deletes the old files first, so a removed command leaves nothing behind. Hidden commands get no man page.

| File | Shell | Install as |
|---|---|---|
| `dotman.bash` | Bash | `~/.local/share/bash-completion/completions/dotman` |
| `_dotman` | Zsh | `_dotman` in a directory on `$fpath` |
| `dotman.fish` | Fish | `~/.config/fish/completions/dotman.fish` |
| `dotman.elv` | Elvish | `eval (slurp < dotman.elv)` in `rc.elv` |
| `_dotman.ps1` | PowerShell | dot-source it from `$PROFILE` |

Without a checkout, `dotman completion <bash|zsh|fish|elvish|powershell>` prints the same script for the name it was started as.

## The `dot` alias

If you installed the optional `dot` alias, point it at the `dotman` completion:

```bash
# bash
complete -F _dotman -o bashdefault -o default dot
# zsh
compdef dot=dotman
# fish
complete -c dot -w dotman
```

## Man pages

`man/dotman.1` is the top page and `man/dotman-<command>.1` covers each command. The exit codes and environment variables are at the end of `dotman.1`, under EXTRA, which is where clap_mangen puts a command's long help epilogue; `status`, `sync`, `diff` and `git` have one too.

```bash
man -l man/dotman.1
sudo install -Dm644 man/*.1 -t /usr/share/man/man1/
```
