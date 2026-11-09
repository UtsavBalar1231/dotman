# Security policy

## Supported versions

dotman 0.1.0 is the first release. Only the latest release receives fixes.

## Reporting a vulnerability

Use [GitHub private vulnerability reporting](https://github.com/UtsavBalar1231/dotman/security/advisories/new). If you cannot, email utsavbalar1231@gmail.com with the subject `[SECURITY] <short description>`.

Please include what you found, the steps to reproduce it, the versions affected and what an attacker gains. Expect an acknowledgment within 48 hours and an assessment within 7 days. For high and critical issues, the aim is a fix within 90 days, with disclosure coordinated with you. Reporters are credited in the advisory unless they ask not to be.

## Threat model

Applying a repo is as trusting as running its shell rc files, because a repo can ship any `.bashrc`. dotman guarantees where it writes and what it runs, not the content it writes.

A hostile repo, import source or system-write plan must never cause:

- a write outside `$HOME`, or outside the roots in `[system] roots`;
- a write through a symlink;
- a setuid, setgid or sticky bit;
- a hook run without approval on that machine;
- a template command execution;
- the silent loss of a file.

How this is enforced:

- Managed files reach `$HOME` through one write engine (`safe_write` and `safe_symlink`). It validates the key, refuses a symlinked parent, never writes through a symlink at the target, creates a temp file at 0600 in the target's directory, strips setuid, setgid and sticky bits, reads the target again just before the rename, and backs up a file it replaces whose content differs.
- Three other paths write under `$HOME`. `import --swap` replaces a symlink with a regular file or a directory holding the same bytes, and on a failure puts the links back; both check the link's target text before and after staging, rename onto the link itself, and record the links in the backups first. dotman's own files live at paths that come from the environment and the machine config, not from the repo: the machine config, `state.toml`, the age identity, the system-write plan and the files stored in the repo are written through a temp file, fsync and rename, and backups as new files opened without following a symlink. `init --git-global` runs `git config --global`, which writes git's own global config.
- Approved hooks run with `$HOME` as their working directory, and can write anything there.
- Every git child process runs with the isolation flags `-c core.excludesFile= -c core.hooksPath=/dev/null -c core.autocrlf=false -c core.safecrlf=false -c core.quotePath=false -c log.showSignature=false -c color.ui=false -c core.fsmonitor=false -c status.renames=true -c core.symlinks=true`, so a `core.symlinks=false` in your git config cannot check a managed symlink out as a file. `git init` and `git clone` also get `--template=`, so no template directory, from `init.templateDir` or `GIT_TEMPLATE_DIR`, puts a config, a sparse checkout or hooks into dotman's repo. Once the repo's `.git` exists, git gets it as `--git-dir` with `--work-tree`, so git does not search the parent directories for another repo. Each git child starts without the 18 variables that would point it at other refs, objects or history: `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`, `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_PREFIX`, `GIT_NAMESPACE`, `GIT_CEILING_DIRECTORIES` (set again to the repo's parent), `GIT_DISCOVERY_ACROSS_FILESYSTEM`, `GIT_SHALLOW_FILE`, `GIT_GRAFT_FILE`, `GIT_REPLACE_REF_BASE`, `GIT_QUARANTINE_PATH`, `GIT_LITERAL_PATHSPECS`, `GIT_GLOB_PATHSPECS`, `GIT_NOGLOB_PATHSPECS` and `GIT_ICASE_PATHSPECS`. `GIT_CONFIG_*` is kept.
- `init`, when it creates or adopts a repo, and every clone write `.git/info/attributes` with `* -text -filter -ident -working-tree-encoding merge`. Every command that writes, `apply` and `sync` among them but not the `git` passthrough, writes the line again before it changes anything when the file holds anything else, as in a repo an older dotman made; `dotman doctor` warns about such a file. The line wins over a tracked `.gitattributes`: `-text` turns off line-ending conversion, and with it `eol`; `-filter` turns off clean and smudge filters; `-ident` turns off `$Id$` expansion; `-working-tree-encoding` turns off re-encoding; and `merge`, set with no value, always uses git's built-in text merge, whatever `merge.<name>.driver` or `merge.default` your git config sets. So neither a tracked `merge=union` nor a merge driver you configured can join both sides of an edit made on two machines: `sync` stops at that conflict instead (exit 4). The line does not say `merge=text`, because git would look a driver named `text` up in your git config first.
- A `.git` path component is refused at any depth, in every spelling git refuses on NTFS: `.GIT`, `.git` followed by spaces or dots, `.git:stream`, and `git~1`.
- Hooks run only after this machine approved the `[hooks]` table. The approval is the table itself, kept as canonical JSON and compared byte for byte, so a change to any command or glob needs approval again. A warning that hooks were not run prints the whole table, escaped, and records it as the table shown. When a table was recorded that way and the current one differs, `--approve-hooks` shows the current table, records it and approves nothing (exit 3); the same command run again approves it. When none is recorded, `--approve-hooks` approves the current table, printing it first, so one run can approve a table that no warning on this machine showed. Approval covers the command text, not the scripts it calls.
- Templates use minijinja with no loader and no command functions. Environment variables are visible only if listed in `[template] env`.
- Importers do not execute the source. The chezmoi importer copies only plain files to a scratch directory and runs chezmoi there with a config that sets only the umask, so the user's chezmoi config and its hooks are never read. `--render-templates` is the explicit opt-in to run chezmoi's templates.
- A git source is cloned with `--single-branch --no-tags`, so an import takes only the history of the branch the source's `HEAD` names. Before the import is put in place, the content guard scans that history's files, binary ones aside, and its commits' messages and headers; a finding makes the import one commit of the current tree, unless `--keep-history` is passed.
- System files are opt-in per machine, read as the user, and written by a helper that checks everything again (below).

## The system-write helper

`apply --system` builds a plan as the user, then escalates once through `sudo`, `doas` or `run0` to run `dotman system-write`. The helper runs with root's power, so it trusts nothing from the caller. It accepts a plan only if:

- the plan is a regular file, not a symlink, at mode 0600, and it, its directory and that directory's parent are owned by the invoking user and not writable by group or others. The invoking user is `SUDO_UID`, else the user named by `DOAS_USER`, else the owner of the plan's directory, as when an escalator sets neither variable or root starts the helper itself in a directory root owns;
- the roots embedded in the plan equal `[system] roots` in the invoking user's machine config, which must be a regular file, not a symlink, owned by that user and not writable by group or others;
- each target is absolute and canonical, is listed once, is not a root itself, is not on the never-allowed list (`/`, `/proc`, `/sys`, `/dev`, `/run`, `/tmp`, `/var/tmp`, and the SIP paths on macOS), and lies inside a root;
- the bytes match the embedded hash, owner and group resolve, and the mode has setuid, setgid and sticky stripped.

It then walks each target from `/` with `openat(O_NOFOLLOW | O_DIRECTORY)`, creates no directories, refuses a symlink or anything but a regular file at the target, and writes through a temp file and `renameat` in that directory descriptor. When it runs as root, it refuses to run while `DOTMAN_SYSTEM_ROOT` is set and refuses any plan that carries a prefix; the prefix exists only for tests under `escalate = "none"`.

**Do not add a `NOPASSWD` sudoers rule for `dotman system-write`.** The user controls `[system] roots` in a file they own, so such a rule lets that user write any root-allowed file, such as `/etc/sudoers.d/`, which is full root. Let the escalator ask for a password.

## age key handling

- `DOTMAN_AGE_KEY` reaches age only on its stdin (`-i -`). Every other child process, including git, `gh`, hooks, the editor, chezmoi and the escalator, starts without it. It is refused in `[template] env`.
- Identity files are created at 0600. `key import` refuses to overwrite a different identity without `--force`, and backs the old one up first.
- Plaintext goes to age on stdin and ciphertext comes back on stdout, so decrypted content never sits in the repo. `diff` decrypts into a 0700 temp directory that is removed afterwards.
- A lost identity cannot be recovered. Keep it in a password manager plus one offline copy, and add a second recipient.
- dotman keeps ciphertext in git history. Removing an old recipient does not revoke access to what they could already read.
- Recipients added upstream are trusted. When `[age] recipients` in `.dotman.toml` changes, the next `sync` on a machine whose key decrypts every encrypted file re-encrypts them all to the new set without asking. Whoever can push to the repo can add a recipient and read every encrypted file after that sync. Check what changed with `dotman git log -p -- .dotman.toml`.

## Backups hold plaintext

Before a write replaces a file in `$HOME`, the file is copied to `$XDG_STATE_HOME/dotman/backups/`, as a 0600 file inside 0700 directories. For an encrypted file, that copy is the decrypted file from `$HOME`. `restore` backs up the same way, and `key import --force` backs up the identity it replaces. Backups are never pruned; `dotman doctor` counts the copies that hold plaintext. Remove the runs you no longer need.

## What the guards hold

The content guard scans what a plain capture would store: `add`, new files under a managed directory, `sync` of a local edit, `import`, and every file `sync` is about to commit, which covers `dotman edit` and edits made by hand in the repo. It holds whatever the remote, public or not.

- The rules: an age identity (`AGE-SECRET-KEY-1` and 58 bech32 characters), GitHub tokens (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`) and fine-grained tokens (`github_pat_`), GitLab tokens, Slack tokens and webhooks, AWS access key IDs, Stripe live keys, and OpenAI, Anthropic, npm, PyPI, DigitalOcean, SendGrid, Hugging Face, OpenRouter, Groq, xAI, Perplexity and Docker tokens, and PEM private key blocks. Each rule starts with a fixed prefix.
- A finding names the rule and the line, never the value.
- An age identity is never stored in plain text: no flag and no `[guard] allow` entry lets one through. In a system file, `add --system` refuses any finding and `sync` holds it, since system scope does not encrypt and no allow entry applies there.
- The name guard holds new files under a managed directory whose names match `[guard] secret_names`. With a remote known to be public, one that answers an anonymous `git ls-remote`, `add` also refuses a secret-named file you name, unless you pass `--encrypt` or `--allow-plain`.

What they miss:

- credentials without a known prefix, such as passwords, generic API keys and tokens of providers not on the list;
- files that look binary, with a NUL byte in the first 8000 bytes, which are not scanned;
- a file whose name does not match `secret_names` and whose content no rule finds.

## Outside the threat model

- **The escalated executable.** `apply --system` escalates by running the dotman executable it was started as. If that file or its directory is writable by your user, as `~/.cargo/bin/dotman` and `~/.local/bin/dotman` are, anything running as your user can replace it and gain root the next time you approve an escalation. On a machine that uses system files, install dotman root-owned, from a package or into `/usr/local/bin`.
- **Old git.** dotman accepts git 2.25 or newer, and `init <path>` and `import <path>` clone local repositories. Git releases before the May 2024 security releases (2.45.1, and 2.39.4, 2.40.2, 2.41.1, 2.42.2, 2.43.4 and 2.44.1 for older lines) have known flaws when they clone a crafted local repository, such as CVE-2024-32004 and CVE-2024-32021. dotman clones a local path with `--no-local`, through git's transport as it would a URL, so git does not take the local-clone shortcuts, copying or hardlinking the source's object store, that some of those fixes addressed. Other flaws do not depend on them, so still use a current git to import a repository you did not make.
- **`[guard] allow` is per machine.** An entry lets one rule through for one path on this machine only, and dotman never reads one from the repo. The file it lets through still goes into the repo, and to every machine and remote the repo reaches. `add --allow-plain` adds the entry by editing `config.toml` in place, which keeps its comments, its layout and the keys dotman does not know. If the file cannot be parsed for that edit, or the edited text would not read back as exactly the config being saved, dotman writes the whole file from its own settings instead, without those comments and keys and without a backup.

## Container bootstrap and releases

The generated `install.sh` downloads a pinned release tarball and verifies it against `checksums.txt`. Both come from the same release URL, so the checksum proves integrity, not authenticity: it catches a corrupt or truncated download, not a compromised release. `DOTMAN_BOOTSTRAP_BASE_URL` overrides the download base; set it only to a host you control.

The install paths all trust the GitHub repository. `install/install.sh` is fetched from its `main` branch, and the tarball and `checksums.txt` from its releases; nothing is signed. To verify who published the binary, build from a tagged checkout.

## Not tested

- `restorecon` runs after a system write when SELinux is enforcing, but it has never run: there is no SELinux in CI.
- The real-root write path runs only in CI containers. Everything else about the helper is tested unprivileged with `escalate = "none"`.
- macOS has not been tested: there is no Mac to test on, and CI has not run on one.

## Dependencies

CI runs `cargo deny`. The one accepted advisory, RUSTSEC-2025-0141 (`bincode`), is in the legacy importer, which reads local files you point it at.
