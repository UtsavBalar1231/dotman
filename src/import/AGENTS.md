# Import conversion

Import converts a source into a Dotman repo and accounts for entries that need user action.

## Where to look

| Task | Location | Boundary |
|---|---|---|
| Dispatch, report, source overlap, freshness | `mod.rs` | `run`, `convert`, `Report`, `Source` |
| Discovery, Git conversion, history scan, staging | `gittree.rs` | Also supports bare repos and vcsh |
| Legacy zstd/bincode history and overlay | `legacy.rs` | Rebuilds snapshots through Git fast-import |
| yadm alternates and template translation | `yadm.rs` | Exact conversion or an explicit refusal |
| chezmoi tree screening and invocation | `chezmoi.rs` | Isolated default versus opted-in rendering |
| Symlink-farm import and replacement | `farm.rs` | `--swap` operates on a prior import |

## Selection and accounting

- Without a source, discovery checks legacy Dotman, yadm, home bare repos, vcsh, chezmoi, and symlink farms. One candidate proceeds. Multiple candidates produce a list and exit 3.
- Explicit sources are local paths resolved to absolute paths or Git URLs. Dispatch checks legacy, chezmoi, fresh farm import, then Git. This lets it recognize chezmoi and farm sources that also have a Git checkout.
- Refuse sources that equal, contain, or lie inside the destination repo, comparing resolved paths even when the destination does not yet exist.
- Fresh import requires no existing commit or managed file. `--overlay` accepts legacy and Git sources into an existing clean repo.
- Overlay compares the source with existing repo entries and reports differences. `--include-missing` opts into adding entries absent from the repo. Preserve the legacy overlay's file-mode repair behavior.
- An unedited, untracked generated `install.sh` is the clean-worktree exception and joins the overlay commit. Fresh import writes that bootstrap afterward unless `--no-bootstrap`.
- `Report` counts each source entry once as converted or needing action. Entries needing action produce exit 3. Keep paths and notes escaped onto single output lines.
- Accounting is format-specific: legacy counts a key across history, while farms count eligible home links and files within folded directory links. Do not silently discard unsupported entries.

## Staging and history

- `Staging` uses private temporary directories beside the destination for installation by rename. Dry-run analysis uses system temp, which leaves the destination's parent untouched.
- Preserve target checks and edited configuration backup before installation. `install_keeping_origin` restores an init-configured origin when the imported repo brings none.
- Git import clones the selected HEAD branch with `--single-branch --no-tags` via `Git::clone_head_from`. Do not bring unrelated branches or tags into the retained history.
- Preserve detached/unclonable local HEAD handling: it can create `main` from the selected commit, and must not retain a misleading source origin. Local source origins are reported as hints rather than copied blindly.
- `tainted_history` walks reachable objects, streams blobs and commit objects through `cat-file`, checks paths, and scans commits again as Git displays them.
- Guard the converted tree's content, names, and link targets.
- Tainted history defaults to a sanitized current-state commit. `--keep-history` retains history with a warning. Current-tree guards apply in both cases.
- Legacy import filters held objects from rebuilt snapshots and checks commit metadata and messages. Preserve parent order and source accounting. Report staged-only legacy index changes without importing them.

## Format-specific boundaries

- Default chezmoi import copies screened entries into private scratch storage with isolated config, state, cache, and source. It uses read commands and sets the destination to home for target lookup. Templates, scripts, encrypted entries, and externals are excluded or reported.
- `--render-templates` is a distinct trust boundary: chezmoi uses the user's config and hooks, full template functions, and template-capable special files. Its rendered outputs become plain host-specific files.
- A dry run with `--render-templates` reports the default view and explains that rendering is disabled.
- Preserve yadm's selector winner rules and supported default-template semantics. Report unsupported processors or inexpressible selectors.
- Farm import copies what home exposes through eligible links and leaves those links and the farm untouched. It does not run Stow, dotbot, or farm scripts.
- Farm discovery inspects home links within depth four and requires the first hop to lie under the farm. Report Nix-store links for action.
- `--swap` replaces only matching managed links with real files/directories and records link targets before replacement. Content differing from the repo stays in place and is reported.
- On replacement failure, restore attempted swaps in reverse. Rollback is conditional: a target no longer matching what swap wrote is preserved and reported, and restoration failures remain visible in the link record.
- Keep swap filesystem operations in `../engine/write.rs` and update state only after the replacement batch succeeds. Users must stop using farm unlink commands on swapped paths.

## Checks

```bash
cargo test --lib import::
cargo test --test cli_import --test cli_import_git
cargo test --test cli_import_chezmoi --test cli_import_farm
cargo test --test cli_init_detect
cargo test --test security_tests
```

[Importer scenarios](../../e2e/import/) cover format journeys, tainted history, and local source paths. [The shell test guide](../../e2e/AGENTS.md) lists the chezmoi requirement and skip controls.
