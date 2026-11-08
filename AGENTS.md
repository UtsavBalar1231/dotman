# Project knowledge base

## Overview
dotman is a dotfiles manager with Git-like semantics, in Rust (edition 2024). The crate is `dotman` and the binary is `dot`. A large rewrite is planned, so when this file and the code disagree, trust the code.

## Structure
```
src/cli.rs         clap definitions; xtask imports it to build man pages
src/main.rs        dispatch only (match cli.command)
src/commands/      one module per subcommand
src/mirror.rs      git transport: dotman <-> bare mirror in ~/.dotman/mirrors/<remote> <-> remote
src/sync.rs        git importer (`dot import`), not remote sync
xtask/             man page generator (cargo run -p xtask -- generate-man-pages)
completions/       dot.* are clap-generated; *-enhanced* are hand-written
man/               generated output, tracked in git
packaging/         Arch PKGBUILD only
```

## Finding your way
| To change | Look in | Why it is not obvious |
|---|---|---|
| A CLI flag or subcommand | `src/cli.rs` | `main.rs` only dispatches |
| Which commands get man pages | `subcommands` array in `xtask/src/main.rs` | The list is hard-coded, so a new command gets no page until you add it |
| Pager selection | `resolve_pager_command` in `src/utils/pager/config.rs` | The precedence spans env vars and config |
| Committed file contents | `SnapshotManager::load_snapshot` | The index holds only staged and deleted entries |
| Path validation | `DotmanContext::validate_user_path` in `src/lib.rs` | It is an inherent method, not part of the `CommandContext` trait |

## House rules
- New code should pass `just lint-strict`, which is the clippy set the pre-commit hook runs. `just lint` is pedantic + nursery, and CI runs only `-D warnings`, so a change can pass CI and still be rejected at commit time.
- No `.unwrap()`, `.expect()` or `panic!` in `src/`, because lint-strict denies them. `clippy.toml` allows them in tests only.
- Every item needs a `///` doc comment, including private items and `pub mod` lines. `src/lib.rs` warns on `missing_docs` and `missing_docs_in_private_items`, and lint runs with `-D warnings`.
- Status output goes through `crate::output` (`success`, `info`, `warning`, `error`, `start_progress`), which writes to stderr in the git convention. stdout is for command output.
- Destructive commands take `--dry-run`. New ones should too, to match checkout, reset, revert, restore, merge, rm, push and clean.
- Code that reads working-tree files for staged or tracked paths must cope with a file that has since been deleted from disk.
- The CLI does not mirror git's flag shapes. Some examples: `dot branch rename [-o old] -n new`, `dot branch create <name> -f <start>`, `dot log [refs...] [--] [paths or globs...]`.
- Adding a command takes these steps:
  1. Add a `Commands` variant in `src/cli.rs`.
  2. Add a match arm in `src/main.rs`.
  3. Add a documented `pub mod` in `src/commands/mod.rs`.
  4. If it needs a man page, add it to the xtask array.
  5. Update the hand-written `completions/*-enhanced*` files. Clap regenerates only the basic ones.
- Commits follow Conventional Commits with a scope, for example `feat(log): ...` or `refactor(storage)!: ...`. Use `!` for breaking changes.
- Tests that change `HOME` must be `#[serial]`, because `set_var` is process-global (and `unsafe` in edition 2024). `integration_basic.rs` avoids this by running the `dot` binary through `assert_cmd` and passing `HOME`/`DOTMAN_REPO_PATH` with `.env()`.
- `DotmanContext::new_explicit` is the test constructor. It sets `no_pager` and `non_interactive`, so prompts never block; `new_with_explicit_paths` sets neither. Prompts are also skipped when `DOTMAN_NON_INTERACTIVE` is set or stdin is not a TTY, and in that case `add` refuses large files unless `--force` is passed.

## Invariants
- The index (v2) holds only the staging area: `staged_entries` and `deleted_entries`. Committed files come from the HEAD snapshot, and `Snapshot.files` maps paths to `SnapshotFile`, not `FileEntry`.
- Tracking intent (which directories and files to watch) lives in `TrackingManifest` (`tracking.bin`), separate from the index.
- `parents[0]` is the mainline. `HEAD~n` and `dag::build_first_parent_chain` follow first parents only; `dag::is_ancestor` walks all parents.
- Hashing is xxh3-128 (32 hex chars) and non-cryptographic. Objects are stored as `objects/{hash}.zst`, commits as `commits/{id}.zst`.
- Git and dotman commit IDs are mapped in `remote-mappings.toml` (`src/mapping.rs`). `pull` runs inside `Transaction` (`src/transaction.rs`), which rolls back on drop unless `commit()` is called.

## Do not
- Do not run commands against the real `~/.dotman` or `$HOME`, because it holds the user's actual dotfiles. Use a throwaway home: `HOME=/tmp/dotman-x DOTMAN_REPO_PATH=/tmp/dotman-x/.dotman ./target/debug/dot ...`.
- Do not change the `Index`, `FileEntry` or `TrackingManifest` structs casually. They are bincode-serialized and `Index::load` never checks the version field, so existing repos break silently.
- Do not weaken the security defaults. Paths must be validated against `allowed_directories`, which defaults to `$HOME` only. Setuid, setgid and sticky bits are stripped on add, and restore always passes `allow_dangerous: false` to `apply_to_path`. Regression tests belong in `tests/security_tests.rs`.
- Do not commit proptest regression files. They are gitignored on purpose.
- Do not hand-edit `man/*.1` or the clap-generated `completions/dot.*`. Regenerate them instead.
- `just release-prepare <ver>` bumps Cargo.toml, Cargo.lock, the PKGBUILD and the fallback version in `install/install.sh`. `src/bootstrap/install.sh.in` pins `CARGO_PKG_VERSION` itself.

## Commands
```bash
cargo test test_reset_hard --test command_tests   # single test
cargo test -- --include-ignored                   # also runs the 2 slow #[ignore] remote tests; CI does this
cargo test --test integration_basic               # `just test-integration` points at a missing file
cargo test --test property_tests                  # `just test-property` points at a missing file
just lint-strict                                  # pre-commit clippy set
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
just generate-manpages
```
Tests shell out to `git`, which must be installed with `user.name` and `user.email` set.

Environment: `DOTMAN_REPO_PATH` (default `~/.dotman`) and `DOTMAN_CONFIG_PATH` (default `~/.config/dotman/config`, TOML) override locations; `DOTMAN_NON_INTERACTIVE` skips prompts; `NO_PAGER` disables paging. Logging uses `RUST_LOG=dotman::<module path>` (default `warn`), but tracing exists only in `main.rs`, `diff/` and `utils/pager/`.
