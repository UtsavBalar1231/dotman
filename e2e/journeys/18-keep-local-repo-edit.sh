#!/usr/bin/env bash
# Keep-local and forget save an uncommitted repo edit under .dotman/repo/ in the backups first.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/18-keep-local-repo-edit
e2e_machines a

e2e_write a .bashrc $'export A=1\n'
e2e_write a .vimrc $'set number\n'
e2e_init a alpha
e2e_dm_rc 0 a add .bashrc .vimrc
e2e_dm_rc 0 a sync

e2e_write_repo a .bashrc $'export A=repo\n'
e2e_write a .bashrc $'export A=home\n'
e2e_dm_rc 3 a apply
e2e_err_has '.bashrc'
e2e_run_printed a "$(e2e_printed 'apply --keep-local')"
e2e_file_is a .bashrc $'export A=home\n'
saved=$(find "$(e2e_backups a)" -path '*/.dotman/repo/.bashrc' -print)
e2e_check "the repo edit is saved under .dotman/repo/" test -n "$saved"
e2e_check "the saved copy holds the repo edit" grep -qx 'export A=repo' "${saved:-/dev/null}"
e2e_dm_rc 0 a sync
e2e_check "the remote has $HOME's version" test "$(e2e_remote_show .bashrc)" = 'export A=home'

e2e_write_repo a .vimrc $'set relativenumber\n'
e2e_dm_rc 0 a forget .vimrc
saved=$(find "$(e2e_backups a)" -path '*/.dotman/repo/.vimrc' -print)
e2e_check "forget saves the repo edit it removes" grep -qx 'set relativenumber' "${saved:-/dev/null}"
e2e_file_is a .vimrc $'set number\n'
e2e_dm_rc 0 a sync
e2e_remote_lacks .vimrc

e2e_finish
