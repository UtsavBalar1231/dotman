# Getting started

Setting up the first machine and the next ones, and what dotman needs. Back to the [README](../README.md).

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

When the remote's HEAD names a branch with no commits, as a bare repo made with git's default `master` does after dotman pushed `main`, `init` checks out the remote's branch: its only branch, or `main` when it has several.

From then on, edit files where they are and run `dotman sync`. It captures local edits, commits, pulls, applies what arrived and pushes. `dotman status` shows what differs, and `dotman diff` shows how.

A directory you add, such as `~/.config/nvim`, goes into `[dirs]` in `.dotman.toml`, and `sync` captures the new files that appear under it. Each machine approves a `[dirs]` entry before its own new files under it are captured: `add <dir>` approves the directory it adds, and on the second machine the first `sync` with new files under an entry from elsewhere asks, or with `--approve-dirs` approves without asking. Until then those files are held, and the run exits 3. The managed files under the entry apply and sync as usual meanwhile. The approval lives in this machine's `state.toml`, per entry; `forget <dir>` drops it, so an entry forgotten here and listed again later is asked about again. A state written by an older dotman approves the entries `.dotman.toml` lists when it is first read, because that machine was capturing under them already.

`[ignore]` patterns are recorded the same way. `state.toml` keeps each pattern this machine has seen in `.dotman.toml`, and the capture under `[dirs]` goes on applying one after the file drops it, whether the edit came from another machine or was made here, so a removed pattern does not turn what it kept out into new files. `status` lists what only such a pattern keeps out, `add <file>` adds one of those files, and `sync --drop-unlisted-ignores` stops applying, on this machine, the patterns `.dotman.toml` no longer lists and captures what they kept out.

## Requirements

- git 2.25 or newer, on `PATH`: every command but `completion` runs it.
- age or rage, for encrypted files (optional). `key init` and `key import` also need `age-keygen` or `rage-keygen`.
- Rust 1.88 or newer, to build from source.
- Linux. macOS is the other target, but it has no release binaries and has not been built or tested yet: there is no Mac to test on, and CI has not run on one. Native Windows is not supported; WSL counts as Linux.
- dotman refuses to run as root unless you pass `--allow-root` or set `DOTMAN_ALLOW_ROOT=1`.
- Two managed files whose names differ only in case or Unicode form are refused: `status` reports them, and `apply` and `sync` stop before writing, because a macOS filesystem cannot hold both.
- Some names cannot be managed, and `add` says why: a name that is not in Unicode NFC form, a name with a control character (C0, DEL or C1) or with a bidi control or an invisible format character (U+061C, U+200E, U+200F, U+202A to U+202E and U+2066 to U+2069, or such as U+200B, U+FEFF and U+00AD), a `.git` component in any spelling git refuses (`.GIT`, `.git` with trailing spaces or dots, `.git:stream`, `git~1`), a name that starts with `.dotman-tmp-`, which dotman uses for its own temp files, and the repo root names `.dotman.toml`, `.dotman`, `install.sh`, `README*` and `LICENSE*`. Nor can dotman's own files on this machine, whatever their case: the machine config, `$XDG_CONFIG_HOME/dotman`, the age identity, `$XDG_STATE_HOME/dotman` and the repo, a file or symlink above one of them, or a variant that targets one. These follow the machine's own XDG dirs, so a repo file at `.config/dotman/config.toml` is refused on a machine with the default `XDG_CONFIG_HOME`, wherever the machine that pushed it keeps its config.
- A tracked file with a name dotman refuses, as another machine can push with plain git, or an older dotman could track before a rule refused it, is never applied or captured. `init`, `apply` and `sync` name it and exit 3, with the `dotman git rm` that removes it from the repo, which for a name output has to escape is a `printf` piped to `xargs -0`, then `dotman sync`. A state entry for one is dropped with a note, and the file in `$HOME` stays.
- Paths follow XDG: the repo is `$XDG_DATA_HOME/dotman`, state is `$XDG_STATE_HOME/dotman`, and the machine config is `$XDG_CONFIG_HOME/dotman/config.toml`. `DOTMAN_REPO_PATH` and `DOTMAN_CONFIG_PATH` override the first and last.
- When dotman saves `config.toml`, as `init` and `add --allow-plain` do, it rewrites only the values that changed and keeps your comments, layout and the keys it does not know.

## Codespaces, Dev Containers and DevPod

`init` writes an `install.sh` at the repo root. It downloads a pinned, checksum-verified static `dotman` and runs a non-interactive `dotman init` on the checkout. Point each tool at the dotfiles repo:

- **GitHub Codespaces:** Settings, Codespaces, enable "Automatically install dotfiles" and choose the repo.
- **VS Code Dev Containers:** set `dotfiles.repository` (and optionally `dotfiles.installCommand`) in your VS Code user settings.
- **DevPod:** `devpod context set-options -o DOTFILES_URL=<repo url>`, or Settings, Dotfiles.

For encrypted files, add a `DOTMAN_AGE_KEY` Codespaces secret holding the identity text. Whether the secret reaches `install.sh` is not documented by GitHub. If it did not, the encrypted files show as `locked`: open a terminal that has the key and run `dotman apply`.

Containers that run as root are handled by the script. Set `DOTMAN_APPROVE_HOOKS=1` in the environment to let it approve the repo's hooks. When the table changed since this machine last showed one, the first approval only shows it, so the script runs `dotman apply --approve-hooks` once more, and exits 1 if that approves nothing either. Refresh an old script with `dotman init --refresh-bootstrap`; `dotman doctor` warns when it is stale.
