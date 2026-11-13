#!/usr/bin/env bash
# Held secret-named and credential files: hints never offer --allow-plain; add --encrypt resolves them.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/05-held-secrets
e2e_machines a b

token=$(e2e_token)
e2e_write a .config/app/app.conf $'theme=dark\n'
e2e_write a .config/app/.env $'API_URL=https://example.invalid\n'
e2e_write a .config/app/remote.conf "url = https://example.invalid
token = $token
"
e2e_init a alpha
e2e_dm_rc 0 a key init
e2e_identity_known "$(e2e_config a)/age-identity.txt"

e2e_dm_rc 3 a add .config/app
e2e_err_has 'held .config/app/.env'
e2e_err_has 'held .config/app/remote.conf: line 2 holds a github-token'
e2e_err_line_lacks remote.conf --allow-plain
e2e_err_lacks "$token"
e2e_dm_rc 3 a sync
e2e_check "sync ends on the line that says something is left" test \
    "$(tail -n 1 "$E2E_ROOT/last.err")" = \
    'not everything was synced; the warnings above say what is left and what to run'
e2e_remote_tracks .config/app/app.conf
e2e_remote_lacks .config/app/.env
e2e_remote_lacks .config/app/remote.conf

# New files under a managed dir go through the same guards at sync.
e2e_write a .config/app/deploy.conf "deploy_token = $token
"
e2e_dm_rc 3 a sync
e2e_err_has '.config/app/deploy.conf (line 1 holds a github-token'
e2e_err_line_lacks deploy.conf --allow-plain
e2e_err_line_lacks remote.conf --allow-plain
e2e_remote_lacks .config/app/deploy.conf
e2e_dm_rc 0 a status
e2e_err_has '3 held back'

e2e_dm_rc 3 a add .config/app
mapfile -t encrypt < <(e2e_recipe | grep -E -- '^dotman add --encrypt ' || true)
e2e_check "add prints one --encrypt command per held file" test "${#encrypt[@]}" = 3
for cmd in "${encrypt[@]}"; do
    e2e_run_printed a "$cmd"
done
e2e_dm_rc 0 a sync
e2e_dm_rc 0 a status
e2e_err_has '4 managed, 4 clean, 0 to look at'
e2e_remote_tracks '.config/app/remote{age}.conf'
e2e_remote_tracks '.config/app/deploy{age}.conf'
e2e_remote_tracks '.config/app/.env{age}'
e2e_mode_cap .config/app/remote.conf 600

e2e_join b beta --age-key-file "$(e2e_config a)/age-identity.txt"
e2e_file_has b .config/app/remote.conf "$token"
e2e_mode_is b .config/app/remote.conf 600

# A token put into a managed file: status names the hold, and the printed `forget` ends it.
e2e_write a .config/app/app.conf "theme=dark
token = $token
"
e2e_dm_rc 0 a status
e2e_out_has 'held in $HOME; `dotman sync` captures none of it:'
e2e_out_has '.config/app/app.conf: line 2 holds a github-token'
e2e_err_has '1 held in $HOME'
e2e_run_printed a "$(e2e_printed '^dotman forget ')"
e2e_dm_rc 0 a sync
e2e_remote_lacks .config/app/app.conf

e2e_finish
