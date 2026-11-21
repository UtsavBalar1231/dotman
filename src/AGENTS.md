# Shared CLI services

This layer resolves machine policy, isolates Git, tracks local state, and owns the boundaries used by commands and the engine.

## Where to look

| Concern | Owner |
|---|---|
| Paths, reserved keys, atomic control-file writes | `paths.rs` |
| Resolved dependencies, lock, operation guard | `context.rs`, `lock.rs` |
| Git subprocesses, staging, pull recovery | `git.rs` |
| Machine policy / tracked repo policy | `config.rs` / `repo_config.rs` |
| Applied state and approval snapshots | `state.rs` |
| Identity, source selectors, template variables | `identity.rs`, `variant.rs`, `template.rs` |
| Credential detection and age integration | `secrets.rs`, `proc.rs` |
| Post-apply approval and pending hooks | `hooks.rs` |
| System scope / escalation / privileged execution | `system.rs`, `system/escalate.rs`, `system/helper.rs` |
| Init-generated bootstrap | `bootstrap.rs`, `bootstrap/install.sh.in` |

Read `commands/AGENTS.md`, `engine/AGENTS.md`, or `import/AGENTS.md` for their own policy and algorithms.

## Git isolation and recovery

- Route Git subprocesses through `Git`. `Git::base` applies `ISOLATION`, removes `LOCAL_REPO_ENV`, and confines repository discovery with `GIT_CEILING_DIRECTORIES`.
- Preserve the isolation flags, including `core.excludesFile=`, `core.hooksPath=/dev/null`, `core.symlinks=true`, and the default rename behavior. Init and clone use `--template=`. Clone also uses `--no-local` so a local source goes through Git transport.
- Request `-z` for parsed path output. Retain Unix path bytes, including whitespace and newlines.
- Large staging exclusions belong in NUL-delimited stdin pathspecs (`--pathspec-from-file=- --pathspec-file-nul`), using literal exclusions. Argument lists can exceed `ARG_MAX`.
- `.git/info/attributes` must end with `* merge -text -filter -ident -working-tree-encoding`. Preserve user lines and remove superseded dotman lines when refreshing.
- Bare `merge` selects Git's built-in text driver. `merge=text` can select a configured driver, and `-merge` selects binary handling. Keep `merge` first on the pinned line so Git's backward attribute scan sets the four other flags before it reaches a tracked `[attr]merge` macro.
- Pull and replay use the merge backend. `pull_backend` selects `recursive` on Git 2.25 through 2.33 and `ort` from 2.34. Keep printed recovery pulls consistent with this choice and `PULL_PINS`.
- `rebase.updateRefs=false` and `rerere.enabled=false` prevent moving unrelated branches or silently reusing resolutions. Recovery recipes must preserve these constraints too.
- The `.git/dotman-pull` marker belongs to replay recovery. Route recovery/refusal through `Git::recover_pull` and `Context`, preserving saved commits and worktree state.

## Context, state, and keys

- `Context::lock(dry_run)` acquires the command lock, rechecks reviewed input, recovers interrupted pulls only outside dry runs, then guards the repo. Keep that order.
- After the guard, non-dry writes refresh pinned attributes only for an initialized plain `.git` directory. On refresh failure the command warns and continues.
- Resolve machine paths and cached identity through `Context`. Do not independently reload identity partway through a decision or template render.
- `State::load` treats missing state as fresh and rejects malformed, empty, or unsupported-version state. A different repo ID clears repo-specific state but retains the identity snapshot.
- `State::save` writes `$XDG_STATE_HOME/dotman/state.toml` through temp, fsync, and rename. Keep captured-directory, ignore, and hook approval state local and bound to `.git/dotman-id`.
- Validate keys through `paths::validate_key` and `Context::check_key`. Reserved root names include `.git`, `.dotman`, `.dotman.toml`, `install.sh`, and names beginning `readme` or `license`, case-insensitively. Reject control and hidden-format characters and all `.git` components, including NTFS spellings.
- Preserve case-folding/NFC collision checks across home and system keys before writes. A linked worktree's `.git` file is not a supported managed repo.

## Policy ownership

- `Config` owns private machine policy and `RepoConfig` owns tracked `.dotman.toml`. Credential allow rules, template environment permissions, the age executable and identity, and system roots and escalation stay machine-local.
- Preserve stable `BTreeMap` serialization in repo settings, with defaults for missing sections and errors for unknown tracked fields.
- `secrets.rs` combines `RULES` in one scanner and runs the age rule separately. Content scans skip binary files. `held` checks the key name before the bytes, and `held_link` checks the name before the target. Per-machine allow entries apply only to eligible findings in file contents. Plaintext age identities cannot be approved.
- Templates consume the resolved `Vars`. `env` exposes only names permitted by `[template] env` in machine policy.
- Hook approval covers the canonical JSON of the ordered command table. Preserve pending keys and the last displayed table so a changed table cannot inherit approval.
- `hooks.rs` owns execution, deferral, and reviewed approval. A clone's first apply uses `never_ask` so it cannot approve fetched hooks implicitly.

## System and bootstrap boundaries

- `system.rs` owns root matching and source eligibility. `system/escalate.rs` selects sudo, doas, run0, or caller-rights execution. Keep plan execution in `system/helper.rs`.
- The helper rereads the invoking user's plan and machine config, checks ownership, and requires planned roots to equal configured usable roots. Caller-side validation is not sufficient.
- At effective UID 0, `gate` rejects both a plan prefix and `DOTMAN_SYSTEM_ROOT`. Non-root execution requires `escalate = "none"`.
- Validate the entire plan before writing. Preserve target symlink refusal and the descriptor-relative `openat(O_NOFOLLOW)` walks and writes through `sys.rs`.
- `bootstrap/install.sh.in` produces the repo's `install.sh`. Its second-line version and hash identify unedited output. Preserve user-edited scripts.
