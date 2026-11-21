# Command policy

Command modules own reviewed actions, engine orchestration, reports, and terminal outcomes. [The shared CLI guide](../AGENTS.md) covers common services.

## Where to look

| Concern | Owner |
|---|---|
| Command modules and outcome type | `mod.rs` |
| Home capture and guarded additions | `add.rs`, `add/` |
| Apply decisions and incoming pass | `apply.rs` |
| Capture/commit/pull/apply/push orchestration | `sync.rs`, `sync/selected.rs` |
| Pull conflict decisions and replay | `conflict.rs` |
| Reviewed snapshots and partial selections | `review.rs`, `partial.rs`, `../decisions.rs` |
| Read-only rows and diff projections | `status.rs`, `diff.rs` |
| Reviewed remedies and identity operations | `remedy.rs`, `remedy/`, `key/reviewed.rs` |
| Initialization and guarded setup | `init.rs`, `init/setup.rs`, `wizard.rs` |

## Adding or changing a command

- Add clap definitions in `../cli.rs`, export the module in `mod.rs`, dispatch it in `../main.rs`, and add its minimal invocation to `tests/guard.rs::MINIMAL_ARGS` unless it belongs to an explicit guard exception.
- Update command integration coverage and the help contract together. Regenerate man pages and completions with `cargo run -p xtask -- generate`.
- Preserve `EXIT_AND_ENVIRONMENT` layout: `Exit status:` and `Environment:` introduce sections, two-space indentation starts an entry, and deeper indentation continues it. Subcommand `after_long_help` uses the same grammar.
- `Outcome::Done`, `Attention`, and `Conflict` map to exit 0, 3, and 4. `main` also maps typed errors, including `InProgress` to 4 and stale `ReviewChanged` to 3.
- `status` exits 0 when inspection succeeds even if paths need attention. `doctor` exits 1 for unhealthy checks, and `git` forwards Git's exit status.

## Guard routing

- Ordinary readers use `Context::guard`. Ordinary writers enter through `Context::lock` or `Context::start` before mutation.
- The guard rejects incomplete repos, interrupted owned pulls, unfinished Git operations/unmerged index entries, and lost tracked settings. Do not bypass it merely to make a new command work during a conflict.
- `log` checks incomplete repos and owned-pull markers but skips operation/conflict and lost-settings checks, since it reads history.
- `doctor` and `git` remain available for diagnosis and recovery. `completion` and clap help run without opening the repo. `system-write` uses the privileged helper's validation.
- Fresh init is allowed before a real `.git` exists. Linux trust review uses `start_trust`/`guard_trust`. Guarded setup and reviewed key and remedy flows have dedicated validation and lock paths.

## Reviewed actions and JSON

- Parse `--decide-from -` through `decisions::Input` before acting. Keep schema, unknown-field, key/path, duplicate, observation, and approval validation centralized there. Reviewed input disables prompting.
- Bind input to the resolved context and managed membership. Recheck it under the lock and preserve HEAD checks around operations. Refuse stale previews.
- `review.rs` owns stored observations, projected deltas, and post-capture snapshots. Dry runs use `Stored::Ciphertext` for the unknown result of future encryption.
- `apply`, `sync`, and `add` use `Context::start` for reporting and locking. `status` and `diff` emit context even when their guard or repo-ID result fails, then propagate that result.
- Write NDJSON records through `output::json::write` with an explicit projection. Scope stdout borrows to reporting and use `child_stdout` to redirect child output from the stream.
- Register new JSON commands in `main::wants_json`. `main` owns the single terminal result after mapping the outcome, including errors before a context record.
- Keep byte-preserving paths and arguments distinct from escaped display text. Expose `RecoveryStep` in the desktop as a terminal recipe. JSON labels every nonzero terminal outcome `error`, including exit 3.
- Update the protocol tests and matching docs when records or decisions change. See `docs/decisions-json.md` for reviewed input and `docs/outcomes-json.md` for action events.

## Apply, sync, and conflicts

- `apply::run` handles ordinary apply and separate trust and hook reviews. `apply::apply` executes the shared apply pass.
- Preserve category-level decisions and undecided keys. A noninteractive run leaves unanswered paths unchanged and reports attention. Keep approval flags scoped to their decision category.
- Whole sync captures local changes, commits permitted content, pulls/replays, applies incoming changes, reencrypts, runs or defers hooks, then pushes. The no-remote branch still applies local results and evaluates hooks.
- The incoming apply skips first-pass undecided keys plus blocked keys. Do not let a second pass reverse a decision made during local capture.
- `sync::held_entries` scans changed repo files against content and name guards and machine allow rules before staging. This covers `edit` and direct repo edits that bypass capture.
- Mass-delete and unrelated-history refusals live in `sync.rs`.
- Held-file recovery must retain edits that exist only in the repo. Do not suggest `add` when copying home over the repo would destroy that sole version.
- Preserve how sync aggregates individual path failures and how those failures affect commit and push.
- `sync/selected.rs` owns selected-line sync and its separate transaction constraints. Do not route selected input through the whole-sync capture or approval path.
- `conflict::accept` validates every newly synthesized text blob: scan the entire merged text, discount only allowed findings or identical rule/text findings in committed sides, then validate template rendering.
- Whole-version choices reuse committed blobs. Encrypt edited `{age}` plaintext before writing a blob.
- Back up losing local committed blobs under `.dotman/repo/` before replay. Age backups remain ciphertext and symlink backups hold their targets. Failed replay preserves accepted edits under `.dotman/edited/`.
- `conflict.rs` decides and validates. `Git::replay` and `replay_reviewed` update the index, track the replay marker, and move the branch. Preserve recovery on repeated stops and interrupted replay.
