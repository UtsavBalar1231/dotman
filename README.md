# dotman

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![CI](https://img.shields.io/github/actions/workflow/status/UtsavBalar1231/dotman/ci.yml?branch=main)](https://github.com/UtsavBalar1231/dotman/actions)

A dotfiles manager with no learning curve, safe by default, for Linux and macOS.

Your dotfiles live in an ordinary git repo. dotman copies them into `$HOME`, notices which side changed, and asks before it overwrites anything. A file it replaces in `$HOME` is backed up first. It never writes through a symlink, never runs a hook you have not approved, and never runs a template command.

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

From then on, edit files where they are and run `dotman sync`. It captures local edits, commits, pulls, applies what arrived and pushes. `dotman status` shows what differs, and `dotman diff` shows how.

## Commands

| Command | Purpose |
|---|---|
| `dotman init` | Create the repo or set its remote, or clone one and apply it |
| `dotman add` | Start managing files or directories |
| `dotman forget` | Stop managing files, keeping them in `$HOME` |
| `dotman list` | List the managed files |
| `dotman status` | Compare `$HOME`, the repo and the last applied state |
| `dotman apply` | Write the repo's files into `$HOME` |
| `dotman diff` | Show how `$HOME` differs from the repo, as a diff from `repo/` to `home/` |
| `dotman edit` | Open a managed file's source in `$VISUAL` or `$EDITOR`, then apply it |
| `dotman sync` | Capture, commit, pull, apply and push |
| `dotman log` | Show the repo's history |
| `dotman restore` | Write an earlier version of a file, backing up the current one |
| `dotman import` | Convert another setup into the repo |
| `dotman git` | Run git in the repo |
| `dotman doctor` | Check the environment and the repo |
| `dotman key` | Create or import the age identity |
| `dotman completion` | Generate shell completions |

Running `dotman init` again on a repo that is set up changes nothing, except the remote when you pass `--remote`. `dotman init <url>` run again over the repo it cloned from that URL, as after an interrupted run, finishes setting up this machine: it writes the machine config, imports `--age-key-file` and applies the repo. It refuses any other repo already there; `dotman apply` applies that one.

`dotman diff` runs from the repo to `$HOME`, so an edit you made in `$HOME` shows as `+` lines.

Destructive commands take `--dry-run`, which changes nothing.

Exit codes: 0 success; 1 an error; 2 a command line that cannot be parsed; 3 the run finished but left paths for you (undecided, held back, blocked, or reported for action); 4 a rebase conflict, which `sync` aborts before it prints the steps to resolve it, or a git operation or conflict left unfinished in the repo. `dotman status` exits 0 even when it lists paths to look at, and `dotman git` exits with git's own status. `man dotman` and the `--help` output list the exit codes and the environment variables.

## Status

`dotman status` lists each managed path that is not clean, with its state, then the new files under managed directories that `sync` would capture, the files held back, and the repo edits that `sync` holds ("held in the repo"). It ends with counts, such as `4 managed, 4 clean, 1 to look at (0 new, 1 held back)`, where a held file counts as to look at but not as managed, and with the commits to push or pull as of the last fetch: `1 commit to pull: run dotman sync`. When `origin` has no branch yet, it counts the commits `origin` does not have: `2 commits not pushed yet: run dotman sync`; but before anything was ever fetched from `origin`, as when every push failed since `init --remote`, it cannot tell which commits `origin` already has, and says `origin has not been fetched yet; dotman sync fetches and pushes`. A file in the repo whose name is not valid UTF-8 cannot be a managed path, so `status` names it, or counts them past one, and `sync` does not commit it; under the repo root names `.dotman`, `README*` and `LICENSE*`, which are repo content and not managed paths, `sync` commits it. It takes no lock and writes nothing.

`dotman status --porcelain` prints one `<token>\t<key>\0` record per path. A template that does not render gets `template-error`. `dotman status --help` lists the tokens and what each one means.

## Sync

`dotman sync` captures local edits, commits them, pulls with rebase, applies what arrived and pushes, then says how many commits it pushed.

- Before the pull it fetches the branch, one fetch more than the pull alone, and refuses (exit 1) when the remote shares no commit with the repo: its history was replaced, or it is another repo. The message prints the commands that start over from the remote and keep what only this repo has.
- A pull that conflicts is aborted, nothing it brought reaches `$HOME`, and the run exits 4 with numbered steps to paste: pull again, resolve the files under the repo, `dotman git add`, `dotman git -c core.editor=true rebase --continue`, `dotman sync`. An encrypted file cannot be merged, so its steps keep one side whole.
- A commit that would delete at least 10 tracked files, and at least half of them, looks like a broken checkout, so sync refuses it. An interactive run asks first. Otherwise pass `--allow-mass-delete`, or bring the files back with `dotman git checkout -- .`. A deleted file under a `[dirs]` entry that `$HOME` still has would be captured again by the next sync, so for those the refusal gives `dotman forget <dir>` first, then `dotman sync --allow-mass-delete`.
- Keeping `$HOME`'s version of a conflicting or existing file, with `--keep-local` or `[k]`, captures it into the repo for every machine. Before you answer, the question says so and prints the `dotman add --for host=<host> <file>` that keeps the file to this machine only: answer `[s]` and run that instead. When any source of the file is encrypted, for whichever machine, the command is `dotman add --encrypt --for host=<host> <file>`, and a plain `--for` variant is refused. When the variant this machine applies already names its host, such as `.zshrc{host=laptop+os=linux}`, keeping its version captures into that variant, which no other machine applies, and no command is printed for it. A run that cannot ask prints the same command under the paths it left undecided.
- An uncommitted repo edit that holds a credential or an age key stops sync before the pull (exit 3), because the pull would set it aside and put it back over what arrives.
- A git lock file in the repo, such as the `index.lock`, `MERGE_MSG.lock` or `AUTO_MERGE.lock` a killed git leaves, stops sync with the `rm` command for each one. `dotman doctor` reports the same file when no dotman is running.
- A `.gitignore` tracked in a managed directory does not decide what dotman commits. Files it ignores go through `[ignore]` and the guards like any other file, and a file you add by name is committed even when it matches.
- Without a remote, sync commits and applies, and says how to add one.

## Backups and modes

Before a write replaces a file in `$HOME` whose content differs, dotman copies it to `$XDG_STATE_HOME/dotman/backups/<UTC time>-<pid>/<key>` and records its mode in `.dotman/modes.txt` in the same directory. A change of mode alone is recorded in `modes.txt` without a copy, and a replaced symlink in `.dotman/links.txt`. A backup copy is never overwritten: a file backed up by both passes of one `sync` keeps both copies. When keeping `$HOME`'s version, with `--keep-local` or at the prompt, would replace an uncommitted edit in the repo, that edit is saved under `.dotman/repo/` first. `dotman forget`, and forgetting a file deleted here (`--forget-deleted`, or at the prompt), also save under `.dotman/repo/` each repo file they remove whose bytes are not in the last commit, in `$HOME` or in the last applied state. When a copy cannot be saved, nothing is removed.

Backups of encrypted files hold the plaintext. Backups are never pruned; `dotman doctor` shows their size and counts the plaintext copies, and you remove the runs you no longer need.

Modes count as content. `add` records a private file's mode, such as 0600, in `[modes]` in `.dotman.toml`, because git keeps only 0644 and 0755. A capture leaves group and other write bits out of what it records, for files and directories alike: a 0664 file or 0775 directory, as umask 002 makes them, records nothing and keeps its mode on this machine only, and a 0660 file records 0640, which this machine then applies too. When the repo's version replaces a file in `$HOME`, group and others gain no read or write bit the file lacked, and get the repo's execute bit only where they can read the file or could already execute it. A stricter mode kept this way stays on this machine: `status` notes "kept at 0600 here; the repo says 0644", the file reads as clean, and nothing is committed. The repo's mode applies as it is when a `[modes]` entry sets it, which is meant for every machine, or when the repo's mode changed since this machine last applied it, as when another machine loosened it on purpose.

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

The `.dotman.toml` that dotman writes has an empty `[hooks]` table. Add entries to it that map a glob over managed paths to a shell command, for example `".config/hypr/**" = "hyprctl reload"`. A second `[hooks]` header is a TOML error. After `apply`, `sync` or `edit` writes a matching path into `$HOME`, the command runs once through `/bin/sh -c`, with `$HOME` as the working directory and its output on stderr. A command may span several lines in a `"""` string. A carriage return, escape or other control character, other than tab and newline, is refused: no hook runs, none is approved, and the run exits 1.

A hook table needs approval on each machine. The prompt shows every command, and `--approve-hooks` answers it. The approval is the table itself, so a changed command or glob needs approval again, while spacing and order in the file do not. An approval saved by an older dotman, which stored a hash, is asked for again. Approved hooks run in unattended `sync`; a table that changed after a pull is skipped with a warning until you approve it again. A hook held back because its table was not approved runs once when the table is approved. The warning prints the whole table, and that table is what the printed `--approve-hooks` approves. If the table changed since that warning, `--approve-hooks` approves nothing: it shows the new table and exits 3, and the same command run again approves the table it showed. With no such warning recorded, `--approve-hooks` approves the current table, printing it first. `init <url>` runs hooks only with `--approve-hooks`, and `--no-hooks` runs none. Approval covers the command text, not the scripts it calls.

## Ignoring files

`[ignore] patterns` in `.dotman.toml` keeps files under managed directories out of the repo. The defaults are `.git/`, `*.swp`, `*~`, `.DS_Store`, `__pycache__/`, `node_modules/`, `*.orig` and `*.rej`, at any depth. As with git's ignore rules, a pattern applies only to files the repo does not track yet: a tracked file keeps syncing. `add <dir>` skips ignored files, and `add <file>` adds an ignored file you name, with a warning.

## System files

Files outside `$HOME`, such as `/etc/keyd/default.conf`, are opt-in. Nothing happens until the machine config lists `[system] roots = ["/etc"]` (`init --system-root /etc`, or answer the prompt in `add --system`). A cloned repo cannot enable it.

```bash
dotman add --system /etc/keyd/default.conf
dotman apply --system
```

- A file owned by a package needs `--package-owned`. Files you cannot read are refused, and so are files the content guard holds, since system scope does not encrypt.
- `apply --system` (and `sync --system`) shows each path with its diff, owner and mode, asks once, then escalates once through `sudo`, `doas` or `run0` to write.
- Never allowed: `/`, `/proc`, `/sys`, `/dev`, `/run`, `/tmp`, `/var/tmp`, and on macOS the SIP-protected paths.
- `sync` captures local edits to system files but does not write them without `--system`.

See [SECURITY.md](SECURITY.md) before adding a sudoers rule, and install dotman root-owned on a machine that uses system files.

## Secrets

`dotman add --encrypt <file>` stores the file as age ciphertext, encrypted to every recipient in `.dotman.toml`. `apply` decrypts it to `$HOME` at mode 0600 unless you set `[modes]` by hand. Plaintext is never written into the repo.

Without a key, encrypted files are reported as `locked` and skipped.

```bash
dotman key init                  # new identity at ~/.config/dotman/age-identity.txt
dotman key import key.txt        # use an existing identity; --add-recipient adds its recipient
```

On a machine where you cannot place a file, set `DOTMAN_AGE_KEY` to the key text, or `DOTMAN_AGE_KEY_FILE` to a path. The key goes only to age's stdin and is stripped from every other child process. It is refused in `[template] env`. `DOTMAN_AGE_KEY` works with any age or rage that reads an identity from `-i -`: dotman checks that by decrypting a built-in test file, so it needs no `age-keygen`. Without an env var, the identity file is `$XDG_CONFIG_HOME/dotman/age-identity.txt`.

Back the identity up now: keep it in a password manager and one offline copy. Without an identity, the encrypted data cannot be read, and no one can recover it. Add a second recipient (`dotman key init` on another machine, or `dotman key import --add-recipient`); `dotman doctor` warns while there is only one. When `[age] recipients` changes, here or upstream, the next `sync` on a machine whose key decrypts every encrypted file re-encrypts them all to the new set, without asking.

### The guards

Two guards keep secrets out of the repo, whatever the remote.

- **The content guard** looks for 20 kinds of credential that start with a fixed provider prefix: GitHub tokens of both kinds, GitLab, Slack tokens and webhooks, AWS access key IDs, Stripe live keys, OpenAI, Anthropic, npm, PyPI, DigitalOcean, SendGrid, Hugging Face, PEM private keys, OpenRouter, Groq, xAI, Perplexity and Docker. It also looks for an age identity: `AGE-SECRET-KEY-1` and 58 more characters, so the bare prefix in a script is not held. A finding names the rule and the line, never the value. A file with a NUL byte in its first 8000 bytes counts as binary and is not scanned.
- **The name guard** holds new files whose names match `[guard] secret_names` in `.dotman.toml`. The defaults are `.netrc`, `.env`, `.env.*`, `*.pem`, `*.key`, `id_rsa*`, `id_ed25519*`, `id_ecdsa*`, `*.kdbx`, `*.gpg`, `credentials*`, `*token*`, `*secret*`, `gh/hosts.yml`, `*.ppk` and `rclone.conf`, at any depth.

The content guard runs on every plain capture: `add`, new files under a managed directory, `sync` of a local edit, `import`, and the commit `sync` makes, which also covers `dotman edit` and edits made by hand in the repo.

- `add <file>` refuses a file the content guard holds. `--encrypt` stores it encrypted. `--allow-plain` stores it as it is and records the path and the rule under `[guard] allow` in this machine's `config.toml`, so later captures of that file are not held for that rule. An age identity is never stored in plain text, whatever the flags.
- `add <dir>` adds the rest and holds back each file a guard holds, listing it with the command that stores it, and exits 3. `add --allow-plain <dir>` releases files held for their names only; a file that holds a credential has to be named.
- A file you name whose name matches `secret_names` is added with a warning. When the remote is known to be public, `add` refuses it unless you pass `--encrypt` or `--allow-plain`. The remote counts as public when an anonymous `git ls-remote` of its https form succeeds. `sync` asks again once a day, and a check that could not tell is asked again after an hour.
- `sync` holds new files and local edits that a guard holds. It lists them, commits the rest, and exits 3.
- `import` leaves out the files the content guard holds and exits 3.

`[guard] allow` is per machine. Entries found in `.dotman.toml` are ignored, with a warning, so an entry another machine pushed never lets a credential out of this one.

The guards miss credentials without a known prefix, such as a password or a generic API key in a config file, and anything in a binary file. Name secret files so that `secret_names` matches them.

## Codespaces, Dev Containers and DevPod

`init` writes an `install.sh` at the repo root. It downloads a pinned, checksum-verified static `dotman` and runs a non-interactive `dotman init` on the checkout. Point each tool at the dotfiles repo:

- **GitHub Codespaces:** Settings, Codespaces, enable "Automatically install dotfiles" and choose the repo.
- **VS Code Dev Containers:** set `dotfiles.repository` (and optionally `dotfiles.installCommand`) in your VS Code user settings.
- **DevPod:** `devpod context set-options -o DOTFILES_URL=<repo url>`, or Settings, Dotfiles.

For encrypted files, add a `DOTMAN_AGE_KEY` Codespaces secret holding the identity text. Whether the secret reaches `install.sh` is not documented by GitHub. If it did not, the encrypted files show as `locked`: open a terminal that has the key and run `dotman apply`.

Containers that run as root are handled by the script. Set `DOTMAN_APPROVE_HOOKS=1` in the environment to let it approve the repo's hooks. When the table changed since this machine last showed one, the first approval only shows it, so the script runs `dotman apply --approve-hooks` once more, and exits 1 if that approves nothing either. Refresh an old script with `dotman init --refresh-bootstrap`; `dotman doctor` warns when it is stale.

## Prompt-answer flags

Every prompt has a flag, so unattended runs never guess. A run is non-interactive when stdin is not a TTY or `DOTMAN_NON_INTERACTIVE` is set; unanswered paths are skipped and listed, and the run exits 3.

| Prompt | Flag |
|---|---|
| Conflicts and existing files | `--keep-local` or `--take-repo` |
| Local deletions, including a file deleted here and changed upstream | `--forget-deleted` or `--restore-deleted` |
| Machine identity changed | `--accept-identity` |
| Hook approval | `--approve-hooks` |
| A `sync` commit that deletes most tracked files | `--allow-mass-delete` |
| Overlay: add missing files | `--include-missing` |
| System apply, import, symlink swap, directory preview | `--yes` |
| System root in `add --system` | `--add-root` |
| Source choice in `init` | `--no-import` |
| `init` questions | `--git-name`, `--git-email`, `--git-global`, `--remote`, `--create-github <owner/name>`, `--no-remote`, `--host`, `--profile`, `--system-root`, `--no-system`, `--create-key`, `--no-key`, `--age-key-file` |
| Adding a recipient | `--add-recipient` |

`key import` and `init --age-key-file` refuse to replace a different identity; `--force` replaces it after backing up the old one.

## Migration

`dotman import [<source>]` converts a setup into a fresh repo. It never modifies the source and reports every entry as converted or needing action (exit 3 if any need it). `--dry-run` prints the report and changes nothing. `dotman init` with no argument detects the setups below and offers the import; `--no-import` skips that.

- **Legacy dotman** (`~/.dotman` from before 0.1.0): converts every commit, keeping parents, author, time and message. `[dirs]` stays empty, because the first sync would capture every file in those directories: the directories the old store tracked are listed with the `dotman add` command for each, to run once you have added `[ignore]` patterns. `--overlay <source>` instead adds what the current repo lacks as one commit: it restores modes, and adds missing files only with `--include-missing`.
- **yadm, bare repos and vcsh, or any git URL**: for trees laid out relative to `$HOME`. yadm `##` alternates become variants, and its default-processor templates become `{tmpl}`. Other templates, yadm encrypt archives and precedence differences are reported. yadm links are replaced by `--swap`. Further vcsh repos go in through `--overlay`.
  - Only the history of the branch the source's `HEAD` names comes in: no other branch and no tag. A source whose `HEAD` is detached comes in on a new branch, `main`, and the report says so. The new repo then has no remote, even from a URL, since sync would pull the source's own `main` and push to it; set a new, empty one with `dotman init --remote <url>`.
  - When a file or a commit message in that history holds what the content guard holds, the import is one commit of the current tree, with no remote set, and a note names each file, rule and commit, and for a commit, the part that holds it: the message, the author or committer, a merged tag or the signature. Push it to a new, empty repo, not to the source: the source still holds the history the import left out and shares no commit with it. `--keep-history` imports every commit anyway.
  - A file in `$HOME` with the same bytes as the source and a stricter owner-only mode gets that mode in `[modes]`. When the source has its own `.dotman.toml`, that file is kept byte for byte and the `[modes]` lines to add are printed instead.
- **chezmoi**: plain files and symlinks only. Templates never run by default. `.chezmoiignore` is read as text, so any file it might exclude is reported, not imported. `--render-templates` opts in to running the real chezmoi after a warning.
- **Symlink farms** (Stow, dotbot, rcm and similar): content is read through the links. `--swap` then replaces the links with real files, recording them so a failure rolls back. Home-manager links into `/nix/store` are report-only.

## Requirements

- git 2.25 or newer, on `PATH`: every command but `completion` runs it.
- age or rage, for encrypted files (optional). `key init` and `key import` also need `age-keygen` or `rage-keygen`.
- Rust 1.88 or newer, to build from source.
- Linux. macOS is the other target, but it has not been tested yet: there is no Mac to test on, and CI has not run on one. Native Windows is not supported; WSL counts as Linux.
- dotman refuses to run as root unless you pass `--allow-root` or set `DOTMAN_ALLOW_ROOT=1`.
- Two managed files whose names differ only in case or Unicode form are refused: `status` reports them, and `apply` and `sync` stop before writing, because a macOS filesystem cannot hold both.
- Some names cannot be managed, and `add` says why: a name that is not in Unicode NFC form, a name with a control character (C0, DEL or C1), a `.git` component in any spelling git refuses (`.GIT`, `.git` with trailing spaces or dots, `.git:stream`, `git~1`), a name that starts with `.dotman-tmp-`, which dotman uses for its own temp files, and the repo root names `.dotman.toml`, `.dotman`, `install.sh`, `README*` and `LICENSE*`.
- Paths follow XDG: the repo is `$XDG_DATA_HOME/dotman`, state is `$XDG_STATE_HOME/dotman`, and the machine config is `$XDG_CONFIG_HOME/dotman/config.toml`. `DOTMAN_REPO_PATH` and `DOTMAN_CONFIG_PATH` override the first and last.
- When dotman saves `config.toml`, as `init` and `add --allow-plain` do, it rewrites only the values that changed and keeps your comments, layout and the keys it does not know.

`dotman restore <file> --from` takes a commit (a hash, `HEAD~2`, a branch) or a date: `YYYY-MM-DD`, optionally followed by a space or `T` and `HH:MM` or `HH:MM:SS`, read as local time; `N.units.ago` or `N units ago`, from seconds to years; or `yesterday`. Anything else, such as a mistyped revision, is an error, not a guess.

## Limitations

- Two machines with keys that re-encrypt at the same moment can hit a sync conflict. Syncing again resolves it.
- Windows is not supported.

## Contributing

See [AGENTS.md](AGENTS.md) for the layout, rules and commands. Run `just lint-strict`, `cargo test --workspace --all-targets` and `just e2e`, the multi-machine, kill-recovery and importer scenarios in `e2e/`, before opening a pull request; `just e2e-distros` repeats the journeys in Docker on several distros. See [SECURITY.md](SECURITY.md) for vulnerability reporting.

## License

MIT License. See [LICENSE](LICENSE).
