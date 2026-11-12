#!/usr/bin/env bash
# A remote that cannot be reached: the commit stays local, status counts it, and the next sync
# pushes it.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/15-unreachable-remote
e2e_machines a b

e2e_write a .bashrc $'export A=1\n'
e2e_init a alpha
e2e_dm_rc 0 a add .bashrc
e2e_dm_rc 0 a sync
e2e_join b beta

mv -- "$(e2e_remote)" "$(e2e_remote).away"
e2e_write a .bashrc $'export A=2\n'
e2e_dm_rc 1 a sync
e2e_err_has 'could not reach origin; your changes are committed locally and go out on the next sync'
e2e_check "the commit is in a's repo" test "$(e2e_git a show HEAD:.bashrc)" = 'export A=2'
e2e_dm_rc 0 a status
e2e_err_has '1 commit to push'

mv -- "$(e2e_remote).away" "$(e2e_remote)"
e2e_run_printed a "$(e2e_printed '^dotman sync$')"
e2e_check "the edit reached the remote" test "$(e2e_remote_show .bashrc)" = 'export A=2'
e2e_dm_rc 0 b sync
e2e_file_is b .bashrc $'export A=2\n'

e2e_finish
