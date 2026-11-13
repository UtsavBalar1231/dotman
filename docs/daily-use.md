# Daily use

The commands, what `status` and `sync` do, backups, ignore patterns and the flags that answer prompts. Back to the [README](../README.md).

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

Running `dotman init` again on a repo that is set up changes nothing, except the remote when you pass `--remote`. `dotman init <url>` run again over the repo it cloned from that URL, as after an interrupted run, finishes setting up this machine: it writes the machine config, imports `--age-key-file` and applies the repo. A URL has to match the repo's origin exactly. A local path, relative to where you run the command or not, matches when it names the same directory as the origin, which git reads relative to the repo; that is also why `--remote` stores a local path made absolute, except one starting with `~`, which git expands itself. A local checkout that has an origin of its own does not match when you run `dotman init <checkout>` again: the clone stores the checkout's origin, not its path, so run `dotman init` with that origin's URL to finish the setup. It refuses any other repo already there; `dotman apply` applies that one.

`dotman diff` runs from the repo to `$HOME`, so an edit you made in `$HOME` shows as `+` lines.

Destructive commands take `--dry-run`, which changes nothing.

Exit codes: 0 success; 1 an error; 2 a command line that cannot be parsed; 3 the run finished but left paths for you (undecided, held back, blocked, or reported for action); 4 a rebase conflict, which `sync` aborts before it prints the steps to resolve it, or a git operation or conflict left unfinished in the repo. `dotman status` exits 0 even when it lists paths to look at, and `dotman git` exits with git's own status. `man dotman` and the `--help` output list the exit codes and the environment variables.

`dotman restore <file> --from` takes a commit (a hash, `HEAD~2`, a branch) or a date: `YYYY-MM-DD`, optionally followed by a space or `T` and `HH:MM` or `HH:MM:SS`, read as local time; `N.units.ago` or `N units ago`, from seconds to years; or `yesterday`. Anything else, such as a mistyped revision, is an error, not a guess.

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

Modes count as content. `add` records a private file's mode, such as 0600, in `[modes]` in `.dotman.toml`, because git keeps only 0644 and 0755. A capture or an import leaves group and other write bits out of what it records, for files and directories alike: a 0664 file or 0775 directory, as umask 002 makes them, records nothing, so other machines get 0644 or 0755 while this one keeps its mode until the repo's version next replaces the file; a 0660 file records 0640, which this machine then applies too. A write bit that a `[modes]` entry already holds, as one you wrote by hand, stays while the file keeps it and goes from the entry once the file loses it: with `".shared" = "0664"`, a capture of a 0666 file records 0664 and one of a 0644 file removes the entry. When the repo's version replaces a file in `$HOME`, group and others gain no read or write bit the file lacked, and get the repo's execute bit only where they can read the file or could already execute it. A stricter mode kept this way stays on this machine: `status` notes "kept at 0600 here; the repo says 0644", the file reads as clean, and nothing is committed. The repo's mode applies as it is when a `[modes]` entry sets it, which is meant for every machine, or when the repo's mode changed since this machine last applied it, as when another machine loosened it on purpose.

## Ignoring files

`[ignore] patterns` in `.dotman.toml` keeps files under managed directories out of the repo. The defaults are `.git/`, `*.swp`, `*~`, `.DS_Store`, `__pycache__/`, `node_modules/`, `*.orig` and `*.rej`, at any depth. As with git's ignore rules, a pattern applies only to files the repo does not track yet: a tracked file keeps syncing. `add <dir>` skips ignored files, and `add <file>` adds an ignored file you name, with a warning. Removing a pattern takes effect on a machine that has seen it only after `sync --drop-unlisted-ignores` runs there.

## Prompt-answer flags

Every prompt has a flag, so unattended runs never guess. A run is non-interactive when stdin is not a TTY or `DOTMAN_NON_INTERACTIVE` is set; unanswered paths are skipped and listed, and the run exits 3.

| Prompt | Flag |
|---|---|
| Conflicts and existing files | `--keep-local` or `--take-repo` |
| Local deletions, including a file deleted here and changed upstream | `--forget-deleted` or `--restore-deleted` |
| Machine identity changed | `--accept-identity` |
| Hook approval | `--approve-hooks` |
| `sync` capturing under `[dirs]` entries this machine has not approved | `--approve-dirs` |
| A `sync` commit that deletes most tracked files | `--allow-mass-delete` |
| Overlay: add missing files | `--include-missing` |
| System apply, import, symlink swap, directory preview | `--yes` |
| System root in `add --system` | `--add-root` |
| Source choice in `init` | `--no-import` |
| `init` questions | `--git-name`, `--git-email`, `--git-global`, `--remote`, `--create-github <owner/name>`, `--no-remote`, `--host`, `--profile`, `--system-root`, `--no-system`, `--create-key`, `--no-key`, `--age-key-file` |
| Adding a recipient | `--add-recipient` |

`key import` and `init --age-key-file` refuse to replace a different identity; `--force` replaces it after backing up the old one.
