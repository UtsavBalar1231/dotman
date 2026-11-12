#!/usr/bin/env bash
# Mass delete: a broken checkout is refused and restored; forget-then-allow stops managing a dir.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/12-mass-delete
e2e_machines a b

for i in $(seq 1 12); do
    e2e_write a ".config/many/f$i" "file $i
"
done
e2e_write a .bashrc $'export A=1\n'
e2e_init a alpha
e2e_dm_rc 0 a add .config/many .bashrc
e2e_dm_rc 0 a sync
e2e_join b beta
remote_before=$(e2e_tool git --git-dir="$(e2e_remote)" rev-parse main)

# A checkout that lost most files, as a killed clone leaves.
rm -rf -- "$(e2e_repo a)/.config"
e2e_dm_rc 1 a sync
e2e_err_has 'delete 12 of its 15 tracked files'
e2e_check "nothing was pushed" test "$(e2e_tool git --git-dir="$(e2e_remote)" rev-parse main)" = "$remote_before"
e2e_run_printed a "$(e2e_printed 'git checkout -- \.')"
e2e_dm_rc 0 a sync
e2e_remote_tracks .config/many/f12

# Stop managing the directory: its files stay in $HOME on a and are left alone on b.
rm -rf -- "$(e2e_repo a)/.config"
e2e_dm_rc 1 a sync
e2e_run_printed a "$(e2e_printed '^dotman forget ')"
e2e_run_printed a "$(e2e_printed 'sync --allow-mass-delete')"
e2e_remote_lacks .config/many/f1
e2e_remote_tracks .bashrc
e2e_file_is a .config/many/f12 $'file 12\n'
e2e_dm_rc 0 b sync
e2e_file_is b .config/many/f12 $'file 12\n'
e2e_dm_rc 0 a status
e2e_err_has '1 managed, 1 clean, 0 to look at'

e2e_finish
