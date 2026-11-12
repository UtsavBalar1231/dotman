#!/usr/bin/env bash
# Symlinks: a managed link reaches the other machine as a link, and a link retargeted upstream
# replaces the old one with its target recorded in the backup.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/08-symlinks
e2e_machines a b

e2e_write a .config/zsh/zshrc $'setopt autocd\n'
e2e_write a .config/zsh/zshrc.work $'setopt nobeep\n'
ln -s .config/zsh/zshrc "$(e2e_home a)/.zshrc"
e2e_init a alpha
e2e_dm_rc 0 a add .config/zsh .zshrc
e2e_dm_rc 0 a sync
e2e_check "the remote stores .zshrc as a link" test \
    "$(e2e_tool git --git-dir="$(e2e_remote)" ls-tree main .zshrc | cut -c1-6)" = 120000

e2e_join b beta
e2e_check "b's .zshrc is a link to .config/zsh/zshrc" test \
    "$(readlink "$(e2e_home b)/.zshrc")" = .config/zsh/zshrc
e2e_file_is b .zshrc $'setopt autocd\n'

ln -sfn .config/zsh/zshrc.work "$(e2e_home a)/.zshrc"
e2e_dm_rc 0 a sync
e2e_dm_rc 0 b sync
e2e_check "b's .zshrc now points at zshrc.work" test \
    "$(readlink "$(e2e_home b)/.zshrc")" = .config/zsh/zshrc.work
e2e_file_is b .config/zsh/zshrc $'setopt autocd\n'
e2e_check "b's backup records the old target" \
    grep -rqP '^\.zshrc\t\.config/zsh/zshrc$' "$(e2e_backups b)"

e2e_write b .config/zsh/zshrc $'setopt autocd\nsetopt correct\n'
e2e_dm_rc 0 b sync
e2e_dm_rc 0 a sync
e2e_file_is a .config/zsh/zshrc $'setopt autocd\nsetopt correct\n'
e2e_check "a's .zshrc is still a link" test -L "$(e2e_home a)/.zshrc"
for m in a b; do
    e2e_dm_rc 0 "$m" status
    e2e_err_has '3 managed, 3 clean, 0 to look at'
done

e2e_finish
