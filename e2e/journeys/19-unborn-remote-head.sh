#!/usr/bin/env bash
# A remote whose HEAD names a branch with no commits: init <url> checks out the branch it has, or refuses when none is main.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/19-unborn-remote-head
e2e_machines a b c

e2e_write a .bashrc $'export A=1\n'
e2e_init a alpha
e2e_dm_rc 0 a add .bashrc
e2e_dm_rc 0 a sync
# A remote made with git's default branch: its HEAD names master, and main is where the commits are.
e2e_tool git --git-dir="$(e2e_remote)" symbolic-ref HEAD refs/heads/master

e2e_dm_rc 0 b init "$(e2e_remote_url)" --dry-run
e2e_err_has 'would check out main, the only branch it has'
e2e_absent b .bashrc
e2e_check_not "the dry run created a repo" test -e "$(e2e_repo b)"

e2e_join b beta
e2e_err_has 'checked out main, the only branch it has'
e2e_file_is b .bashrc $'export A=1\n'
e2e_dm_rc 0 b status
e2e_err_has '1 managed, 1 clean, 0 to look at'

e2e_write b .vimrc $'set number\n'
e2e_dm_rc 0 b add .vimrc
e2e_dm_rc 0 b sync
e2e_remote_tracks .vimrc
e2e_check "the remote still has only main" test \
    "$(e2e_tool git --git-dir="$(e2e_remote)" for-each-ref --format='%(refname)')" = refs/heads/main
e2e_dm_rc 0 a sync
e2e_file_is a .vimrc $'set number\n'

# With several branches and none of them main, nothing says which holds the dotfiles.
e2e_tool git --git-dir="$(e2e_remote)" branch -m main dots
e2e_tool git --git-dir="$(e2e_remote)" branch other dots
e2e_dm_rc 1 c init "$(e2e_remote_url)"
e2e_err_has 'none of its 2 branches is main (dots, other)'
e2e_err_has 'Nothing was cloned'
e2e_check_not "the refusal created a repo" test -e "$(e2e_repo c)"
e2e_absent c .bashrc

e2e_fsck_all
e2e_finish
