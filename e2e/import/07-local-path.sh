#!/usr/bin/env bash
# A local-path import leaves no dangling origin/HEAD: fsck passes, doctor fails only on the remote, and a sync to a new remote works.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin import/07-local-path
e2e_machines a b

work=$E2E_ROOT/src/plain
mkdir -p "$work/.config/app"
printf 'set number\n' >"$work/.vimrc"
printf 'theme=dark\n' >"$work/.config/app/app.conf"
e2e_tool git init -q "$work"
e2e_tool git -C "$work" symbolic-ref HEAD refs/heads/main
e2e_tool git -C "$work" add -A
e2e_tool git -C "$work" commit -q -m one
e2e_tool git -C "$work" config remote.origin.url https://example.invalid/dots.git
source_before=$(e2e_manifest_dir "$work")

e2e_dm_rc 0 a import --no-bootstrap "$work"
e2e_err_has 'https://example.invalid/dots.git'
e2e_check "the source is unchanged" test "$source_before" = "$(e2e_manifest_dir "$work")"
e2e_check "the import has no remote" test -z "$(e2e_git a remote)"
e2e_check "no ref under refs/remotes is left" test -z "$(e2e_git a for-each-ref refs/remotes/)"
e2e_check_not "origin/HEAD is gone" e2e_git a symbolic-ref -q refs/remotes/origin/HEAD
e2e_fsck "$(e2e_repo a)/.git"
e2e_dm_rc 1 a doctor
e2e_out_has 'pass  repo objects'
e2e_out_has 'fail  remote: '
e2e_check "doctor fails only on the missing remote" \
    test -z "$(grep '^fail' "$E2E_ROOT/last.out" | grep -v '^fail  remote: ' || true)"

e2e_dm_rc 0 a init --remote "$(e2e_remote_url)"
e2e_dm_rc 0 a apply
e2e_dm_rc 0 a sync
e2e_remote_tracks .config/app/app.conf
e2e_dm_rc 0 a doctor
e2e_check "doctor fails nothing once a remote is set" test -z "$(grep '^fail' "$E2E_ROOT/last.out" || true)"
e2e_join b beta
e2e_file_is b .vimrc $'set number\n'
e2e_fsck_all

e2e_finish
