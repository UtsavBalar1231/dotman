#!/usr/bin/env bash
# A conflict on one file edited on both machines: exit 4 with the three answers, and the printed
# --take-repo settles it, with this machine's version saved first.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/02-conflict jq
e2e_machines a b

e2e_write a .bashrc $'alias ll="ls -l"\n'
e2e_init a alpha
e2e_dm_rc 0 a add .bashrc
e2e_dm_rc 0 a sync
e2e_join b beta

e2e_write a .bashrc $'alias ll="ls -la"\n'
e2e_write b .bashrc $'alias ll="ls -lh"\n'
e2e_dm_rc 0 a sync
e2e_dm_rc 4 b sync
e2e_err_has 'the pull conflicts with your changes'
e2e_err_has 'conflicting paths:'
e2e_err_lacks 'git pull'
e2e_file_is b .bashrc $'alias ll="ls -lh"\n'

mapfile -t answers < <(e2e_recipe)
e2e_check "the report prints 3 commands" test "${#answers[@]}" = 3
e2e_check "the first keeps this machine's side" test "${answers[0]:-}" = 'dotman sync --keep-local'
e2e_check "the second takes origin's side" test "${answers[1]:-}" = 'dotman sync --take-repo'
e2e_check "the third asks on a terminal" test "${answers[2]:-}" = 'dotman sync'
e2e_run_printed b "${answers[1]:-}"
for dir in rebase-merge rebase-apply dotman-pull; do
    e2e_check "no $dir is left" test ! -e "$(e2e_repo b)/.git/$dir"
done

e2e_file_is b .bashrc $'alias ll="ls -la"\n'
e2e_dm_rc 0 a sync
e2e_file_is a .bashrc $'alias ll="ls -la"\n'
e2e_check "b's version is saved on b" grep -rqF 'alias ll="ls -lh"' "$(e2e_backups b)"
e2e_dm_rc 0 b status
e2e_err_has '1 managed, 1 clean, 0 to look at'

e2e_write_repo b .bashrc $'repo-reviewed\n'
e2e_write b .bashrc $'home-reviewed\n'
review="$E2E_ROOT/conflict-decisions.json"
e2e_review b apply "$review" .bashrc take_repo
e2e_write b .bashrc $'home-later\n'
e2e_decided 3 b apply "$review"
e2e_file_is b .bashrc $'home-later\n'
e2e_review b apply "$review" .bashrc take_repo
e2e_decided 0 b apply "$review"
e2e_file_is b .bashrc $'repo-reviewed\n'
e2e_check "the decided write backed up the losing home bytes" grep -rqF home-later "$(e2e_backups b)"

rm -- "$(e2e_home b)/.bashrc"
e2e_review b apply "$review" .bashrc restore
e2e_decided 0 b apply "$review"
e2e_file_is b .bashrc $'repo-reviewed\n'
e2e_git b add -- .bashrc
e2e_git b -c core.hooksPath=/dev/null commit -qm restored-version
rm -- "$(e2e_home b)/.bashrc"
e2e_write_repo b .bashrc $'repo-before-forget\n'
e2e_review b apply "$review" .bashrc forget
e2e_decided 0 b apply "$review"
e2e_check "the reviewed forget removed the repo source" test ! -e "$(e2e_repo b)/.bashrc"
e2e_check "the reviewed forget saved its uncommitted repo edit" grep -rqF repo-before-forget "$(e2e_backups b)"

e2e_finish
