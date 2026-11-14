#!/usr/bin/env bash
# A pull that conflicts, one file of each kind (plain, {tmpl}, {age}, binary, symlink, an edit against
# a removal in both directions), answered by --keep-local and by --take-repo, ending converged; then
# the one shape dotman does not settle, a file against a directory, whose printed steps run as printed.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/21-conflict-kinds
e2e_machines a b

lines=$'l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\n'

# Prints $lines with each line l<N> replaced by the TEXT of an N=TEXT argument.
edit_lines() {
    local out=$lines spec
    for spec in "$@"; do out=${out/"l${spec%%=*}"$'\n'/"${spec#*=}"$'\n'}; done
    printf '%s\n' "${out%$'\n'}"
}

# Writes a binary file on machine M: REL holds a NUL byte and TEXT.
binary_write() {
    local path
    path="$(e2e_home "$1")/$2"
    printf 'bin\0%s' "$3" >"$path"
    e2e_ledger "$1:~/$2" "$path"
}

# Asserts that REL under machine M's HOME is the binary file binary_write wrote with TEXT.
binary_is() {
    local want got
    want=$(printf 'bin\0%s' "$3" | sha256sum)
    got=$(sha256sum <"$(e2e_home "$1")/$2" 2>/dev/null || true)
    if [[ $want == "$got" ]]; then e2e_pass "$1:~/$2 is the binary file '$3'"; else e2e_fail "$1:~/$2 is the binary file '$3'"; fi
}

# Asserts that REL under machine M's HOME is a symlink to TARGET.
link_is() {
    if [[ $(readlink "$(e2e_home "$1")/$2" 2>/dev/null || true) == "$3" ]]; then
        e2e_pass "$1:~/$2 is a link to $3"
    else
        e2e_fail "$1:~/$2 is a link to $3"
    fi
}

# Asserts that machine M saved its own version of the repo path KEY in a backup.
saved_is() {
    e2e_check "$1: this machine's $2 is saved" test -n "$(find "$(e2e_backups "$1")" -path "*/.dotman/repo/$2" -print -quit)"
}

# Whether machine M's list names KEY; e2e_check runs it.
# shellcheck disable=SC2329
managed() {
    e2e_dm "$1" list
    grep -qF -- "$2" <<<"$E2E_OUT"
}

# Asserts that machine M manages KEY, or does not.
manages() { e2e_check "$1 manages $2" managed "$1" "$2"; }
manages_not() { e2e_check_not "$1 does not manage $2" managed "$1" "$2"; }

# Asserts that machine A's and machine B's HOME hold the same REL.
same_file() {
    e2e_check "a and b hold the same $1" cmp -s "$(e2e_home a)/$1" "$(e2e_home b)/$1"
}

e2e_write a .base $'base\n'
e2e_write a .mix "$lines"
e2e_init a alpha
e2e_dm_rc 0 a key init
key="$(e2e_config a)/age-identity.txt"
e2e_identity_known "$key"
for r in k t; do
    e2e_write a ".plain-$r" "$lines"
    e2e_write a ".age-$r" $'secret\n' 600
    e2e_mode_cap ".age-$r" 600
    binary_write a ".bin-$r" base
    ln -s .base "$(e2e_home a)/.link-$r"
    e2e_write a ".gone-b-$r" $'gone b\n'
    e2e_write a ".gone-a-$r" $'gone a\n'
    e2e_write a ".tpl-$r" "$lines"
    e2e_dm_rc 0 a add .base .mix ".plain-$r" ".bin-$r" ".link-$r" ".gone-b-$r" ".gone-a-$r"
    e2e_dm_rc 0 a add --template ".tpl-$r"
    e2e_dm_rc 0 a add --encrypt ".age-$r"
done
e2e_dm_rc 0 a sync
e2e_join b beta --age-key-file "$key"

# One pull holding a conflict of every kind. a edits and syncs first; b edits the same files and
# answers with FLAG, which keeps b's side of each clash or takes a's.
conflict_round() {
    local flag=$1 r=$2 text key who clash plain src link
    local a_clash='A2 {{ 1 }}' b_clash='B2 {{ 2 }}' b_tail='B8 {{ 3 }}'
    e2e_write a ".plain-$r" "$(edit_lines 2=A2 5=A5)"$'\n'
    e2e_write_repo a ".tpl-$r{tmpl}" "$(edit_lines "2=$a_clash" 5=A5)"$'\n'
    e2e_write a ".age-$r" "A-$r"$'\n' 600
    binary_write a ".bin-$r" "A-$r"
    ln -sfn ".plain-$r" "$(e2e_home a)/.link-$r"
    e2e_write a ".gone-a-$r" "edited on a $r"$'\n'
    e2e_dm_rc 0 a forget ".gone-b-$r"
    e2e_dm_rc 0 a sync

    e2e_write b ".plain-$r" "$(edit_lines 2=B2 8=B8)"$'\n'
    e2e_write_repo b ".tpl-$r{tmpl}" "$(edit_lines "2=$b_clash" "8=$b_tail")"$'\n'
    e2e_write b ".age-$r" "B-$r"$'\n' 600
    binary_write b ".bin-$r" "B-$r"
    ln -sfn ".bin-$r" "$(e2e_home b)/.link-$r"
    e2e_write b ".gone-b-$r" "edited on b $r"$'\n'
    e2e_dm_rc 0 b forget ".gone-a-$r"
    e2e_dm_rc 4 b sync
    e2e_err_has 'the pull conflicts with your changes'
    e2e_err_has 'conflicting paths:'
    for key in ".plain-$r" ".tpl-$r{tmpl}" ".age-$r{age}" ".bin-$r" ".link-$r" ".gone-b-$r" ".gone-a-$r"; do
        e2e_err_has "  $key"
    done
    e2e_file_has b ".plain-$r" B2
    mapfile -t answers < <(e2e_recipe)
    e2e_check "the report prints the two flag commands and dotman sync" test "${#answers[@]}" = 3
    e2e_run_printed b "$(e2e_printed "^dotman sync $flag\$")"
    for key in rebase-merge rebase-apply dotman-pull; do
        e2e_check "no $key is left" test ! -e "$(e2e_repo b)/.git/$key"
    done

    # Each clash takes one side; what is edited elsewhere in the file is kept from both.
    if [[ $flag == --keep-local ]]; then
        who=B clash=$b_clash plain=B2 link=.bin-$r
    else
        who=A clash=$a_clash plain=A2 link=.plain-$r
    fi
    e2e_file_is b ".plain-$r" "$(edit_lines "2=$plain" 5=A5 8=B8)"$'\n'
    src=$(edit_lines "2=$clash" 5=A5 "8=$b_tail")
    e2e_check "the remote holds the merged template source" test "$(e2e_remote_show ".tpl-$r{tmpl}")" = "$src"
    e2e_file_is b ".tpl-$r" "$(sed -E 's/\{\{ ([0-9]) \}\}/\1/g' <<<"$src")"$'\n'
    e2e_file_is b ".age-$r" "$who-$r"$'\n'
    e2e_mode_is b ".age-$r" 600
    binary_is b ".bin-$r" "$who-$r"
    link_is b ".link-$r" "$link"
    saved_is b ".plain-$r"
    saved_is b ".tpl-$r{tmpl}"

    # An edit against a removal: keep-local keeps b's edit managed and a's removal of the other
    # file in force; take-repo accepts a's removal and brings a's edit of the other back.
    e2e_file_is b ".gone-b-$r" "edited on b $r"$'\n'
    if [[ $flag == --keep-local ]]; then
        manages b ".gone-b-$r"
        manages_not b ".gone-a-$r"
        e2e_file_is b ".gone-a-$r" $'gone a\n'
    else
        manages_not b ".gone-b-$r"
        manages b ".gone-a-$r"
        e2e_file_is b ".gone-a-$r" "edited on a $r"$'\n'
        saved_is b ".age-$r{age}"
        saved_is b ".bin-$r"
        saved_is b ".link-$r"
    fi

    # a pulls the answer. Under keep-local the file b kept managed meets a's own copy of it, which
    # a settles with the flag it is given.
    if [[ $flag == --keep-local ]]; then
        e2e_dm_rc 3 a sync
        e2e_err_has ".gone-b-$r"
        e2e_run_printed a "$(e2e_printed '^dotman apply --take-repo$')"
        e2e_dm_rc 0 a sync
        manages a ".gone-b-$r"
        e2e_file_is a ".gone-b-$r" "edited on b $r"$'\n'
        manages_not a ".gone-a-$r"
        e2e_file_is a ".gone-a-$r" "edited on a $r"$'\n'
    else
        e2e_dm_rc 0 a sync
        manages_not a ".gone-b-$r"
        e2e_file_is a ".gone-b-$r" $'gone b\n'
        manages a ".gone-a-$r"
    fi
    for key in .plain-$r .tpl-$r .age-$r .bin-$r; do same_file "$key"; done
    link_is a ".link-$r" "$link"
    for key in a b; do
        e2e_dm_rc 0 "$key" status
        e2e_err_has ', 0 to look at'
    done
    e2e_dm_rc 0 b sync
    e2e_dm_rc 0 a sync
    e2e_check "both repos are at the remote's main" test \
        "$(e2e_git a rev-parse HEAD)" = "$(e2e_git b rev-parse HEAD)"
}

conflict_round --keep-local k
conflict_round --take-repo t

# A file against a directory is the one shape dotman does not settle. Even with a flag, and beside a
# conflict it does settle, sync exits 4 and changes nothing; the steps it prints run as printed, with
# GIT_EDITOR=false exported, and end in a clean sync.
e2e_write a .shape/y $'y\n'
e2e_write a .mix "$(edit_lines 2=A2)"$'\n'
e2e_dm_rc 0 a add .shape
e2e_dm_rc 0 a sync
e2e_write b .shape $'a file\n'
e2e_write b .mix "$(edit_lines 2=B2)"$'\n'
e2e_dm_rc 0 b add .shape
e2e_dm_rc 4 b sync --keep-local
e2e_err_has 'conflicting paths:'
e2e_err_has '.shape is a file here and a directory in origin/main, which dotman cannot settle'
e2e_err_lacks 'dotman sync --keep-local'
repo=$(e2e_repo b)
before=$(
    e2e_manifest_dir "$repo/.git/refs"
    e2e_manifest_dir "$(e2e_home b)"
)
e2e_dm_rc 4 b sync --take-repo
after=$(
    e2e_manifest_dir "$repo/.git/refs"
    e2e_manifest_dir "$(e2e_home b)"
)
e2e_check "an unmodelled stop changes nothing on b" test "$before" = "$after"
mapfile -t steps < <(e2e_recipe)
e2e_check "the report prints five commands" test "${#steps[@]}" = 5
E2E_PRINTED_ENV=(GIT_EDITOR=false)
e2e_run_printed b "${steps[0]}" 1
# The user's step 2: .mix with both edits, and the directory over git's copy of the file.
e2e_write_repo b .mix "$(edit_lines '2=A2 B2')"$'\n'
rm -f -- "$repo"/.shape~*
rm -f -- "$(e2e_home b)/.shape"
for i in 1 2 3 4; do e2e_run_printed b "${steps[i]}"; done
E2E_PRINTED_ENV=()
e2e_check "no rebase is left" test ! -e "$repo/.git/rebase-merge" -a ! -e "$repo/.git/rebase-apply"
e2e_dm_rc 0 a sync
for m in a b; do
    e2e_file_is "$m" .shape/y $'y\n'
    e2e_file_is "$m" .mix "$(edit_lines '2=A2 B2')"$'\n'
    e2e_dm_rc 0 "$m" status
    e2e_err_has ', 0 to look at'
done

e2e_finish
