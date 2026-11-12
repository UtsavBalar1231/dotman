#!/usr/bin/env bash
# Local deletions: left undecided unattended, restored with --restore-deleted, then forgotten.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/04-deletions
e2e_machines a b

e2e_write a .bashrc $'export A=1\n'
e2e_write a .vimrc $'set number\n'
e2e_init a alpha
e2e_dm_rc 0 a add .bashrc .vimrc
e2e_dm_rc 0 a sync
e2e_join b beta

rm -- "$(e2e_home b)/.bashrc" "$(e2e_home b)/.vimrc"
e2e_dm_rc 0 b status
e2e_out_has 'deleted-locally'
e2e_dm_rc 3 b sync
e2e_err_has 'local deletions left undecided (2)'
e2e_remote_tracks .bashrc
e2e_remote_tracks .vimrc

e2e_run_printed b "$(e2e_printed 'sync --restore-deleted')"
e2e_file_is b .bashrc $'export A=1\n'
e2e_file_is b .vimrc $'set number\n'

rm -- "$(e2e_home b)/.vimrc"
e2e_dm_rc 3 b sync
e2e_run_printed b "$(e2e_printed 'sync --forget-deleted')"
e2e_remote_lacks .vimrc
e2e_remote_tracks .bashrc
e2e_absent b .vimrc

e2e_dm_rc 0 a sync
e2e_err_has '.vimrc: forgotten upstream; the file stays in place'
e2e_file_is a .vimrc $'set number\n'
e2e_dm_rc 0 a list
e2e_out_lacks .vimrc

e2e_finish
