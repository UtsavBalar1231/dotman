# Project knowledge base

## Overview
dotman is a dotfiles manager in Rust (edition 2024, MSRV 1.88). The repo is an ordinary git repo whose root maps to `$HOME`; dotman copies files in and out and compares three sides (repo R, home H, recorded state S). The package is `dotman-cli`, the library crate is `dotman`, and the binary is `dotman` (`dot` is an optional alias). When this file and the code disagree, trust the code.

## Structure
```
src/cli.rs              clap definitions and long help; xtask imports it
src/main.rs             dispatch, root refusal, exit codes
src/commands/           one module per subcommand
src/engine/             status model (status.rs) and the safe write path (write.rs)
src/import/             importers: legacy, gittree (yadm, bare, vcsh), chezmoi, farm
src/context.rs          per-run paths, configs and git handle; the in-progress guard and temp sweep
src/paths.rs            dotman's own file locations, key validation, write_atomic
src/config.rs           the machine config, with the per-machine [guard] allow
src/repo_config.rs      .dotman.toml and its defaults
src/state.rs            state.toml, including the hook approval and held-back hooks
src/identity.rs         the machine identity that variants match
src/remote.rs           whether the remote is public
src/lock.rs             the per-user process lock
src/prompt.rs           questions, and when a run is interactive
src/system.rs           system scope: roots, verdicts, never-allowed paths
src/system/             helper.rs (root `system-write`) and escalate.rs (sudo, doas, run0)
src/variant.rs          brace-block variant grammar and ranking
src/template.rs         minijinja rendering
src/secrets.rs          age and rage handling, and the content guard rules
src/hooks.rs            hook approval and the one spawn site
src/git.rs              the git layer, its isolation flags and stripped variables
src/proc.rs             every child process starts here
src/name.rs             the invoked name (`dot` or `dotman`) for hints
src/sys.rs              the only module with `unsafe`
src/output/             stderr status output and its escaping
src/bootstrap.rs        writes and refreshes the install.sh at the repo root
src/bootstrap/          the install.sh template written by `init`
tests/                  integration tests; tests/common is the harness
xtask/                  man page and completion generator
man/                    generated man pages
completions/            generated shell completions
packaging/arch/         PKGBUILD
install/                install.sh for users
scripts/                release helper scripts
```

## Finding your way
| To change | Look in | Why it is not obvious |
|---|---|---|
| A flag or subcommand | `src/cli.rs` | `main.rs` only dispatches |
| What a row or category means | `src/engine/status.rs` | Eleven rows are evaluated in order; categories drive prompts |
| How a file reaches `$HOME` | `src/engine/write.rs` | `safe_write` is the only path for managed files; SECURITY.md lists the other writes under `$HOME` |
| When a write backs up | `src/commands/apply.rs` (`write`) | Whenever H's content differs from R, whatever S says; a mode-only change goes to `modes.txt` |
| A mode written over a file | `src/engine/mod.rs` (`never_widen`), `apply.rs` (`kept_mode`) | `[modes]` and a repo mode changed since S bypass the narrowing |
| Variant selection | `src/variant.rs` | Pure; the status model calls it |
| Template variables | `src/template.rs` | `env` holds only `[template] env` names |
| Encryption, keys, recipients | `src/secrets.rs`, `src/commands/key.rs` | The key goes to age's stdin only |
| The content guard | `src/secrets.rs` (`RULES`, `held`) | One combined regex; the age rule also runs alone, and binary files are skipped |
| What sync holds or refuses | `src/commands/sync.rs` | `held_entries` is the staging guard; the mass-delete and unrelated-history refusals live here too |
| Key rules | `src/paths.rs` (`validate_key`) | Control characters, `.git` in NTFS spellings and reserved root names |
| Checks every command runs first | `src/context.rs` (`guard`, `lock`) | All but `doctor`, `git` and `completion`; `log` runs only the first. A command that writes calls `lock`, which takes the lock, runs `guard` and refreshes `.git/info/attributes` |
| An importer | `src/import/` | `import::run` routes by detection order |
| System files, the helper | `src/system.rs`, `src/system/helper.rs`, `src/system/escalate.rs` | The helper runs as root and trusts nothing from the caller |
| Hook approval | `src/hooks.rs` | The state stores the approved `[hooks]` table as canonical JSON |
| Exit codes and environment in the help | `src/cli.rs` (`EXIT_AND_ENVIRONMENT`) | `xtask` turns its blocks into the man page's EXIT STATUS and ENVIRONMENT sections, so keep the indentation |
| A never, always or every claim in the docs | `tests/docs.rs` (`CLAIMS`, `GAPS`) | Each one names the tests that prove it, or is a gap |
| Invoked name in hints | `src/name.rs` | Never hard-code `dot` or `dotman` in a hint |
| Running a child process | `src/proc.rs` | It strips `DOTMAN_AGE_KEY` |
| Man pages and completions | `cargo run -p xtask -- generate`, or `just generate` | Output is tracked; CI checks freshness |
| Security regressions | `tests/security_tests.rs` | Hostile-repo and hostile-plan cases |
| Subcommand coverage | `tests/guard.rs` | Every subcommand needs a row |

## House rules
- New code passes `just lint-strict`, the clippy set that the pre-commit hook and CI both run.
- No `.unwrap()`, `.expect()` or `panic!` in `src/`. `clippy.toml` allows `unwrap` and `expect` in tests only.
- Comments and docs only where they carry what the code cannot: a constraint, invariant, unit, safety reason, or why. No doc that restates an item's name, no narrating comments. `missing_docs` covers public items only, so a public item carries a one-line `///` and a private one needs none.
- Status output goes through `crate::output` to stderr. stdout is for command output.
- Destructive commands take `--dry-run`, and a dry run changes nothing.
- Every child process starts from `proc::command`.
- Hints print `name::bin()`, never a hard-coded `dotman` or `dot`; `tests/names.rs` guards it.
- Every prompt has a flag that answers it. There is no hidden test switch.
- Code that reads working-tree files for tracked paths must cope with one deleted from disk.
- Enforce each guarantee at one choke point that every path goes through, not at each caller.
- Every command or recovery recipe that dotman prints is run, as printed, by a test.
- Every never, always or every claim in README.md and SECURITY.md has an entry in `tests/docs.rs`: `CLAIMS` names the tests that prove it, and `GAPS` holds the ones no test proves yet.
- Commits follow Conventional Commits with a scope, such as `feat(sync): ...`. Use `!` for breaking changes.
- Commit through the pre-commit hook, with your own git identity, never with `git -c user.name=... -c user.email=...`.
- Adding a command:
  1. Add a `Commands` variant in `src/cli.rs`.
  2. Add a match arm in `src/main.rs`.
  3. Add a module in `src/commands/mod.rs`.
  4. Add a row to `tests/guard.rs`, which checks the in-progress-operation guard.
  5. Run `cargo run -p xtask -- generate`.

## Invariants
- **Isolation flags.** Every git call carries `-c core.excludesFile= -c core.hooksPath=/dev/null`, `-c core.symlinks=true` and the other flags in `ISOLATION` in `src/git.rs`, starts without the `GIT_*` variables in `LOCAL_REPO_ENV`, and parsed output uses `-z`. `git init` and `git clone` also get `--template=`, and clones `--no-local`, so a local source goes through git's transport. Staging pathspecs go to `git add --pathspec-from-file=-` on stdin, never as arguments. A user's global excludes, hooks, `core.symlinks` or template directory never affect the repo.
- **Attributes file.** `init` and every clone write `.git/info/attributes` (`* -text -filter -ident -working-tree-encoding merge`), and `Context::lock`, which every writing command takes, writes it again outside a dry run when it differs. A tracked `.gitattributes` cannot transform content. The bare `merge` always uses git's built-in text merge, whatever `merge.*.driver` or `merge.default` the user configures; never write `merge=text`, which git looks up in the user's config as a driver name.
- **Import scope.** A git-tree import clones with `--single-branch --no-tags`: HEAD's branch only, no tags or other branches. The content guard scans that history's blobs and commit messages, and a finding squashes the import unless `--keep-history`.
- **State and repo ID.** `$XDG_STATE_HOME/dotman/state.toml` is never synced and is written through temp, fsync and rename. `.git/dotman-id` identifies the repo; a state whose `repo_id` differs is treated as empty. A state file that fails to parse is never treated as empty.
- **Repo root names.** `.git`, `.dotman.toml`, `.dotman/`, `README*`, `LICENSE*` and `install.sh` are never applied. `.git` is refused as a key component at any depth.
- **Home writes.** Managed files reach `$HOME` only through `safe_write` and `safe_symlink`: no symlinked parent, no write through a symlink, temp file at 0600, setuid, setgid and sticky stripped, a backup first when the content differs. The `--swap` replacement and rollback, `write_atomic` for dotman's own files and `git config --global` are the other writes, listed in SECURITY.md; a new one goes there too.
- **System scope.** `system-write` is the only path that runs as root. It rereads `[system] roots` from the invoking user's config, refuses `DOTMAN_SYSTEM_ROOT` and a plan prefix when euid is 0, and walks every target with `openat(O_NOFOLLOW)`.
- **`unsafe`.** Only in `src/sys.rs`, each block with a `// SAFETY:` comment. The crate root denies it.
- **Arithmetic.** `arithmetic_side_effects`, `float_arithmetic` and `indexing_slicing` are errors under lint-strict, in `tests/` too. Use checked or saturating integer arithmetic and `.get()`. A local `#[allow]` goes on one item with a `// reason:` comment that proves the bound.
- **Plaintext.** Decrypted content is never written into the repo. `DOTMAN_AGE_KEY` is removed from every child except age, and refused in `[template] env`.
- **Threat model.** A hostile repo, import source or plan must never cause a write outside `$HOME` or the roots, a write through a symlink, a setuid bit, an unapproved hook, a template command, or a silent loss of a file.

## Commands
```bash
cargo test --workspace --all-targets          # the full suite; run before every commit
cargo test --test cli_sync                    # one test file
just lint-strict                              # pre-commit clippy set
cargo fmt --check
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo run -p xtask -- generate                # man pages and completions
just release-prepare 0.2.0                    # bumps Cargo.toml, Cargo.lock, the PKGBUILD and install/install.sh
```
- `DOTMAN_REQUIRE_AGE=1` and `DOTMAN_REQUIRE_CHEZMOI=1` turn a skipped age or chezmoi test into a failure. CI sets them.
- The real-sudo test in `tests/system_scope.rs` runs only with `DOTMAN_REAL_SUDO_TEST=1` and a container marker (`/.dockerenv` or `/run/.containerenv`). Only CI's `system-scope` job sets both; elsewhere it returns early.
- Tests shell out to `git`, which needs `user.name` and `user.email` set. The harness in `tests/common` gives each test its own HOME and XDG dirs.

## Do not
- Do not run `dotman` or tests against the real `~/.dotman`, `~/.config/dotman`, `~/.local/share/dotman`, `~/.local/state/dotman`, `/etc` or `$HOME`. Use `tests/common`, or a temp root with a HOME and every `XDG_*` pointing inside it.
- Do not weaken the defaults: paths stay confined to `$HOME` and the opted-in roots, and setuid, setgid and sticky bits stay stripped. Regression tests go in `tests/security_tests.rs`.
- Do not hand-edit `man/*.1` or `completions/` (except `completions/README.md`). Regenerate them.
- Do not commit proptest regression files. They are gitignored.
- Do not add a dependency that is not already in `Cargo.toml` without raising it first.
