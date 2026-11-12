#!/usr/bin/env bash
# Dry runs are pure: every --dry-run form leaves every HOME, repo, state, config and the remote
# unchanged, where the same command without it would change them.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/16-dry-run
e2e_machines a b c

e2e_dry_run a init --no-import --remote "$(e2e_remote_url)" --host alpha --no-system --no-key
e2e_write a .bashrc $'export A=1\n'
e2e_write a .vimrc $'set number\n'
e2e_write a .config/app/app.conf $'v=1\n'
e2e_init a alpha
e2e_dry_run a add .bashrc .vimrc .config/app
e2e_dm_rc 0 a add .bashrc .vimrc .config/app
e2e_dry_run a sync
e2e_dm_rc 0 a sync

e2e_dry_run b init "$(e2e_remote_url)" --host beta
e2e_dry_run b import "$(e2e_remote_url)"
e2e_join b beta

# Pending work on both sides: a local edit, an incoming edit, a deletion and a new file.
e2e_write a .bashrc $'export A=2\n'
e2e_write a .config/app/new.conf $'n=1\n'
e2e_dm_rc 0 a sync
e2e_write b .vimrc $'set nonumber\n'
rm -- "$(e2e_home b)/.config/app/app.conf"
e2e_dm_rc 0 b git fetch -q origin

for args in "sync" "sync --keep-local" "sync --take-repo" "sync --forget-deleted" \
    "sync --restore-deleted" "apply" "apply --take-repo" "apply --keep-local" \
    "apply --restore-deleted" "apply --forget-deleted" "forget .vimrc" "forget .config/app" \
    "restore .vimrc --from HEAD"; do
    read -ra argv <<<"$args"
    e2e_dry_run b "${argv[@]}"
done

e2e_write c .zshrc $'setopt autocd\n'
e2e_dry_run c init --no-import --host gamma --no-remote --no-system --no-key
e2e_dm_rc 0 c init --no-import --host gamma --no-remote --no-system --no-key
e2e_dry_run c add .zshrc

# The same commands for real do change things, so the dry runs above had work to skip.
before=$(e2e_snapshot)
e2e_dm_rc 0 b sync --restore-deleted
e2e_check "the real sync changed something" test "$before" != "$(e2e_snapshot)"
e2e_file_is b .bashrc $'export A=2\n'
e2e_file_is b .config/app/app.conf $'v=1\n'

e2e_finish
