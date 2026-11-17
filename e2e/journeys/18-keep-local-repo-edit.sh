#!/usr/bin/env bash
# Keep-local and forget save an uncommitted repo edit under .dotman/repo/ in the backups first.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/18-keep-local-repo-edit jq
e2e_machines a

e2e_write a .bashrc $'export A=1\n'
e2e_write a .vimrc $'set number\n'
e2e_init a alpha
e2e_dm_rc 0 a add .bashrc .vimrc
e2e_dm_rc 0 a sync

e2e_write_repo a .bashrc $'export A=repo\n'
e2e_write a .bashrc $'export A=home\n'
e2e_dm_rc 3 a apply
e2e_err_has '.bashrc'
e2e_run_printed a "$(e2e_printed 'apply --keep-local')"
e2e_file_is a .bashrc $'export A=home\n'
saved=$(find "$(e2e_backups a)" -path '*/.dotman/repo/.bashrc' -print)
e2e_check "the repo edit is saved under .dotman/repo/" test -n "$saved"
e2e_check "the saved copy holds the repo edit" grep -qx 'export A=repo' "${saved:-/dev/null}"
e2e_dm_rc 0 a sync
e2e_check "the remote has $HOME's version" test "$(e2e_remote_show .bashrc)" = 'export A=home'

e2e_write_repo a .vimrc $'set relativenumber\n'
e2e_dm_rc 0 a forget .vimrc
saved=$(find "$(e2e_backups a)" -path '*/.dotman/repo/.vimrc' -print)
e2e_check "forget saves the repo edit it removes" grep -qx 'set relativenumber' "${saved:-/dev/null}"
e2e_file_is a .vimrc $'set number\n'
e2e_dm_rc 0 a sync
e2e_remote_lacks .vimrc

e2e_write a .second $'second-base\n'
e2e_dm_rc 0 a add .second
e2e_dm_rc 0 a sync
e2e_write a .bashrc $'reviewed-bashrc\n'
e2e_write a .second $'reviewed-second\n'
review="$E2E_ROOT/outgoing-decisions.json"
e2e_review a sync "$review"
e2e_write a .second $'later-second\n'
e2e_write_repo a .unlisted $'unreviewed-index\n'
e2e_git a add -- .unlisted
index_before=$(e2e_git a ls-files -s -- .unlisted)
e2e_decided 3 a sync "$review"
e2e_check "the matching reviewed capture was pushed" test "$(e2e_remote_show .bashrc)" = reviewed-bashrc
e2e_check "the stale home capture was not pushed" test "$(e2e_remote_show .second)" = second-base
e2e_remote_lacks .unlisted
e2e_check "the unreviewed staged entry is unchanged" test "$(e2e_git a ls-files -s -- .unlisted)" = "$index_before"
e2e_file_is a .second $'later-second\n'

e2e_review a sync "$review"
e2e_git a -c core.hooksPath=/dev/null commit --allow-empty -qm unreviewed-head
remote_before=$(e2e_tool git --git-dir="$(e2e_remote)" rev-parse main)
e2e_decided 3 a sync "$review"
e2e_check "a moved reviewed HEAD pushed nothing" test "$(e2e_tool git --git-dir="$(e2e_remote)" rev-parse main)" = "$remote_before"

e2e_dm_rc 0 a sync
e2e_dm_rc 0 a key init
e2e_identity_known "$(e2e_config a)/age-identity.txt"
e2e_write a .one $'review-private-one\n' 600
e2e_write a .two $'review-private-two\n' 600
e2e_mode_cap .one 600
e2e_mode_cap .two 600
e2e_dm_rc 0 a add --encrypt .one .two
e2e_dm_rc 0 a sync
recipient=$(e2e_on a age-keygen -y "$(e2e_config a)/age-identity.txt")

for dirty in plain age config record; do
    receiver="receiver-$dirty"
    e2e_machine "$receiver"
    e2e_join "$receiver" "$receiver"
    e2e_dm_rc 0 "$receiver" key init
    e2e_identity_known "$(e2e_config "$receiver")/age-identity.txt"
    e2e_review a sync "$review"
    e2e_dm_rc 0 "$receiver" sync
    case $dirty in
        plain)
            e2e_write_repo a .unlisted-age $'unreviewed-reencrypt-neighbor\n'
            e2e_git a add -- .unlisted-age
            index_before=$(e2e_git a ls-files -s -- .unlisted-age)
            ;;
        age) cp -- "$(e2e_repo a)/.two{age}" "$(e2e_repo a)/.one{age}" ;;
        config) sed -i "\|$recipient|d" "$(e2e_repo a)/.dotman.toml" ;;
        record) printf 'unreviewed-recipient-record\n' >"$(e2e_repo a)/.dotman/age-recipients" ;;
    esac
    before=$(sha256sum "$(e2e_repo a)/.one{age}" "$(e2e_repo a)/.two{age}" \
        "$(e2e_repo a)/.dotman/age-recipients")
    e2e_decided '3|4' a sync "$review"
    if [[ $dirty == plain ]]; then
        changed=$(e2e_tool git --git-dir="$(e2e_remote)" diff-tree --no-commit-id --name-only -r main)
        e2e_check "re-encryption commits only its two sources and record" test "$changed" = $'.dotman/age-recipients\n.one{age}\n.two{age}'
        e2e_check "re-encryption preserves the unlisted staged entry" test "$(e2e_git a ls-files -s -- .unlisted-age)" = "$index_before"
        e2e_remote_never_holds unreviewed-reencrypt-neighbor
        mv -- "$(e2e_repo a)/.unlisted-age" "$(e2e_mroot a)/unreviewed-reencrypt-neighbor"
        e2e_git a reset -q HEAD -- .unlisted-age
    else
        after=$(sha256sum "$(e2e_repo a)/.one{age}" "$(e2e_repo a)/.two{age}" \
            "$(e2e_repo a)/.dotman/age-recipients")
        e2e_check "$dirty: the refused re-encryption replaces no source or record" test "$after" = "$before"
        e2e_check "$dirty: the remote retains only the receiver's reviewed settings" test \
            "$(e2e_tool git --git-dir="$(e2e_remote)" rev-parse main:.dotman.toml)" = \
            "$(e2e_git "$receiver" rev-parse HEAD:.dotman.toml)"
        e2e_git a checkout HEAD -- .dotman.toml '.one{age}' .dotman/age-recipients
        e2e_dm_rc 0 a sync
    fi
    e2e_remote_never_holds review-private-one
    e2e_remote_never_holds review-private-two
done

e2e_finish
