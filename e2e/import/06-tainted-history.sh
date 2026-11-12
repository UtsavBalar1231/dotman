#!/usr/bin/env bash
# A source whose history holds a token: squashed to one commit, synced to a new remote with no secret; --keep-history keeps it.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin import/06-tainted-history
e2e_machines a b c

token=$(e2e_token)
work=$E2E_ROOT/src/history
mkdir -p "$work"
printf 'alias ll=ls\n' >"$work/.bashrc"
printf 'export GH=%s\n' "$token" >"$work/.oldrc"
printf 'export TOK=%s\n' "$token" >"$work/.tokrc"
e2e_tool git init -q "$work"
e2e_tool git -C "$work" symbolic-ref HEAD refs/heads/main
e2e_tool git -C "$work" add -A
e2e_tool git -C "$work" commit -q -m one
# .oldrc leaves the tree in the second commit, so only the history holds it; .tokrc stays.
rm -- "$work/.oldrc"
printf 'alias ll=ls\nalias la=ls\n' >"$work/.bashrc"
e2e_tool git -C "$work" add -A
e2e_tool git -C "$work" commit -q -m two
src=$E2E_ROOT/src/history.git
e2e_tool git clone -q --bare --no-local "$work" "$src"
url="file://$src"
first=$(e2e_tool git --git-dir="$src" rev-parse --short=12 main~1)

e2e_dm_rc 3 a import --dry-run "$url"
e2e_err_has 'would import the current tree of 2 commits as one commit'
e2e_err_lacks "$token"
e2e_dm_rc 3 a import --yes --no-bootstrap "$url"
e2e_err_has "  .oldrc: github-token, added in $first"
e2e_err_has '  .tokrc: github-token, added in '
e2e_err_has 'so the import is one commit of the current tree'
e2e_err_lacks "$token"
e2e_check "the import is one commit" test "$(e2e_git a rev-list --count HEAD)" = 1
e2e_objects_lack "$(e2e_repo a)/.git" "$token"
e2e_check "the squashed repo has no remote" test -z "$(e2e_git a remote)"
apply=$(e2e_printed '^dotman apply$')

e2e_dm_rc 0 a init --remote "$(e2e_remote_url)"
e2e_run_printed a "$apply"
e2e_dm_rc 0 a sync
e2e_remote_tracks .bashrc
e2e_remote_lacks .tokrc
e2e_remote_never_holds "$token"
e2e_objects_lack "$(e2e_remote)" "$token"
e2e_join b beta
e2e_file_is b .bashrc $'alias ll=ls\nalias la=ls\n'
e2e_objects_lack "$(e2e_repo b)/.git" "$token"

# --keep-history imports every commit, and says what that keeps.
e2e_dm_rc 3 c import --yes --no-bootstrap --keep-history "$url"
e2e_err_has '--keep-history imports all 2 commits'
e2e_err_lacks "$token"
e2e_check "the import keeps both commits and adds one" test "$(e2e_git c rev-list --count HEAD)" = 3
e2e_check "the kept history holds the token" \
    grep -qF -- "$token" <(e2e_git c show "$first:.oldrc")

e2e_fsck_all
e2e_finish
