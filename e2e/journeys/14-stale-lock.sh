#!/usr/bin/env bash
# A stale .git/*.lock left by a killed git: sync and doctor stop with the `rm` recovery, which
# works as printed.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/14-stale-lock
e2e_machines a

e2e_write a .bashrc $'export A=1\n'
e2e_init a alpha
e2e_dm_rc 0 a add .bashrc
e2e_dm_rc 0 a sync

e2e_write a .bashrc $'export A=2\n'
: >"$(e2e_repo a)/.git/index.lock"
: >"$(e2e_repo a)/.git/MERGE_MSG.lock"
e2e_dm_rc 1 a sync
e2e_err_has "git's lock files are in the repo"
e2e_err_has 'index.lock'
e2e_err_has 'MERGE_MSG.lock'
e2e_file_is a .bashrc $'export A=2\n'
e2e_dm_rc 1 a doctor
e2e_out_has 'fail  git locks'

e2e_run_recipe a '^rm ' 1
e2e_check "no lock is left" test -z "$(find "$(e2e_repo a)/.git" -maxdepth 1 -name '*.lock')"
e2e_dm_rc 0 a sync
e2e_remote_tracks .bashrc
e2e_check "the edit reached the remote" test "$(e2e_remote_show .bashrc)" = 'export A=2'

e2e_finish
