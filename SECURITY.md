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

- Every home write goes through one path. It validates the key, refuses a symlinked parent, never writes through a symlink at the target, creates a temp file at 0600, strips setuid, setgid and sticky bits, and backs up what it replaces first.
- Git runs with the user's excludes, hooks and filters disabled, so a global config or a tracked `.gitattributes` cannot change what is read or written. A `.git` path component is refused at any depth.
- Hooks run only when the hash of the `[hooks]` table matches the one approved on this machine. Approval covers the command text, not the scripts it calls.
- Templates use minijinja with no loader and no command functions. Environment variables are visible only if listed in `[template] env`.
- Importers do not execute the source. The chezmoi importer copies only plain files to a scratch directory and runs chezmoi there with an empty config. `--render-templates` is the explicit opt-in to run chezmoi's templates.
- System files are opt-in per machine, read as the user, and written by a helper that rechecks everything (below).

## The system-write helper

`apply --system` builds a plan as the user, then escalates once through `sudo`, `doas` or `run0` to run `dotman system-write`. The helper runs with root's power, so it trusts nothing from the caller. It accepts a plan only if:

- the plan is a regular file, not a symlink, at mode 0600, owned by the invoking user (`SUDO_UID`, else the user from `DOAS_USER`); when root starts the helper directly, the plan directory must be root-owned;
- the roots embedded in the plan equal `[system] roots` in the invoking user's machine config, which must be a regular file, not a symlink, owned by that user and not writable by group or others;
- each target is absolute and canonical, is not on the never-allowed list (`/`, `/proc`, `/sys`, `/dev`, `/run`, `/tmp`, `/var/tmp`, and the SIP paths on macOS), and lies inside a root;
- the bytes match the embedded hash, owner and group resolve, and the mode has setuid, setgid and sticky stripped.

It then walks each target from `/` with `openat(O_NOFOLLOW | O_DIRECTORY)`, creates no directories, and writes through a temp file and `renameat` in that directory descriptor. When it runs as root, it ignores `DOTMAN_SYSTEM_ROOT` and refuses any plan that carries a prefix; the prefix exists only for tests under `escalate = "none"`.

**Do not add a `NOPASSWD` sudoers rule for `dotman system-write`.** The user controls `[system] roots` in a file they own, so such a rule lets that user write any root-allowed file, such as `/etc/sudoers.d/`, which is full root. Let the escalator ask for a password.

## age key handling

- `DOTMAN_AGE_KEY` reaches age only on its stdin (`-i -`). Every other child process, including git, `gh`, hooks, the editor, chezmoi and the escalator, starts without it. It is refused in `[template] env`.
- Identity files are created at 0600. `key import` refuses to overwrite a different identity without `--force`, and backs the old one up first.
- Any plain capture (`add`, `sync` of a new file, a local edit) checks the content for `AGE-SECRET-KEY-`. `add` refuses the file; `sync` holds it back and never commits it.
- Plaintext goes to age on stdin and ciphertext comes back on stdout, so decrypted content never sits in the repo. `diff` decrypts into a 0700 temp directory that is removed afterwards.
- A lost identity cannot be recovered. Keep it in a password manager plus one offline copy, and add a second recipient.
- dotman keeps ciphertext in git history. Removing an old recipient does not revoke access to what they could already read.

## Container bootstrap

The generated `install.sh` downloads a pinned release tarball and verifies it against `checksums.txt`. Both come from the same release URL, so the checksum proves integrity, not authenticity: it catches a corrupt or truncated download, not a compromised release. To verify who published the binary, build from a tagged checkout. `DOTMAN_BOOTSTRAP_BASE_URL` overrides the download base; set it only to a host you control.

## Not tested

- `restorecon` runs after a system write when SELinux is enforcing, but it has never run: there is no SELinux in CI.
- The real-root write path runs only in CI containers. Everything else about the helper is tested unprivileged with `escalate = "none"`.

## Dependencies

CI runs `cargo deny`. The one accepted advisory, RUSTSEC-2025-0141 (`bincode`), is in the legacy importer, which reads local files you point it at.
