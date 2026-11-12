#!/usr/bin/env bash
# A bare dotfiles repo with $HOME as its work tree: found without a source, edits made since its last commit kept.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin import/03-bare
e2e_machines a

home=$(e2e_home a)
work=$E2E_ROOT/src/cfg
mkdir -p "$work/.config/git" "$work/.local/bin"
printf 'set number\n' >"$work/.vimrc"
printf '[core]\n\tpager = less\n' >"$work/.config/git/config"
printf '#!/bin/sh\necho tool\n' >"$work/.local/bin/tool"
chmod 0755 "$work/.local/bin/tool"
e2e_tool git init -q "$work"
e2e_tool git -C "$work" symbolic-ref HEAD refs/heads/main
e2e_tool git -C "$work" add -A
e2e_tool git -C "$work" commit -q -m 'dotfiles'
# The `git --git-dir=$HOME/.cfg --work-tree=$HOME` setup, checked out into $HOME.
cfg=$home/.cfg
e2e_tool git clone -q --bare --no-local "$work" "$cfg"
e2e_tool git --git-dir="$cfg" remote remove origin
e2e_tool git --git-dir="$cfg" config core.bare false
e2e_tool git --git-dir="$cfg" config core.worktree "$home"
e2e_tool git --git-dir="$cfg" checkout -q -f main
e2e_mode_is a .local/bin/tool 755
e2e_write a .vimrc $'set number\nset hidden\n'
source_before=$(e2e_manifest_dir "$cfg")

e2e_dm_rc 0 a import --no-bootstrap
e2e_err_has "imported from $cfg"
e2e_err_has '3 entries: 3 converted, 0 need action'
e2e_check "the import keeps the history and adds one commit" test "$(e2e_git a rev-list --count HEAD)" = 2
e2e_check "the bare repo is unchanged" test "$source_before" = "$(e2e_manifest_dir "$cfg")"
e2e_check "the repo has the committed .vimrc" test "$(e2e_git a show HEAD:.vimrc)" = 'set number'
e2e_check "the repo keeps the exec bit" grep -q '^100755 ' <(e2e_git a ls-tree HEAD .local/bin/tool)

e2e_run_printed a "$(e2e_printed '^dotman status$')"
# The import records no applied state, so the edit is a file apply will not decide on alone.
e2e_run_printed a "$(e2e_printed '^dotman apply$')" 3
e2e_dm_rc 3 a apply
e2e_err_has 'existing files left undecided (1)'
e2e_run_printed a "$(e2e_printed '^dotman apply --keep-local$')"
e2e_file_is a .vimrc $'set number\nset hidden\n'
e2e_dm_rc 0 a sync
e2e_check "sync captured the edit made after the bare repo's last commit" \
    test "$(e2e_git a show HEAD:.vimrc)" = $'set number\nset hidden'
e2e_dm_rc 0 a status
e2e_err_has '3 managed, 3 clean, 0 to look at'

e2e_finish
