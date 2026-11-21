# Shell scenarios

The Bash scenarios run the real CLI across disposable machines and a shared local bare remote.

## Where to look

| Surface | Files |
|---|---|
| Suite selection, timings and aggregate counts | `run.sh` |
| Machines, command runners, assertions and invariants | `lib.sh` |
| User workflows and printed recovery recipes | `journeys/*.sh` |
| SIGKILL recovery and interrupted JSON output | `faults/kill.sh`, `faults/json-interrupt.sh` |
| Legacy, Git, chezmoi and symlink-farm imports | `import/*.sh` |
| Separate Docker distro matrix | `distros/run.sh` |

## Running scenarios

- `just e2e` builds a locked release binary unless `DOTMAN_BIN` already names an executable. Set `DOTMAN_BIN` when calling `run.sh` directly.
- `run.sh` accepts suite names, scenario basenames or file paths. With no arguments it runs all suites except `distros/`.
- The runner enables `E2E_STATUS_PARITY=1` for journeys. This checks porcelain and JSON status counts, framing, and exit codes after `e2e_dm`. Set it explicitly to get parity checks when invoking a scenario directly.
- `E2E_OUTCOME_JSON=1` adds JSON output to `apply`, `sync` and `add` calls through `e2e_dm`, then checks framing, final exit and decision parity. JSON checks require `jq`.
- `e2e_begin` requires git, age, age-keygen, and bash, plus tools named by the scenario.
- `just e2e-keep` or `E2E_KEEP=1` preserves each temporary root. Inspect its `log`, `last.out`, `last.err`, machine trees, and `ledger/`.

## Scenario conventions

- Start with `set -euo pipefail`, source `../lib.sh` with its shellcheck source annotation, call `e2e_begin suite/name [TOOLS...]`, and finish with `e2e_finish`.
- Create devices with `e2e_machines a b`. `e2e_init a HOST` creates the first repo and `e2e_join b HOST` clones the shared `file://` remote.
- `e2e_on` guards paths and runs under `env -i` with per-machine HOME, XDG directories, runtime directory, TMPDIR, PATH and Git identity. HTTP(S) Git URLs are blocked and only file transport is allowed.
- Use `e2e_git` for a machine's repo and `e2e_tool` for tools outside machines. Both retain fixture isolation. Install executable stubs with `e2e_stub`.
- `e2e_dm` captures `E2E_RC`, `E2E_OUT` and `E2E_ERR` and does not fail on the product's exit status. Use `e2e_dm_rc EXPECTED MACHINE ARGS...` or assert with `e2e_rc_is`.
- Use `e2e_check DESCRIPTION COMMAND...` and `e2e_check_not` for predicates. Assertions increment failure counters and `e2e_finish` sets the scenario exit.
- `e2e_err_has`/`e2e_out_has` and their `lacks` partners match fixed strings. Check file bytes, absence and modes with `e2e_file_is`, `e2e_absent` and `e2e_mode_is`.
- Use `e2e_write` and `e2e_write_repo` for hand edits: they populate the no-loss ledger. Register decryption identities with `e2e_identity_known` or create them with `e2e_age_identity`.
- `e2e_finish` checks retained hand edits, secrets in remote history, committed temporary paths and mode constraints. Use `e2e_mode_cap` for scenario-specific permission bounds and `e2e_dry_run` to compare complete snapshots.
- Extract recovery commands with `e2e_recipe`/`e2e_printed` and execute them through `e2e_run_printed`/`e2e_run_recipe`.
- Give each new `lib.sh` helper a one-line comment stating its contract.

## Gates and fault controls

- Chezmoi import skips when `chezmoi` is missing. `E2E_REQUIRE_CHEZMOI=1` turns that into failure.
- `E2E_KILL_ITERS` defaults to 3 per operation. `E2E_KILL_SEED` fixes generated edits and delays. `E2E_KILL_OPS` selects from `sync apply add import init key conflict`.
- The distro matrix requires Docker. `just e2e-distros [IMAGE...]` builds static musl unless `DOTMAN_BIN_MUSL` is supplied. Scenarios run as uid 1000 after container package setup.

## Focused checks

```bash
just e2e 02-conflict
just e2e-keep import
E2E_KILL_OPS=sync E2E_KILL_ITERS=1 just e2e kill
E2E_OUTCOME_JSON=1 just e2e journeys
```
