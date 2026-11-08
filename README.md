# dotman

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![CI](https://img.shields.io/github/actions/workflow/status/UtsavBalar1231/dotman/ci.yml?branch=main)](https://github.com/UtsavBalar1231/dotman/actions)

A dotfiles manager with no learning curve, safe by default, for Linux and macOS.

Your dotfiles live in an ordinary git repo. dotman copies them into `$HOME`, notices which side changed, and asks before it overwrites anything. Every file it replaces is backed up first. It never writes through a symlink, never runs a hook you have not approved, and never runs a template command.

## Install

The binary is `dotman`. Installers also add a `dot` symlink, but only when no other `dot` is on `PATH`, because `/usr/bin/dot` is usually Graphviz.

```bash
# release tarball, checksum-verified (Linux and macOS)
curl -fsSL https://raw.githubusercontent.com/UtsavBalar1231/dotman/main/install/install.sh | bash

# from source (Rust 1.88 or newer)
cargo install --git https://github.com/UtsavBalar1231/dotman --locked dotman-cli

# Arch Linux, from a checkout
cd packaging/arch && makepkg -si
```

Release tarballs are on the [releases page](https://github.com/UtsavBalar1231/dotman/releases).

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

Every day, edit files where they are, then run `dotman sync`. It captures local edits, commits, pulls, applies what arrived and pushes. `dotman status` shows what differs, and `dotman diff` shows how.

## Commands

| Command | Purpose |
|---|---|
| `dotman init` | Create the repo, or clone one and apply it. Safe to rerun |
| `dotman add` | Start managing files or directories |
| `dotman forget` | Stop managing files, keeping them in `$HOME` |
| `dotman list` | List the managed files |
| `dotman status` | Compare `$HOME`, the repo and the last applied state |
| `dotman apply` | Write the repo's files into `$HOME` |
| `dotman diff` | Show how `$HOME` differs from the repo |
| `dotman edit` | Open a managed file's source in `$VISUAL` or `$EDITOR`, then apply it |
| `dotman sync` | Capture, commit, pull, apply and push |
| `dotman log` | Show the repo's history |
| `dotman restore` | Write an earlier version of a file, backing up the current one |
| `dotman import` | Convert another setup into the repo |
| `dotman git` | Run git in the repo |
| `dotman doctor` | Check the environment and the repo |
| `dotman key` | Create or import the age identity |
| `dotman completion` | Generate shell completions |

Destructive commands take `--dry-run`, which changes nothing. Exit codes: 1 error, 3 some paths were left undecided, held, blocked or reported for action, 4 a rebase conflict (`sync` aborts it and prints the steps to resolve it), 0 otherwise.

Backups go to `$XDG_STATE_HOME/dotman/backups/`.

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

`[hooks]` in `.dotman.toml` maps a glob over managed paths to a shell command, for example `".config/hypr/**" = "hyprctl reload"`. After `apply` or `sync` changes a matching path, the command runs once with `$HOME` as the working directory.

A hook table needs approval on each machine. The prompt shows every command, and `--approve-hooks` answers it. Approved hooks run in unattended `sync`; a table that changed after a pull is skipped with a warning until you approve it again. `init <url>` runs hooks only with `--approve-hooks`. `--no-hooks` never runs them. Approval covers the command text, not the scripts it calls.

## System files

Files outside `$HOME`, such as `/etc/keyd/default.conf`, are opt-in. Nothing happens until the machine config lists `[system] roots = ["/etc"]` (`init --system-root /etc`, or answer the prompt in `add --system`). A cloned repo cannot enable it.

```bash
dotman add --system /etc/keyd/default.conf
dotman apply --system
```

- A file owned by a package needs `--package-owned`. Files you cannot read are refused, since system scope does not encrypt.
- `apply --system` (and `sync --system`) shows each path with its diff, owner and mode, asks once, then escalates once through `sudo`, `doas` or `run0` to write.
- Never allowed: `/`, `/proc`, `/sys`, `/dev`, `/run`, `/tmp`, `/var/tmp`, and on macOS the SIP-protected paths.
- `sync` captures local edits to system files but does not write them without `--system`.

See [SECURITY.md](SECURITY.md) before adding a sudoers rule.

## Secrets

`dotman add --encrypt <file>` stores the file as age ciphertext, encrypted to every recipient in `.dotman.toml`. `apply` decrypts it to `$HOME` at mode 0600 unless you set `[modes]` by hand. Plaintext is never written into the repo.

Without a key, encrypted files are reported as `locked` and skipped.

```bash
dotman key init                  # new identity at ~/.config/dotman/age-identity.txt
dotman key import key.txt        # use an existing identity; --add-recipient adds its recipient
```

On a machine where you cannot place a file, set `DOTMAN_AGE_KEY` to the key text, or `DOTMAN_AGE_KEY_FILE` to a path. The key goes only to age's stdin and is stripped from every other child process. It is refused in `[template] env`. Without an env var, the identity file is `$XDG_CONFIG_HOME/dotman/age-identity.txt`.

Back the identity up now: keep it in a password manager and one offline copy. If every identity is lost, the encrypted data is lost, and no one can recover it. Add a second recipient (`dotman key init` on another machine, or `dotman key import --add-recipient`); `dotman doctor` warns while there is only one. The next `sync` on a machine with a key re-encrypts every file to the new set.

dotman refuses to add a file that contains `AGE-SECRET-KEY-`, and holds back new files whose names look like secrets (`.env`, `*.pem`, `id_ed25519*` and so on; the list is `[guard] secret_names`). When the remote is public, `add` refuses those names unless you pass `--encrypt` or `--allow-plain`.

## Codespaces, Dev Containers and DevPod

`init` writes an `install.sh` at the repo root. It downloads a pinned, checksum-verified static `dotman` and runs a non-interactive `dotman init` on the checkout. Point each tool at the dotfiles repo:

- **GitHub Codespaces:** Settings, Codespaces, enable "Automatically install dotfiles" and choose the repo.
- **VS Code Dev Containers:** set `dotfiles.repository` (and optionally `dotfiles.installCommand`) in your VS Code user settings.
- **DevPod:** `devpod context set-options -o DOTFILES_URL=<repo url>`, or Settings, Dotfiles.

For encrypted files, add a `DOTMAN_AGE_KEY` Codespaces secret holding the identity text. Whether the secret reaches `install.sh` is not documented by GitHub. If it did not, the encrypted files show as `locked`: open a terminal that has the key and run `dotman apply`.

Containers that run as root are handled by the script. Set `DOTMAN_APPROVE_HOOKS=1` in the environment to let it approve the repo's hooks. Refresh an old script with `dotman init --refresh-bootstrap`; `dotman doctor` warns when it is stale.

## Prompt-answer flags

Every prompt has a flag, so unattended runs never guess. A run is non-interactive when stdin is not a TTY or `DOTMAN_NON_INTERACTIVE` is set; unanswered paths are skipped and listed, and the run exits 3.

| Prompt | Flag |
|---|---|
| Conflicts and existing files | `--keep-local` or `--take-repo` |
| Local deletions | `--forget-deleted` or `--restore-deleted` |
| Machine identity changed | `--accept-identity` |
| Hook approval | `--approve-hooks` |
| Overlay: add missing files | `--include-missing` |
| System apply, symlink swap, directory preview | `--yes` |
| System root in `add --system` | `--add-root` |
| Source choice in `init` | `--no-import` |
| `init` questions | `--git-name`, `--git-email`, `--git-global`, `--remote`, `--create-github <owner/name>`, `--no-remote`, `--host`, `--profile`, `--system-root`, `--no-system`, `--create-key`, `--no-key`, `--age-key-file` |
| Replacing an identity | `--force` (backs up the old one first) |
| Adding a recipient | `--add-recipient` |

## Migration

`dotman import [<source>]` converts a setup into a fresh repo. It never modifies the source and reports every entry as converted or needing action (exit 3 if any need it). `--dry-run` prints the report and changes nothing. `dotman init` with no argument detects the setups below and offers the import; `--no-import` skips that.

- **Legacy dotman** (`~/.dotman` from before 0.1.0): converts every commit, keeping parents, author, time and message. `--overlay <source>` instead adds what the current repo lacks as one commit: it restores modes, and adds missing files only with `--include-missing`.
- **yadm, bare repos and vcsh, or any git URL**: for trees laid out relative to `$HOME`. yadm `##` alternates become variants, and its default-processor templates become `{tmpl}`. Other templates, yadm encrypt archives and precedence differences are reported. yadm links are replaced by `--swap`. Further vcsh repos go in through `--overlay`.
- **chezmoi**: plain files and symlinks only. Templates never run by default. `.chezmoiignore` is read as text, so any file it might exclude is reported, not imported. `--render-templates` opts in to running the real chezmoi after a warning.
- **Symlink farms** (Stow, dotbot, rcm and similar): content is read through the links. `--swap` then replaces the links with real files, recording them so a failure rolls back. Home-manager links into `/nix/store` are report-only.

## Requirements

- git 2.25 or newer.
- age 1.1 or newer, or rage, for encrypted files (optional).
- Rust 1.88 or newer, to build from source.
- Linux or macOS. Native Windows is not supported; WSL counts as Linux.
- dotman refuses to run as root unless you pass `--allow-root` or set `DOTMAN_ALLOW_ROOT=1`.
- Paths follow XDG: the repo is `$XDG_DATA_HOME/dotman`, state is `$XDG_STATE_HOME/dotman`, and the machine config is `$XDG_CONFIG_HOME/dotman/config.toml`. `DOTMAN_REPO_PATH` and `DOTMAN_CONFIG_PATH` override the first and last.

## Limitations

- Two machines with keys that re-encrypt at the same moment can hit a sync conflict. Syncing again resolves it.
- Windows is not supported.

## Contributing

See [AGENTS.md](AGENTS.md) for the layout, rules and commands. Run `just lint-strict` and `cargo test --workspace --all-targets` before opening a pull request. See [SECURITY.md](SECURITY.md) for vulnerability reporting.

## License

MIT License. See [LICENSE](LICENSE).
