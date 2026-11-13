# dotman

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![CI](https://img.shields.io/github/actions/workflow/status/UtsavBalar1231/dotman/ci.yml?branch=main)](https://github.com/UtsavBalar1231/dotman/actions)

A dotfiles manager with no learning curve, safe by default, for Linux.

Your dotfiles live in an ordinary git repo. dotman copies them into `$HOME`, notices which side changed, and asks before it overwrites anything. A file it replaces in `$HOME` is backed up first. It never writes through a symlink, never runs a hook you have not approved, and never runs a template command.

## Install

The binary is `dotman`. Installers also add a `dot` symlink, but only when no other `dot` is on `PATH`, because `/usr/bin/dot` is usually Graphviz.

```bash
# release tarball for Linux, checksum-verified
curl -fsSL https://raw.githubusercontent.com/UtsavBalar1231/dotman/main/install/install.sh | bash

# from source (Rust 1.88 or newer)
cargo install --git https://github.com/UtsavBalar1231/dotman --locked dotman-cli

# Arch Linux, from a checkout
cd packaging/arch && makepkg -si
```

Release tarballs are on the [releases page](https://github.com/UtsavBalar1231/dotman/releases). They are for Linux on x86_64 and aarch64: the glibc builds need glibc 2.39 or newer, and the static musl builds run on any Linux, which is why the installer picks them. There are no macOS binaries, so `install.sh` fails on macOS, and dotman has not been built or tested there. On a Mac, build from source: `cargo install --git https://github.com/UtsavBalar1231/dotman --tag v0.1.0 --locked dotman-cli`.

## Quick start

On the first machine, `init` asks only what it cannot detect: git identity, a remote (it can create a private GitHub repo through `gh`), host name and profiles, system files, and an encryption key.

```bash
dotman init
dotman add ~/.zshrc ~/.config/nvim
dotman sync
```

On a second machine:

```bash
dotman init <url>
```

From then on, edit files where they are and run `dotman sync`. It captures local edits, commits, pulls, applies what arrived and pushes. `dotman status` shows what differs, and `dotman diff` shows how.

[Getting started](docs/getting-started.md) covers the rest, such as how `sync` captures new files under a directory you add.

## Documentation

- [Getting started](docs/getting-started.md): the full quick start, requirements, and Codespaces, Dev Containers and DevPod.
- [Daily use](docs/daily-use.md): the commands and exit codes, `status`, `sync`, backups and modes, ignoring files, and the flags that answer prompts.
- [Per-machine files](docs/per-machine.md): variants, templates and hooks.
- [Secrets](docs/secrets.md): encrypted files, the age identity, and the guards that keep credentials out of the repo.
- [System files](docs/system-files.md): files outside `$HOME`, such as under `/etc`.
- [Migration](docs/migration.md): importing legacy dotman, yadm, bare repos, vcsh, chezmoi and symlink farms.

## Limitations

- Two machines with keys that re-encrypt at the same moment can hit a sync conflict. Syncing again resolves it.
- macOS has not been built or tested; build from source there.
- Windows is not supported.

## Contributing

See [AGENTS.md](AGENTS.md) for the layout, rules and commands. Run `just lint-strict`, `cargo test --workspace --all-targets` and `just e2e`, the multi-machine, kill-recovery and importer scenarios in `e2e/`, before opening a pull request; `just e2e-distros` repeats the journeys in Docker on several distros. See [SECURITY.md](SECURITY.md) for vulnerability reporting.

## License

MIT License. See [LICENSE](LICENSE).
