#!/usr/bin/env bash
# A credential in a repo edit stops sync before the pull (exit 3); the printed remedy keeps the edit.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/06-held-repo-edit
e2e_machines a b

e2e_write a .bashrc $'export A=1\n'
e2e_init a alpha
e2e_dm_rc 0 a add .bashrc
e2e_dm_rc 0 a sync
e2e_join b beta

token=$(e2e_token)
e2e_write_repo a .bashrc "export A=1
export GH_TOKEN=$token
"
e2e_write b .bashrc $'export A=2\n'
e2e_dm_rc 0 b sync

e2e_dm_rc 3 a sync
e2e_err_has 'sync stopped before the pull'
e2e_err_has '.bashrc: line 2 holds a github-token'
e2e_err_lacks '--allow-plain'
e2e_err_lacks "$token"
e2e_file_is a .bashrc $'export A=1\n'
e2e_check "a has not pulled" test "$(e2e_git a rev-parse HEAD)" != \
    "$(e2e_tool git --git-dir="$(e2e_remote)" rev-parse main)"
e2e_dm_rc 0 a status
e2e_err_has 'held in the repo'

e2e_run_printed a "$(e2e_printed 'git diff -- ')"
# The remedy's prose step: keep a copy, without the secret, before dropping the edit.
kept="$(e2e_mroot a)/kept-bashrc"
cp -- "$(e2e_repo a)/.bashrc" "$kept"
e2e_run_printed a "$(e2e_printed 'git checkout -- ')"
e2e_check "the kept copy holds the edit" grep -qF 'GH_TOKEN' "$kept"

e2e_dm_rc 0 a sync
e2e_file_is a .bashrc $'export A=2\n'
e2e_file_lacks a .bashrc '<<<<<<<'
e2e_dm_rc 0 a status
e2e_err_has '1 managed, 1 clean, 0 to look at'

e2e_finish
