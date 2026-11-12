#!/usr/bin/env bash
# A conflict on one file edited on both machines: exit 4, the printed steps run as printed.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/02-conflict
e2e_machines a b

e2e_write a .bashrc $'alias ll="ls -l"\n'
e2e_init a alpha
e2e_dm_rc 0 a add .bashrc
e2e_dm_rc 0 a sync
e2e_join b beta

e2e_write a .bashrc $'alias ll="ls -la"\n'
e2e_write b .bashrc $'alias ll="ls -lh"\n'
e2e_dm_rc 0 a sync
e2e_dm_rc 4 b sync
e2e_err_has 'the pull conflicts with your changes'
e2e_err_has 'conflicting paths:'
e2e_file_is b .bashrc $'alias ll="ls -lh"\n'

mapfile -t steps < <(e2e_recipe)
e2e_check "the recipe prints 4 commands" test "${#steps[@]}" = 4
e2e_check "the first step pulls" test "${steps[0]:-}" = 'dotman git pull --rebase origin main'
# The pull stops at the conflict, which is what the next step resolves.
e2e_run_printed b "${steps[0]}" 1
e2e_check "the repo copy holds conflict markers" grep -q '^<<<<<<<' "$(e2e_repo b)/.bashrc"
printf 'alias ll="ls -la"\nalias lh="ls -lh"\n' >"$(e2e_repo b)/.bashrc"
e2e_ledger b:resolved "$(e2e_repo b)/.bashrc"
for step in "${steps[@]:1}"; do
    e2e_run_printed b "$step"
done

e2e_file_is b .bashrc $'alias ll="ls -la"\nalias lh="ls -lh"\n'
e2e_dm_rc 0 a sync
e2e_file_is a .bashrc $'alias ll="ls -la"\nalias lh="ls -lh"\n'
e2e_check "b's edit is in a backup on b" grep -rqF 'alias ll="ls -lh"' "$(e2e_backups b)"
e2e_dm_rc 0 b status
e2e_err_has '1 managed, 1 clean, 0 to look at'

e2e_finish
