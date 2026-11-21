# Status and managed writes

The engine observes repo, home, and state tuples, selects a source, and performs confined replacements.

## Where to look

| Task | Location | Boundary |
|---|---|---|
| Tuple, observation, portable links, modes | `mod.rs` | Kind, hash, and mode all participate in equality |
| Three-way rows and prompt categories | `classify`, `Row`, `Category` in `status.rs` | Pure classification |
| Repo reads and selected variants | `status.rs`: `Worktree`, `Variants`, `SourceEntry` | Physical source and effective content are distinct |
| Replacement, backup, farm swap helpers | `write.rs` | Expected-state checks and filesystem operations |
| Apply orchestration and mode policy | `../commands/apply.rs`: `Run::write`, `kept_mode` | Decides backups, writes, and recorded state |

## Classification contract

- R is the effective repo tuple, H is `Home`, and S is the recorded tuple. `classify` returns no row when both R and S are absent.
- Preserve the order of checks in `classify`. It handles missing R, then H equal to R, then blocked or missing H, then the symlink override, then the remaining S comparisons.
- Rows 1–3: R absent means gone everywhere, forgotten upstream with unchanged H, or forgotten upstream with local edits.
- Row 4 applies when H equals R. `update_state` repairs a differing S.
- Rows 5–7: H absent means not applied, deleted locally with unchanged R, or deleted locally with changed R.
- Rows 8–11 classify an absent S as existing, S equal to R as changed locally, S equal to H as changed in repo, and the remaining case as both changed.
- A symlink H under a regular-file R is `SymlinkAtTarget` unless H still equals S. A link still equal to S can be replaced as `ChangedInRepo`.
- Categories group local deletions, existing files, and conflicts separately. Forgotten edits, template drift, and blocked paths are report-only attention cases.
- `Row::drifted` converts local/both changes to template drift. `Row::switched` prevents capturing an old source's local edits into a new source and refreshes S even for equal tuples.

## Observation and source selection

- `read_node` validates keys and checks parent components without following symlinks. Directories, special nodes, and non-directory parents remain obstacles.
- Regular-file reads use `O_NOFOLLOW | O_NONBLOCK` and verify the opened node so a FIFO substituted during observation cannot block the read.
- Symlink tuples hash the raw target and use mode `0777`. `observe_portable` normalizes suitable absolute in-home targets to the relative form stored by add. Write expectations use the raw observation.
- `Worktree` reads current disk contents and handles tracked paths that are missing. `[modes]` overrides Git's `0644` and `0755` defaults. Strip special permission bits.
- `Variants` resolves identity-specific sources before rendering templates or decrypting age content. It clears plain physical observations for transformed content. Encrypted files default to `0600`.
- State and mode lookup follow the selected repo path in `SourceEntry.source`.

## Write contract

- `safe_write`, `safe_write_over_link`, and `safe_symlink` converge at `write_node`. Put shared managed-replacement guarantees there.
- `safe_write` also serves restore, selected sync, and repo ciphertext replacement in `../secrets.rs`.
- `WriteOptions.expected` is the reviewed H. Dry runs compare it without creating parents, backups, or temps. Real writes read the target again before replacement and return `Skip::Changed` on mismatch.
- Reject blocked nodes and dotman's own paths. Only `safe_write_over_link` authorizes a regular file to replace the expected symlink.
- File temps are created beside the destination at `0600`, filled, given their final mode, synced, and renamed. Link temps hold the literal target. The containing directory is synced after replacement.
- Create parent directories with recorded directory modes or `0755`. Apply modes through a no-follow directory handle.
- Atomic replacement breaks hard links and reports that fact. The explicit in-place fallback applies only to regular files after a busy or cross-device rename failure. It uses a no-follow open and sets mode before contents.
- Apply schedules a byte/link backup when kind or hash differs, and for explicit in-place writes, regardless of whether H equals S. A mode-only change records the old mode in `.dotman/modes.txt` instead.
- The writer uses `WriteOptions.backup` to decide whether to create a backup. Backup files use `0600` and directories use `0700`. Preserve existing byte backups and record link targets in `.dotman/links.txt`.
- Apply's `kept_mode` normally calls `never_widen`: group/other read-write bits cannot increase, and execute bits follow its readability/prior-execute rule.
- Mode exceptions are deliberate: new targets, non-file transitions, explicit source `[modes]`, or changed repo read-write bits relative to the basis take the repo mode directly.
- Farm swaps use the replacement and restoration helpers in `write.rs`.

## Checks

```bash
cargo test --lib engine::
cargo test --test cli_apply
cargo test --test cli_variants --test cli_templates
cargo test --test cli_selected --test cli_secrets
cargo test --test security_tests
```

For covered behavior changes, update the corresponding `e2e/journeys/07-modes.sh`, `08-symlinks.sh`, `09-variants.sh`, or `17-backups.sh` scenario. Farm replacement changes also need the importer scenarios in `../../tests/cli_import_farm.rs` and `../../e2e/import/05-farm.sh`.
