# dotman

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![CI](https://img.shields.io/github/actions/workflow/status/UtsavBalar1231/dotman/ci.yml?branch=main)](https://github.com/UtsavBalar1231/dotman/actions)

dotman is a dotfiles manager for Linux. It has no learning curve and is safe by default.

Your dotfiles live in a normal git repo. dotman copies them into `$HOME`, works out which side changed, and asks before it overwrites anything. When it does replace a file in `$HOME`, it backs up the old one first. It never writes through a symlink, runs a hook you haven't approved, or runs a command from a template.

## Install

The command is `dotman`. The installers also add a shorter `dot` symlink, unless another `dot` is already on your `PATH` (on most systems `/usr/bin/dot` is Graphviz).

```bash
# release tarball for Linux, checksum-verified
curl -fsSL https://raw.githubusercontent.com/UtsavBalar1231/dotman/main/install/install.sh | bash

# from source (Rust 1.88 or newer)
cargo install --git https://github.com/UtsavBalar1231/dotman --locked dotman-cli

# Arch Linux, from a checkout
cd packaging/arch && makepkg -si
```

The [releases page](https://github.com/UtsavBalar1231/dotman/releases) has tarballs for Linux on x86_64 and aarch64. The glibc builds need glibc 2.39 or newer. The static musl builds run on any Linux, so the installer uses those.

There are no macOS builds yet, and dotman hasn't been built or tested on a Mac, so `install.sh` won't work there. Build it from source instead: `cargo install --git https://github.com/UtsavBalar1231/dotman --tag v0.1.0 --locked dotman-cli`.

## Quick start

On your first machine, run `init`. It only asks what it can't work out for itself: your git name and email, the remote (it can create a private GitHub repo for you with `gh`), the host name and profiles, any system files, and an encryption key.

```bash
dotman init
dotman add ~/.zshrc ~/.config/nvim
dotman sync
```

On your other machines:

```bash
dotman init <url>
```

After that, edit your files where they are and run `dotman sync`. It picks up your edits, commits them, pulls, applies whatever came in, and pushes. `dotman status` tells you what's different, and `dotman diff` shows you the changes.

[Getting started](docs/getting-started.md) has the rest, such as how `sync` picks up new files in a directory you've added.

## Demo

Setting up two machines:

https://github.com/user-attachments/assets/a9fac84a-79f5-49c9-802e-d13a6b2af84b

Editing and syncing day to day, and the backups sync makes:

https://github.com/user-attachments/assets/4068b53a-8dfb-46b2-a1b2-8414b8a904de

Per-machine files, templates and secrets:

https://github.com/user-attachments/assets/8bde50d1-c0ee-4340-8b72-7202184c55ae

## Documentation

- [Getting started](docs/getting-started.md): setting up in full, what dotman needs, and using it in Codespaces, Dev Containers and DevPod.
- [Daily use](docs/daily-use.md): the commands and their exit codes, how `status` and `sync` work, backups and file modes, ignoring files, and the flags that answer prompts.
- [Per-machine files](docs/per-machine.md): variants, templates and hooks.
- [Secrets](docs/secrets.md): encrypted files, your age key, and how dotman keeps credentials out of the repo.
- [System files](docs/system-files.md): files outside `$HOME`, such as in `/etc`.
- [Migration](docs/migration.md): moving over from the old dotman, yadm, a bare repo, vcsh, chezmoi or a symlink farm.

## Limitations

- If two machines re-encrypt your secrets at the same moment, sync can hit a conflict. Syncing again sorts it out.
- macOS hasn't been built or tested. Build from source there.
- Windows isn't supported.

## Contributing

[AGENTS.md](AGENTS.md) explains the layout, the rules and the commands. Before you open a pull request, run `just lint-strict`, `cargo test --workspace --all-targets` and `just e2e`. That last one runs the scenarios in `e2e/`: several machines syncing, recovery after a killed run, and the importers. `just e2e-distros` runs the same journeys in Docker on several distros.

To report a vulnerability, see [SECURITY.md](SECURITY.md).

## License

MIT. See [LICENSE](LICENSE).
