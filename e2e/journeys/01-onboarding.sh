#!/usr/bin/env bash
# Onboarding: init and add on machine A, init <url> on B, edits on both converge.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/01-onboarding
e2e_machines a b

e2e_write a .bashrc $'export EDITOR=vi\n'
e2e_write a .config/app/app.conf $'theme=dark\n'
e2e_write a .config/app/themes/light.conf $'bg=white\n'
e2e_dm_rc 0 a init --no-import --remote "$(e2e_remote_url)" --host alpha --no-system --no-key
e2e_dm_rc 0 a add .bashrc .config/app
e2e_dm_rc 0 a sync
e2e_err_has 'pushed 1 commit to origin/main'
e2e_remote_tracks .bashrc
e2e_remote_tracks .config/app/themes/light.conf

e2e_dm_rc 0 b init "$(e2e_remote_url)" --host beta
e2e_file_is b .bashrc $'export EDITOR=vi\n'
e2e_file_is b .config/app/themes/light.conf $'bg=white\n'
e2e_dm_rc 0 b status
e2e_err_has '3 managed, 3 clean, 0 to look at'

e2e_write a .bashrc $'export EDITOR=nvim\n'
e2e_write b .config/app/app.conf $'theme=light\n'
e2e_write b .config/app/themes/new.conf $'bg=grey\n'
e2e_dm_rc 0 a sync
e2e_dm_rc 0 b sync
e2e_dm_rc 0 a sync

for m in a b; do
    e2e_file_is "$m" .bashrc $'export EDITOR=nvim\n'
    e2e_file_is "$m" .config/app/app.conf $'theme=light\n'
    e2e_file_is "$m" .config/app/themes/new.conf $'bg=grey\n'
    e2e_dm_rc 0 "$m" status
    e2e_err_has '4 managed, 4 clean, 0 to look at'
done
e2e_check "both repos are at the remote's main" test \
    "$(e2e_git a rev-parse HEAD)" = "$(e2e_git b rev-parse HEAD)"

e2e_finish
