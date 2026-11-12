#!/usr/bin/env bash
# Hooks: an unapproved table is held back and runs once on approval; a table swapped since the
# warning is refused until it is shown and approved again.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/10-hooks
e2e_machines a b

e2e_write a .config/app/app.conf $'v=1\n'
e2e_init a alpha
e2e_dm_rc 0 a add .config/app
e2e_dm_rc 0 a sync
e2e_join b beta

# Sets the [hooks] table in a's repo to one entry that appends WORD to ~/hook.log.
set_hook() {
    local toml
    toml="$(e2e_repo a)/.dotman.toml"
    sed -i '/^"\.config\/app\/\*\*" = /d' "$toml"
    sed -i "s|^\\[hooks\\]\$|[hooks]\\n\".config/app/**\" = \"echo $1 >> hook.log\"|" "$toml"
}
hook_log() {
    cat -- "$(e2e_home b)/hook.log" 2>/dev/null || true
}

set_hook one
e2e_write a .config/app/app.conf $'v=2\n'
e2e_dm_rc 0 a sync --approve-hooks
e2e_dm_rc 0 b sync
e2e_err_has 'hook not run: the [hooks] table in .dotman.toml is not approved on this machine'
e2e_err_has '.config/app/** -> echo one >> hook.log'
e2e_file_is b .config/app/app.conf $'v=2\n'
e2e_check "no hook ran on b" test -z "$(hook_log)"
e2e_run_printed b "$(e2e_printed 'apply --approve-hooks')"
e2e_check "the held-back hook ran once" test "$(hook_log)" = one
e2e_dm_rc 0 b apply --approve-hooks
e2e_check "it does not run again" test "$(hook_log)" = one

# An approved table that changes upstream is skipped with a warning.
set_hook two
e2e_write a .config/app/app.conf $'v=3\n'
e2e_dm_rc 0 a sync --approve-hooks
e2e_dm_rc 0 b sync
e2e_err_has '.config/app/** -> echo two >> hook.log'
e2e_check "the changed hook did not run" test "$(hook_log)" = one
approve=$(e2e_printed 'apply --approve-hooks')

# The table changes again behind the warning, through a plain pull.
set_hook three
e2e_dm_rc 0 a sync --approve-hooks
e2e_on b dotman git pull -q --rebase origin main >/dev/null 2>&1
e2e_run_printed b "$approve" 3
e2e_check "the swapped table shows the new command" grep -qF 'echo three' "$E2E_ROOT/recipe.out"
e2e_check "nothing ran for the swapped table" test "$(hook_log)" = one
e2e_run_printed b "$approve"
e2e_check "the second approval runs the shown table once" test "$(hook_log)" = $'one\nthree'
e2e_check_not "the unseen table never ran" grep -q two "$(e2e_home b)/hook.log"

e2e_finish
