# Rust integration tests

These tests exercise the built CLI through isolated `common::TestEnv` fixtures.

## Where to look

| Surface | Files |
|---|---|
| Environment, Git, fixtures, snapshots, printed commands | `common/mod.rs` |
| Harness identity and checksum checks | `harness_smoke.rs` |
| Subcommand coverage and operation guards | `guard.rs` |
| Hostile inputs and confinement | `security_tests.rs` |
| System roots, plans, escalation | `system_scope.rs` |
| Documentation contracts and claim registry | `docs.rs` |
| Binary alias and printed command names | `cli_name.rs`, `names.rs` |
| Reviewed input and machine-readable outcomes | `cli_decisions.rs`, `cli_reviewed_key.rs`, `cli_outcomes_json.rs` |
| Selected publication and transport boundaries | `cli_selected.rs`, `cli_selected/edges.rs`, `cli_selected/transport.rs` |

## Harness conventions

- Start CLI fixtures with `TestEnv::new()`. `second_device()` creates a separate home and state while retaining the shared bare remote.
- `dot()` clears the child environment and sets HOME, XDG config/state/data, C locale, noninteractive mode, and the fixture PATH. Set test-specific child variables with `run_with`, `ok_with` or `dot().env(...)`.
- `dot_process()` and `process_for()` expose standard process commands with the same environment. `command_with_path()` restricts PATH for installation and executable-discovery tests.
- The harness writes `Test User <test@example.invalid>` into its own `.gitconfig`. Use `git`, `git_raw` and `git_in` rather than inheriting the caller's Git configuration.
- HTTP(S) Git URLs are rewritten to a missing local path, only the `file` protocol is allowed, and a failing `gh` stub blocks the user's account. Add fixture stubs through `prepend_path()`.
- `use_system_root()` redirects system paths into the fixture and installs package-manager stubs. Use `write_system()` and `system_path()` for ordinary system-scope tests.
- Use `tree_checksum()` for before/after assertions involving bytes, modes, entry kinds and symlink targets. Import `Source` fixtures seal and check the original source tree.
- Use `run_hint()` and `paste()` to execute printed recovery commands inside the fixture.

## Adding coverage

- `guard.rs` compares `MINIMAL_ARGS` and the explicit `NOT_COVERED` exclusions with the visible commands. The parser check rejects exit 2 and panic exit 101.
- Assert the affected behavior through exit status, output, home/repo/state changes, remote refs or objects, and backups as applicable.
- `docs.rs` checks documented commands and flags, the root guide's structure paths, and claim-to-test mappings. The claim registry matches sentence substrings and looks up named `#[test]` functions under `src/` or `tests/`.
- Run the `cli_selected` Cargo target to exercise its `edges.rs` and `transport.rs` modules.

## Optional tools and live tests

- `age_installed()` accepts age or rage on PATH. `DOTMAN_REQUIRE_AGE=1` turns an absent tool into a failure.
- `have_chezmoi()` runs `chezmoi --version`. `DOTMAN_REQUIRE_CHEZMOI=1` turns an absent tool into a failure. Keep stub-based importer coverage independent of that installation.
- `real_sudo_writes_a_root_owned_file` requires `DOTMAN_REAL_SUDO_TEST=1` and a container marker (`/.dockerenv` or `/run/.containerenv`). Run it only in the dedicated disposable container because it writes under `/etc/dotman-ci`.
- Live selected HTTPS/SSH tests are ignored by default. `sh tests/cli_selected/live.sh` runs the three exact tests and verifies that each passed. SSH uses Docker and `DOTMAN_SSH_FIXTURE_IMAGE` (default `act-runner-rust:local`).

## Focused checks

```bash
cargo test --test harness_smoke --test guard
cargo test --test docs
cargo test --test cli_selected
DOTMAN_REQUIRE_AGE=1 cargo test --test cli_secrets
DOTMAN_REQUIRE_CHEZMOI=1 cargo test --test cli_import_chezmoi
```
