# Project knowledge base

## Overview
dotman is a dotfiles manager in Rust (edition 2024, MSRV 1.88). The repo is an ordinary git repo whose root maps to `$HOME`; dotman copies files in and out and compares three sides (repo R, home H, recorded state S). The package is `dotman-cli`, the library crate is `dotman`, and the binary is `dotman` (`dot` is an optional alias). When this file and the code disagree, trust the code.

## Structure
```
src/cli.rs              clap definitions; xtask imports it
src/main.rs             dispatch, root refusal, exit codes
src/commands/           one module per subcommand
src/engine/             status model (status.rs) and the safe write path (write.rs)
src/import/             importers: legacy, gittree (yadm, bare, vcsh), chezmoi, farm
src/system.rs           system scope: roots, verdicts, never-allowed paths
src/system/             helper.rs (root `system-write`) and escalate.rs (sudo, doas, run0)
src/variant.rs          brace-block variant grammar and ranking
src/template.rs         minijinja rendering
src/secrets.rs          age and rage handling
src/hooks.rs            hook approval and the one spawn site
src/git.rs              the git layer and its isolation flags
src/proc.rs             every child process starts here
src/name.rs             the invoked name (`dot` or `dotman`) for hints
src/sys.rs              the only module with `unsafe`
src/output/             stderr status output
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
| How a file reaches `$HOME` | `src/engine/write.rs` | `safe_write` is the only home write path |
| Variant selection | `src/variant.rs` | Pure; the status model calls it |
| Template variables | `src/template.rs` | `env` holds only `[template] env` names |
| Encryption, keys, recipients | `src/secrets.rs`, `src/commands/key.rs` | The key goes to age's stdin only |
| An importer | `src/import/` | `import::run` routes by detection order |
| System files, the helper | `src/system.rs`, `src/system/helper.rs`, `src/system/escalate.rs` | The helper runs as root and trusts nothing from the caller |
| Hook approval | `src/hooks.rs` | The state stores a hash of the `[hooks]` table |
| Invoked name in hints | `src/name.rs` | Never hard-code `dot` or `dotman` in a hint |
| Running a child process | `src/proc.rs` | It strips `DOTMAN_AGE_KEY` |
| Man pages and completions | `cargo run -p xtask -- generate`, or `just generate` | Output is tracked; CI checks freshness |
| Security regressions | `tests/security_tests.rs` | Hostile-repo and hostile-plan cases |
| Subcommand coverage | `tests/guard.rs` | Every subcommand needs a row |

## House rules
- New code passes `just lint-strict`, the clippy set the pre-commit hook runs. CI runs only `-D warnings`, so a change can pass CI and fail at commit.
- No `.unwrap()`, `.expect()` or `panic!` in `src/`. `clippy.toml` allows `unwrap` and `expect` in tests only.
- Comments and docs only where they carry what the code cannot: a constraint, invariant, unit, safety reason, or why. No doc that restates an item's name, no narrating comments. `missing_docs` covers public items only, so a public item carries a one-line `///` and a private one needs none.
- Status output goes through `crate::output` to stderr. stdout is for command output.
- Destructive commands take `--dry-run`, and a dry run changes nothing.
- Every child process starts from `proc::command`.
- Hints print `name::bin()`, never a hard-coded `dotman` or `dot`; `tests/names.rs` guards it.
- Every prompt has a flag that answers it. There is no hidden test switch.
- Code that reads working-tree files for tracked paths must cope with one deleted from disk.
- Commits follow Conventional Commits with a scope, such as `feat(sync): ...`. Use `!` for breaking changes.
- Adding a command:
  1. Add a `Commands` variant in `src/cli.rs`.
  2. Add a match arm in `src/main.rs`.
  3. Add a module in `src/commands/mod.rs`.
  4. Add a row to `tests/guard.rs`, which checks the in-progress-operation guard.
  5. Run `cargo run -p xtask -- generate`.

## Invariants
- **Isolation flags.** Every git call carries `-c core.excludesFile= -c core.hooksPath=/dev/null` and the other flags in `src/git.rs`, and parsed output uses `-z`. A user's global excludes or hooks never affect the repo.
- **Attributes file.** `init` and every clone write `.git/info/attributes` (`* -text -filter -ident -working-tree-encoding`), so a tracked `.gitattributes` cannot transform content.
- **State and repo ID.** `$XDG_STATE_HOME/dotman/state.toml` is never synced and is written through temp, fsync and rename. `.git/dotman-id` identifies the repo; a state whose `repo_id` differs is treated as empty. A state file that fails to parse is never treated as empty.
- **Repo root names.** `.git`, `.dotman.toml`, `.dotman/`, `README*`, `LICENSE*` and `install.sh` are never applied. `.git` is refused as a key component at any depth.
- **Home writes.** Only `safe_write` writes into `$HOME`: no symlinked parent, no write through a symlink, temp file at 0600, setuid, setgid and sticky stripped, backup first.
- **System scope.** `system-write` is the only path that runs as root. It rereads `[system] roots` from the invoking user's config, never honors `DOTMAN_SYSTEM_ROOT` or a plan prefix when euid is 0, and walks every target with `openat(O_NOFOLLOW)`.
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
