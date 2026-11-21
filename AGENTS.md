# Project knowledge base

**Source snapshot** `796225f` on `main`, mapped 2026-11-21.

## Overview

Dotman is a dotfiles manager that maps an ordinary Git repo onto `$HOME`. It compares the repo (R), home (H), and recorded state (S) before applying or capturing files. The root workspace contains `dotman-cli` (library and binary `dotman`, optional alias `dot`) and `xtask`. The CLI uses Rust edition 2024 with MSRV 1.88. The Linux desktop app has a separate Tauri workspace in `gui/` with MSRV 1.90. When guidance and code disagree, trust the code.

## Structure

```
src/cli.rs              clap definitions and long help; xtask imports it
src/main.rs             dispatch, root refusal, exit codes
src/commands/           command policy, reviewed actions, sync, and conflict resolution
src/engine/             three-way status, source selection, observation, and confined writes
src/import/             legacy, chezmoi, farm, and Git-tree conversion
src/system/             escalation and the privileged system-write helper
src/output/             terminal output, escaping, and NDJSON transport
gui/                    separate Linux workspace with native IPC and bundled WebView assets
docs/                   user guides and experimental CLI/desktop protocol contracts
tests/                  Rust integration tests with an isolated CLI harness
e2e/                    Bash journeys, failure injection, and importer scenarios
xtask/                  clap-based man page and completion generator
man/                    generated man pages
completions/            generated shell completions
video/                  demo films recorded from the real CLI in containers
install/                release installer, separate from the bootstrap written by init
packaging/arch/         CLI and desktop package recipes
scripts/                release and local desktop packaging helpers
```

## Finding your way

Read the closest scoped guide before editing in that area.

| Area | Guide | Coverage |
|---|---|---|
| Shared CLI services | [src/AGENTS.md](src/AGENTS.md) | Git isolation, context and state, settings, guards, and system scope |
| CLI behavior and JSON | [src/commands/AGENTS.md](src/commands/AGENTS.md) | Adding a command, reviewed inputs, outcomes, apply, and sync |
| Status and managed writes | [src/engine/AGENTS.md](src/engine/AGENTS.md) | Classification, source selection, and write mechanics |
| Import | [src/import/AGENTS.md](src/import/AGENTS.md) | Format detection, input accounting, and import-specific safety |
| Rust integration tests | [tests/AGENTS.md](tests/AGENTS.md) | Harness, optional tools, security and docs contracts |
| Shell scenarios | [e2e/AGENTS.md](e2e/AGENTS.md) | Fixtures, machine isolation, and interrupted operations |
| Desktop | [gui/AGENTS.md](gui/AGENTS.md) | Build, assets, IPC, and the paired CLI |

## Code map

References are Rust language server usages at the snapshot above, excluding declarations and including tests.

| Symbol | Location | Refs | Role |
|---|---|---:|---|
| `Context` | `src/context.rs` | 499 | Resolved paths, Git handle, local policy, operation checks |
| `Git` | `src/git.rs` | 86 | CLI Git subprocess and repository operations |
| `proc::command` | `src/proc.rs` | 36 | CLI child process creation and environment scrubbing |
| `classify` | `src/engine/status.rs` | 27 | Converts repo, home, and state observations to a status row |
| `safe_write` | `src/engine/write.rs` | 15 | Confined write for regular files |
| `decisions::Input` | `src/decisions.rs` | 11 | Reviewed CLI decision input |

## House rules

- New code passes `just lint-strict`, the clippy set that the pre-commit hook and CI both run.
- No `.unwrap()`, `.expect()` or `panic!` in `src/`. `clippy.toml` allows `unwrap` and `expect` in tests only.
- Comments and docs only where they carry what the code cannot: a constraint, invariant, unit, safety reason, or why. No doc that restates an item's name, no narrating comments. `missing_docs` covers public items only, so a public item carries a one-line `///` and a private one needs none.
- Status output goes through `crate::output` to stderr. stdout is for command output.
- Destructive commands take `--dry-run`, and a dry run changes nothing.
- Start every CLI child process through `proc::command`.
- Hints print `name::bin()`, never a hard-coded `dotman` or `dot`; `tests/names.rs` guards it.
- Every prompt has a flag that answers it. There is no hidden test switch.
- A fake credential in a test is split with `concat!`, a const or a format argument, so no secret scanner sees it whole. `just secrets` runs gitleaks over the commits a push would publish.
- Code that reads working-tree files for tracked paths must cope with one deleted from disk.
- Enforce each guarantee at one choke point that every path goes through, not at each caller.
- Every command or recovery recipe that dotman prints is run, as printed, by a test.
- A behaviour change that an `e2e/journeys` scenario covers updates that scenario in the same commit.
- Every never, always or every claim in README.md, `docs/*.md` and SECURITY.md has an entry in `tests/docs.rs`: `CLAIMS` names the tests that prove it, and `GAPS` holds the ones no test proves yet.
- Commits follow Conventional Commits with a scope, such as `feat(sync): ...`. Use `!` for breaking changes.
- Commit through the pre-commit hook, with your own git identity, never with `git -c user.name=... -c user.email=...`.
- The CLI permits `unsafe` only in `src/sys.rs`, with a `// SAFETY:` comment per block. The desktop crate has its own `gui/src/sys.rs`.
- Strict lint denies `arithmetic_side_effects`, `float_arithmetic`, and `indexing_slicing`, including in tests. Use checked or saturating integer operations and `.get()`. A local `#[allow]` goes on one item with a `// reason:` comment that proves the bound.

## Safety boundaries

- A hostile repo, import source, or plan must never cause a write outside `$HOME` or the opted-in roots, a write through a symlink, a setuid, setgid, or sticky bit, an unapproved hook, a template command, or silent file loss.
- Keep managed writes in the engine and system writes in the privileged helper. [SECURITY.md](SECURITY.md) documents the complete write surface, including importer swaps, dotman's own atomic writes, and explicitly requested global Git config. Update it when that surface changes.
- Never write decrypted contents to the repo. `DOTMAN_AGE_KEY` is stripped from CLI children except age and rejected as a template environment variable.
- Use `tests/common` or a temp root with HOME and every XDG directory inside it. Do not point development commands at the real `~/.dotman`, `~/.config/dotman`, `~/.local/share/dotman`, `~/.local/state/dotman`, `$HOME`, or `/etc`.
- Put regressions involving hostile repos or plans in `tests/security_tests.rs`.

## Commands

```bash
cargo test --workspace --all-targets          # the full suite; run before every commit
just ci                                       # local CLI checks, including e2e and gitleaks; run before a push
cargo test --test cli_sync                    # one test file
just lint-strict                              # pre-commit clippy set
cargo fmt --check
just docs-check                               # rustdoc with warnings denied
cargo run -p xtask -- generate                # man pages and completions
just e2e                                      # scenarios against a release build
just gui-check                                # separate desktop workspace and frontend checks
just video setup                              # demo output in target/video; see video/README.md
just release-prepare 0.2.0                    # updates release versions and runs just ci
```

The specialized CLI jobs are defined in `.github/workflows/ci.yml`. The root Cargo workspace and `just ci` do not cover the desktop workspace.

## Do not

- Do not hand-edit `man/*.1` or `completions/` (except `completions/README.md`). Regenerate them.
- Do not commit proptest regression files. They are gitignored.
- Do not add a dependency that is not already in `Cargo.toml` without raising it first.
