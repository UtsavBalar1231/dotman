#!/usr/bin/env bash
# An encrypted {age} file edited on both machines: the printed pick-a-side steps keep one whole.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/03-age-conflict
e2e_machines a b

e2e_write a .secret.env $'TOKEN=one\n' 600
e2e_mode_cap .secret.env 600
e2e_init a alpha
e2e_dm_rc 0 a key init
key="$(e2e_config a)/age-identity.txt"
e2e_identity_known "$key"
e2e_dm_rc 0 a add --encrypt .secret.env
e2e_dm_rc 0 a sync
e2e_remote_tracks '.secret{age}.env'
e2e_remote_never_holds 'TOKEN=one'
e2e_join b beta --age-key-file "$key"
e2e_file_is b .secret.env $'TOKEN=one\n'

e2e_write a .secret.env $'TOKEN=two\n' 600
e2e_write b .secret.env $'TOKEN=three\n' 600
e2e_dm_rc 0 a sync
e2e_dm_rc 4 b sync
e2e_err_has 'Encrypted files cannot be merged'
e2e_file_is b .secret.env $'TOKEN=three\n'

e2e_run_printed b "$(e2e_printed 'pull --rebase')" 1
e2e_run_recipe b "checkout --theirs|git add|rebase --continue" 3
e2e_run_printed b "$(e2e_printed '^dotman sync$')"

e2e_file_is b .secret.env $'TOKEN=three\n'
e2e_dm_rc 0 a sync
e2e_file_is a .secret.env $'TOKEN=three\n'
e2e_check "a's version is in a backup on a" grep -rqF 'TOKEN=two' "$(e2e_backups a)"
for m in a b; do
    e2e_mode_is "$m" .secret.env 600
    e2e_dm_rc 0 "$m" status
    e2e_err_has '1 managed, 1 clean, 0 to look at'
done

e2e_finish
