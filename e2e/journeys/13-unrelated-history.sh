#!/usr/bin/env bash
# Unrelated remote history: sync refuses with exit 1, and the printed start-over recipe keeps
# what only this repo had.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/13-unrelated-history
e2e_machines a b

e2e_write a .bashrc $'export A=1\n'
e2e_init a alpha
e2e_dm_rc 0 a add .bashrc
e2e_dm_rc 0 a sync
remote_before=$(e2e_tool git --git-dir="$(e2e_remote)" rev-parse main)

# b made its own repo with the same remote instead of cloning it.
e2e_write b .vimrc $'set number\n'
e2e_init b beta
e2e_dm_rc 0 b add .vimrc
e2e_dm_rc 1 b sync
e2e_err_has 'shares no history with the repo'
e2e_check "nothing was pushed" test "$(e2e_tool git --git-dir="$(e2e_remote)" rev-parse main)" = "$remote_before"
e2e_file_is b .vimrc $'set number\n'

e2e_run_recipe b '^(mv |dotman import )' 3
e2e_dm_rc 0 b sync
e2e_remote_tracks .vimrc
e2e_remote_tracks .bashrc
e2e_file_is b .bashrc $'export A=1\n'
e2e_dm_rc 0 a sync
e2e_file_is a .vimrc $'set number\n'
for m in a b; do
    e2e_dm_rc 0 "$m" status
    e2e_err_has '2 managed, 2 clean, 0 to look at'
done

e2e_finish
