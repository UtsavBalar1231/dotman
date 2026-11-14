# Daily use

The commands, what `status` and `sync` do, pull conflicts, backups, ignore patterns and the flags that answer prompts. Back to the [README](../README.md).

https://github.com/user-attachments/assets/4068b53a-8dfb-46b2-a1b2-8414b8a904de

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

Exit codes: 0 success; 1 an error; 2 a command line that cannot be parsed; 3 the run finished but left paths for you (undecided, held back, blocked, or reported for action); 4 a pull conflict that `sync` did not settle, so nothing was pulled or pushed, or a git operation, a pull that a stopped sync left, or a conflict unfinished in the repo. `dotman status` exits 0 even when it lists paths to look at, and `dotman git` exits with git's own status. `man dotman` and the `--help` output list the exit codes and the environment variables.

`dotman restore <file> --from` takes a commit (a hash, `HEAD~2`, a branch) or a date: `YYYY-MM-DD`, optionally followed by a space or `T` and `HH:MM` or `HH:MM:SS`, read as local time; `N.units.ago` or `N units ago`, from seconds to years; or `yesterday`. Anything else, such as a mistyped revision, is an error, not a guess.

## Status

`dotman status` lists each managed path that is not clean, with its state, then the new files under managed directories that `sync` would capture, the files held back, the managed files whose edit in `$HOME` the content guard holds ("held in $HOME", which keep their `changed-locally` row and list the commands that end the hold), and the repo edits that `sync` holds ("held in the repo"). It ends with counts, such as `4 managed, 4 clean, 1 to look at (0 new, 1 held back)`, where a held file counts as to look at but not as managed, and a held edit adds `, 1 held in $HOME` or `, 1 held in the repo`, and with the commits to push or pull as of the last fetch: `1 commit to pull: run dotman sync`. When `origin` has no branch yet, it counts the commits `origin` does not have: `2 commits not pushed yet: run dotman sync`; but before anything was ever fetched from `origin`, as when every push failed since `init --remote`, it cannot tell which commits `origin` already has, and says `origin has not been fetched yet; dotman sync fetches and pushes`. A file in the repo whose name is not valid UTF-8 cannot be a managed path, so `status` names it, or counts them past one, and `sync` does not commit it; under the repo root names `.dotman`, `README*` and `LICENSE*`, which are repo content and not managed paths, `sync` commits it. It takes no lock and writes nothing.

`dotman status --porcelain` prints one `<token>\t<key>\0` record per path. A template that does not render gets `template-error`. `dotman status --help` lists the tokens and what each one means.

## Sync

`dotman sync` captures local edits, commits them, pulls with rebase, applies what arrived and pushes. It says how many commits it pulled, before it applies them, and how many it pushed.

- Before the pull it fetches the branch, one fetch more than the pull alone, and refuses (exit 1) when the remote shares no commit with the repo: its history was replaced, or it is another repo. The message prints the commands that start over from the remote and keep what only this repo has.
- A pull that conflicts with your commits is settled before anything it brought reaches `$HOME`, by `--keep-local` or `--take-repo` or by your answers on a terminal. With neither, the run exits 4 and changes nothing. [Pull conflicts](#pull-conflicts) below has the details.
- A commit that would delete at least 10 tracked files, and at least half of them, looks like a broken checkout, so sync refuses it. An interactive run asks first. Otherwise pass `--allow-mass-delete`, or bring the files back with `dotman git checkout -- .`. A deleted file under a `[dirs]` entry that `$HOME` still has would be captured again by the next sync, so for those the refusal gives `dotman forget <dir>` first, then `dotman sync --allow-mass-delete`.
- Keeping `$HOME`'s version of a conflicting or existing file, with `--keep-local` or `[k]`, puts it in the repo for every machine. The capture is immediate when the question comes before the pull, and happens on the next `sync` when it comes after the pull, which keeps the file for now. Before you answer, the question says so and prints the `dotman add --for host=<host> <file>` that keeps the file to this machine only: answer `[s]` and run that instead. When any source of the file is encrypted, for whichever machine, the command is `dotman add --encrypt --for host=<host> <file>`, and a plain `--for` variant is refused. When the variant this machine applies already names its host, such as `.zshrc{host=laptop+os=linux}`, keeping its version captures into that variant, which no other machine applies, and no command is printed for it. A run that cannot ask prints the same command under the paths it left undecided.
- An uncommitted repo edit that holds a credential or an age key stops sync before the pull (exit 3), because the pull would set it aside and put it back over what arrives.
- A git lock file in the repo, such as the `index.lock`, `MERGE_MSG.lock` or `AUTO_MERGE.lock` a killed git leaves, stops sync with the `rm` command for each one. `dotman doctor` reports the same file when no dotman is running.
- A `.gitignore` tracked in a managed directory does not decide what dotman commits. Files it ignores go through `[ignore]` and the guards like any other file, and a file you add by name is committed even when it matches.
- Without a remote, sync commits and applies, and says how to add one.

## Pull conflicts

When the pull conflicts with commits made here, `sync` puts the repo back as it was and looks at each file that clashes. Nothing the pull brought reaches `$HOME` until the conflict is settled. `dotman sync --dry-run` doesn't fetch, so it can't tell you that a pull will conflict.

Some files are settled with no question:

- `.dotman.toml`, which is merged setting by setting (see [Settings](#settings));
- `.dotman/age-recipients` and `install.sh`, which take origin's version, since dotman generates them;
- a file that is now the same on both machines;
- an encrypted file whose two versions decrypt to the same text here, as after two machines re-encrypt at once.

If the commit left a tracked file changed in the repo, or a guard held a capture, `sync` exits 4 before it asks anything. Resolve what the warnings hold, then run `dotman sync` again.

The rest take an answer. `--keep-local` keeps this machine's side and `--take-repo` takes origin's:

- In a text file, plain or `{tmpl}`, the flag picks a side for each clashing line, and both machines' edits elsewhere in the file are kept.
- Encrypted files, binary files and symlinks are taken whole.
- For a file edited here and removed in origin, `--keep-local` keeps managing it with this machine's edit, and `--take-repo` accepts the removal and leaves the file in `$HOME`.
- For a file removed here and edited in origin, `--keep-local` keeps it removed, and `--take-repo` brings it back.

Either flag also answers the other questions the same sync asks, about existing files and files changed on both sides. With no flag and no terminal, `sync` lists the files, prints the commands that answer them and exits 4. Nothing was pulled or pushed, and your files and commits are as they were.

An answer that makes new text, such as a merged file, has to pass the content guard, which holds a credential that neither machine's version had, and a merged template has to render. Under a flag, a refusal exits 4 with nothing changed and says what to edit. On a terminal, the question is asked again.

### On a terminal

With no flag, a terminal asks. One file gets this question, with only the answers that apply to it:

```
.bashrc [k] keep mine  [t] take theirs  [h] keep mine on this machine only  [e] edit  [d] diff  [s] stop:
```

- `[k]` and `[t]` settle the file as `--keep-local` and `--take-repo` do.
- `[h]` keeps this machine's version as its own variant, the one `dotman add --for host=<host> <file>` would make, and the shared file takes origin's version for the other machines. It's offered for a file edited on both machines whose name has no conditions yet, when neither side has that variant already. System files don't get it.
- `[e]` opens the merged file in `$VISUAL` or `$EDITOR`, with conflict markers around each clash. Remove the markers, save and quit, and dotman takes the file. A file you didn't change, one that still has markers, one with a new credential and a template that doesn't render are refused, and the question comes back; `[e]` then reopens your last edit. An encrypted file can be edited when both versions decrypt here. The edit is encrypted again before it's taken, and the content guard doesn't check it, as it doesn't check a capture of an encrypted file.
- `[d]` shows origin's version against this machine's, then asks again. An encrypted file is shown decrypted, and only when both versions decrypt here.
- `[s]` stops the whole pull, since git can't pull half of it, and the run exits 4 as it would without a terminal. An empty answer, or the end of input, counts as `[s]`.

Several files get one question first, `[k] all mine  [t] all theirs  [d] diffs  [p] per file  [s] stop`, and `[p]` asks about each file in turn. A key that both machines set differently in `.dotman.toml` is asked in the same list. When a later commit of yours stops on another file, that file is asked as it comes.

A pull is never left half merged while you decide. The repo, `$HOME` and the remote stay as they were until the last answer is in, so a run killed at a question or in the editor leaves the pull undone, and the next `sync` asks again. The editor's private copy stays until a later `sync`, `apply` or `add` removes it, once it is ten minutes old.

The editor opens a copy in a private 0700 directory under `$XDG_RUNTIME_DIR`, or under dotman's state directory when that isn't set, outside the repo and `$HOME`. dotman holds its lock while the editor is open, so another dotman command that writes fails until you close it.

### Settings

`.dotman.toml` is merged setting by setting, with no question. A list, such as `[ignore]`, `[dirs]` or `[age] recipients`, keeps what either machine added and drops what either removed. In a table, such as `[modes]`, `[hooks]` or `[system]`, each key takes the side that changed it. A key both machines set to different values is the only thing asked: `--keep-local` keeps this machine's value, `--take-repo` takes origin's, and a terminal asks with the files. A mode is the exception: a `[modes]` key, or the mode of a `[system]` table, that both machines set differently gets the stricter of the two with no question, so neither answer widens a file. The run then says how many settings came from origin, as in `merged .dotman.toml: 2 settings from origin/main`. A `[hooks]` table that the merge changes needs approval again, and the merge can't approve a hook or a `[dirs]` entry for this machine, since those approvals live in this machine's state.

A file's `[modes]` entry goes with the answer. When one machine's version is taken whole, the file gets that version's entry, or none if it had none. When the file is merged, by a flag or by `[e]`, it gets the stricter of the two modes, so a 0600 file stays 0600 whichever side wins. `[h]` gives the new variant this machine's entry.

All the variants of a system file share one `[system]` table in `.dotman.toml`, keyed by the file's path, so settling a conflict on one variant rewrites the table that every variant of that path uses.

### What is saved

The version that loses is always saved first. Before the replay, this machine's committed version of each file whose answer replaces it is copied to the run's backup directory as `.dotman/repo/<path>`, and the run prints where. It's saved even when `$HOME` holds the same bytes. An encrypted file is saved as ciphertext, and a symlink as its target. If the replay then fails, a file you edited with `[e]` is saved as `.dotman/edited/<path>` in the same directory.

### The replay

Once the last answer is in, dotman replays this machine's commits onto origin's with git. Resolved files go into git's index and git writes them to the repo. `$HOME` is written afterwards, by the usual apply. Your git config's `commit.gpgsign` and `merge.directoryRenames` affect the replay as they affect any pull.

Ctrl-C, a hangup and a terminate signal are ignored while the replay runs. That also means Ctrl-C can't interrupt a `commit.gpgsign` pinentry that hangs. A run killed during the replay leaves `.git/dotman-pull` in the repo. `dotman status` and dry runs then exit 4 and say so, and the next command that writes, such as `dotman sync`, puts the repo back as it was before that pull. `sync` then pulls again.

### Conflicts dotman doesn't settle

Some conflicts have a shape dotman doesn't settle: a file on one machine and a directory on the other, a file git matched with one renamed on the other side, an entry that is neither a file nor a symlink, such as a submodule, and a `.dotman.toml` that doesn't parse on one side. No flag or answer applies to them. `sync` exits 4 with nothing changed and prints the git steps that resolve the pull by hand, ending with `dotman sync`. When the pull puts one version of a path aside as a copy, such as `.x~HEAD`, step 2 names the copy, and step 3 keeps this machine's version under the path's name and removes the copy; the report says how to keep origin's version instead. The step that continues the rebase, `env GIT_EDITOR=true dotman git rebase --continue`, sets `GIT_EDITOR` itself: `dotman git` passes yours through, and git prefers it to `core.editor`. The steps are ordinary git, so you can also use them for a conflict that dotman would settle.

## Backups and modes

Before a write replaces a file in `$HOME` whose content differs, dotman copies it to `$XDG_STATE_HOME/dotman/backups/<UTC time>-<pid>/<key>` and records its mode in `.dotman/modes.txt` in the same directory. A change of mode alone is recorded in `modes.txt` without a copy, and a replaced symlink in `.dotman/links.txt`. The run names the directory once, on a line of its own after the file lines (`backed up the files it replaced to <directory>`), and each file it replaced ends its own line with `(backed up)`. A backup copy is never overwritten: a file backed up by both passes of one `sync` keeps both copies. When keeping `$HOME`'s version, with `--keep-local` or at the prompt, would replace an uncommitted edit in the repo, that edit is saved under `.dotman/repo/` first. A pull that conflicts saves there too: this machine's version of each file that loses goes to `.dotman/repo/<path>` before the replay. `dotman forget`, and forgetting a file deleted here (`--forget-deleted`, or at the prompt), also save under `.dotman/repo/` each repo file they remove whose bytes are not in the last commit, in `$HOME` or in the last applied state. When a copy cannot be saved, nothing is removed.

Backups of encrypted files from `$HOME` hold the plaintext, while the losing version a pull conflict saves from the repo is ciphertext. Backups are never pruned; `dotman doctor` shows their size and counts the plaintext copies, and you remove the runs you no longer need.

Modes count as content. `add` records a private file's mode, such as 0600, in `[modes]` in `.dotman.toml`, because git keeps only 0644 and 0755. A capture or an import leaves group and other write bits out of what it records, for files and directories alike: a 0664 file or 0775 directory, as umask 002 makes them, records nothing, so other machines get 0644 or 0755 while this one keeps its mode until the repo's version next replaces the file; a 0660 file records 0640, which this machine then applies too. A write bit that a `[modes]` entry already holds, as one you wrote by hand, stays while the file keeps it and goes from the entry once the file loses it: with `".shared" = "0664"`, a capture of a 0666 file records 0664 and one of a 0644 file removes the entry. When the repo's version replaces a file in `$HOME`, group and others gain no read or write bit the file lacked, and get the repo's execute bit only where they can read the file or could already execute it. A stricter mode kept this way stays on this machine: `status` notes "kept at 0600 here; the repo says 0644", the file reads as clean, and nothing is committed. The repo's mode applies as it is when a `[modes]` entry sets it, which is meant for every machine, or when the repo's mode changed since this machine last applied it, as when another machine loosened it on purpose.

## Ignoring files

`[ignore] patterns` in `.dotman.toml` keeps files under managed directories out of the repo. The defaults are `.git/`, `*.swp`, `*~`, `.DS_Store`, `__pycache__/`, `node_modules/`, `*.orig` and `*.rej`, at any depth. As with git's ignore rules, a pattern applies only to files the repo does not track yet: a tracked file keeps syncing. `add <dir>` skips ignored files, and `add <file>` adds an ignored file you name, with a warning. Removing a pattern takes effect on a machine that has seen it only after `sync --drop-unlisted-ignores` runs there.

## Prompt-answer flags

Every prompt has a flag, so unattended runs never guess. A run is non-interactive when stdin is not a TTY or `DOTMAN_NON_INTERACTIVE` is set; unanswered paths are skipped and listed, and the run exits 3. A pull conflict that nothing answers stops `sync` instead, with nothing changed and exit 4.

| Prompt | Flag |
|---|---|
| Conflicts, including a pull that conflicts, and existing files | `--keep-local` or `--take-repo` |
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
