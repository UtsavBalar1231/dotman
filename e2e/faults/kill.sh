#!/usr/bin/env bash
# Faults: SIGKILL sync, apply, add <dir>, import, init <url> and key init at random points, then recover as printed.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

# E2E_KILL_ITERS kills per operation; raise it for a soak run. E2E_KILL_SEED fixes the edits and each
# kill's point as a fraction of how long the command last took, which varies from machine to machine.
iters=${E2E_KILL_ITERS:-3}
seed=${E2E_KILL_SEED:-20261112}
read -r -a ops <<<"${E2E_KILL_OPS:-sync apply add import init key}"
RANDOM=$seed

e2e_begin faults/kill timeout
printf 'seed %s (E2E_KILL_SEED), %s kills per operation (E2E_KILL_ITERS), operations: %s\n' \
    "$seed" "$iters" "${ops[*]}"

declare -A versions=() latest=() span=()
killed=0
finished=0
known=0
# Every plaintext of the {age} file holds this; no repo object may.
plain_marker=PLAIN-e2e-
key_a=

# Writes TEXT to REL on machine M at MODE and records it as a version some machine wrote.
fk_write() {
    e2e_write "$@"
    fk_version "$2" "$(e2e_home "$1")/$2"
}

# Writes about 2 MB of fresh random text to REL on machine M, so a write of it takes a while.
fk_write_big() {
    local path
    path="$(e2e_home "$1")/$2"
    head -c 1500000 /dev/urandom | base64 >"$path"
    e2e_ledger "$1:~/$2" "$path"
    fk_version "$2" "$path"
}

fk_sum() { sha256sum <"$1" | cut -d' ' -f1; }

fk_version() {
    local sum
    sum=$(fk_sum "$2")
    versions["$1:$sum"]=1
    latest[$1]=$sum
}

# Runs dotman ARGS on machine M under a SIGKILL at a random point of how long the last finished run
# of that command took, or of MAX_MS milliseconds until one finishes; timeout kills the whole
# process group, git children included.
fk_kill() {
    local m=$1 max=${span[$3]:-$2} ms delay began
    shift 2
    ms=$((max * (RANDOM % 1000) / 1000))
    delay=$((ms / 1000)).$(printf '%03d' $((ms % 1000)))
    E2E_LAST_MACHINE=$m
    E2E_LAST_CMD="dotman $* (SIGKILL after ${ms} ms)"
    E2E_RC=0
    e2e_guard "$m"
    began=$(date +%s%N)
    e2e_on "$m" timeout -s KILL "$delay" dotman "$@" </dev/null >"$E2E_ROOT/last.out" 2>"$E2E_ROOT/last.err" || E2E_RC=$?
    [[ $E2E_RC == 137 ]] || span[$1]=$((($(date +%s%N) - began) / 1000000 + 1))
    {
        printf '\n[%s] $ %s  (exit %s)\n' "$m" "$E2E_LAST_CMD" "$E2E_RC"
        cat "$E2E_ROOT/last.out" "$E2E_ROOT/last.err"
    } >>"$E2E_ROOT/log"
    if [[ $E2E_RC == 137 ]]; then
        killed=$((killed + 1))
        printf 'kill  [%s] %s: dotman %s killed after %s ms\n' "$E2E_NAME" "$m" "$1" "$ms"
    else
        finished=$((finished + 1))
        printf 'kill  [%s] %s: dotman %s finished (exit %s) before %s ms\n' "$E2E_NAME" "$m" "$1" "$E2E_RC" "$ms"
    fi
}

# Runs dotman ARGS on machine M with a git on its PATH that kills dotman as soon as a clone returns:
# the window between a clone and its move into place, which a random delay seldom hits.
fk_kill_after_clone() {
    local m=$1 real
    shift
    real=$(command -v git)
    e2e_stub "$m" git "#!/bin/sh
'$real' \"\$@\"
rc=\$?
case \" \$* \" in *' clone '*) kill -KILL \"\$PPID\" ;; esac
exit \$rc
"
    e2e_dm "$m" "$@"
    e2e_rc_is 137 "$m: dotman $1 killed after its clone"
    rm -f -- "$(e2e_bin "$m")/git"
    killed=$((killed + 1))
    printf 'kill  [%s] %s: dotman %s killed after its git clone returned (exit %s)\n' "$E2E_NAME" "$m" "$1" "$E2E_RC"
}

# Makes every dotman temp file or staging dir under machine M look 11 minutes old, as if the user
# came back later: the sweep leaves younger ones alone, since a live run may own them.
fk_age() {
    find "$(e2e_mroot "$1")" \( -name '.dotman-tmp-*' -o -name '.dotman-init-*' -o -name '.dotman-import-*' \) \
        -prune -exec touch -h -d '-11 minutes' {} + 2>/dev/null || true
}

# Invariant: every managed file under machine M's HOME, and every plain one in its repo worktree,
# is whole: its bytes are a version some machine wrote. Missing is allowed mid-run.
fk_whole() {
    local m=$1 rel path bad=0
    for rel in "${!latest[@]}"; do
        for path in "$(e2e_home "$m")/$rel" "$(e2e_repo "$m")/$rel"; do
            [[ -e $path || -L $path ]] || continue
            if [[ ! -f $path || -L $path ]]; then
                bad=1
                e2e_violation "$m: ${path//"$E2E_ROOT"/\$ROOT} is no longer a regular file"
            elif [[ -z ${versions["$rel:$(fk_sum "$path")"]:-} ]]; then
                bad=1
                e2e_violation "$m: ${path//"$E2E_ROOT"/\$ROOT} is torn or mixed: no machine wrote these bytes"
            fi
        done
    done
    [[ $bad == 1 ]] || e2e_pass "$m: every managed file is a version some machine wrote"
}

# Prints the commands the last dotman call printed that undo what a killed run left: removing git's
# lock files, aborting the operation git was in, or applying a repo an init left unapplied.
fk_ways_out() {
    e2e_recipe | grep -E -- '^rm |^dotman git (rebase|merge|cherry-pick|revert|am) --abort$|^dotman apply$' || true
}

# Runs dotman ARGS on machine M until it exits 0 or ACCEPT (a grep -E pattern of its stderr) matches,
# following the ways out it prints, at most three times.
fk_retry() {
    local m=$1 accept=$2 round cmds=() cmd
    shift 2
    for round in 1 2 3; do
        e2e_dm "$m" "$@"
        if [[ $E2E_RC == 0 ]] || { [[ -n $accept ]] && grep -qE -- "$accept" "$E2E_ROOT/last.err"; }; then
            e2e_pass "$m: dotman $* recovers (round $round, exit $E2E_RC)"
            return 0
        fi
        mapfile -t cmds < <(fk_ways_out)
        if [[ ${#cmds[@]} == 0 ]]; then
            e2e_fail "$m: dotman $* exits $E2E_RC after a kill and prints no way out"
            return 0
        fi
        for cmd in "${cmds[@]}"; do
            e2e_run_printed "$m" "$cmd"
            [[ $E2E_PRINTED_RC == 0 ]] || return 0
        done
    done
    e2e_fail "$m: dotman $* recovers within three rounds"
}

# Invariant: no leftover temp file or staging dir is anywhere under machine M once a run swept.
fk_swept() {
    local left
    left=$(find "$(e2e_mroot "$1")" \( -name '.dotman-tmp-*' -o -name '.dotman-init-*' -o -name '.dotman-import-*' \) -print -quit)
    if [[ -z $left ]]; then
        e2e_pass "$1: no temp file or staging dir is left"
    else
        e2e_violation "$1: ${left//"$E2E_ROOT"/\$ROOT} is left after the sweep"
    fi
}

# Syncs the machines in order, then checks that every one is clean and holds the latest version of
# every managed file, as the remote does, and the repo-wide invariants.
fk_converge() {
    local m rel path got
    for m in "$@"; do e2e_dm_rc 0 "$m" sync; done
    local -A seen=()
    for m in "$@"; do
        [[ -n ${seen[$m]:-} ]] && continue
        seen[$m]=1
        e2e_dm_rc 0 "$m" status
        e2e_err_has ', 0 to look at'
        local stale=0
        for rel in "${!latest[@]}"; do
            path="$(e2e_home "$m")/$rel"
            got=$([[ -f $path ]] && fk_sum "$path" || printf 'missing')
            if [[ $got != "${latest[$rel]}" ]]; then
                stale=1
                e2e_violation "$m:~/$rel holds the latest version (got $got)"
            fi
        done
        [[ $stale == 1 ]] || e2e_pass "$m: every managed file holds its latest version"
        fk_swept "$m"
    done
    stale=0
    for rel in "${!latest[@]}"; do
        if [[ $rel == .secret.env ]]; then
            e2e_tool git --git-dir="$(e2e_remote)" show 'main:.secret{age}.env' >"$E2E_ROOT/blob" 2>/dev/null || true
            e2e_tool age -d -i "$key_a" -o "$E2E_ROOT/plain" "$E2E_ROOT/blob" 2>/dev/null || : >"$E2E_ROOT/plain"
            got=$(fk_sum "$E2E_ROOT/plain")
        else
            got=$(e2e_tool git --git-dir="$(e2e_remote)" show "main:$rel" 2>/dev/null | sha256sum | cut -d' ' -f1)
        fi
        if [[ $got != "${latest[$rel]}" ]]; then
            stale=1
            e2e_violation "the remote's $rel is its latest version"
        fi
    done
    rm -f -- "$E2E_ROOT/blob" "$E2E_ROOT/plain"
    [[ $stale == 1 ]] || e2e_pass "the remote holds the latest version of every managed file"
    fk_repo_invariants "${!seen[@]}"
}

# fsck, and no plaintext of the {age} file, on the remote and the repos of the machines named.
# e2e_finish checks every repo's history for committed temp files.
fk_repo_invariants() {
    local m gd dirs=("$(e2e_remote)")
    for m in "$@"; do dirs+=("$(e2e_repo "$m")/.git"); done
    e2e_fsck "${dirs[@]}"
    for gd in "${dirs[@]}"; do e2e_objects_lack "$gd" "$plain_marker"; done
}

# Edits on both machines, to files only that machine edits, so a sync never conflicts.
fk_edit() {
    local n=$1 i
    for i in $((RANDOM % 5 + 1)) $((RANDOM % 5 + 1)); do
        fk_write a ".config/app/a$i.conf" "a$i version $n $RANDOM
"
        fk_write b ".config/app/b$i.conf" "b$i version $n $RANDOM
"
    done
    fk_write b .bashrc "export E2E_ROUND=$n
"
    if ((RANDOM % 2)); then fk_write_big a .big; fi
    if ((RANDOM % 2)); then fk_write a .secret.env "TOKEN=$plain_marker$n-$RANDOM
" 600; fi
}

fk_setup() {
    local i
    e2e_machines a b
    for i in 1 2 3 4 5; do
        fk_write a ".config/app/a$i.conf" "a$i version 0
"
        fk_write a ".config/app/b$i.conf" "b$i version 0
"
    done
    fk_write a .bashrc $'export E2E_ROUND=0\n'
    fk_write_big a .big
    fk_write a .secret.env "TOKEN=${plain_marker}0
" 600
    e2e_mode_cap .secret.env 600
    e2e_init a alpha
    e2e_dm_rc 0 a key init
    key_a="$(e2e_config a)/age-identity.txt"
    e2e_identity_known "$key_a"
    e2e_dm_rc 0 a add .config/app .bashrc .big
    e2e_dm_rc 0 a add --encrypt .secret.env
    e2e_dm_rc 0 a sync
    e2e_join b beta --age-key-file "$key_a"
}

op_sync() {
    local n=$1 m other
    if ((RANDOM % 2)); then m=a other=b; else m=b other=a; fi
    fk_edit "$n"
    if ((RANDOM % 2)); then e2e_dm_rc 0 "$other" sync; fi
    fk_kill "$m" 900 sync
    fk_whole "$m"
    # The retry sees the killed run's temp files while they are young, so it must leave them out
    # of what it commits; once they are old, the next run sweeps them.
    fk_retry "$m" '' sync
    fk_age "$m"
    fk_converge "$other" "$m" "$other"
}

op_apply() {
    local n=$1
    fk_edit "$n"
    fk_write_big a .big
    e2e_dm_rc 0 a sync
    # b's own edits stay in its HOME for the next sync; the pull brings only a's to apply.
    e2e_dm_rc 0 b git pull -q --ff-only
    fk_kill b 200 apply
    fk_whole b
    # The retry sees the killed run's temp files while they are young, so it must leave them out
    # of what it commits; once they are old, the next run sweeps them.
    fk_retry b '' apply
    fk_age b
    fk_converge b a b
}

op_add() {
    local n=$1 i dir=.config/new$1
    for i in 1 2 3 4; do fk_write a "$dir/n$i" "new $n file $i
"; done
    fk_kill a 120 add "$dir"
    fk_whole a
    # The retry sees the killed run's temp files while they are young, so it must leave them out
    # of what it commits; once they are old, the next run sweeps them.
    fk_retry a '' add "$dir"
    fk_age a
    fk_converge a b a
}

# A git work tree to import, built once.
fk_import_source() {
    local work=$E2E_ROOT/src/import i
    [[ -d $work ]] && return 0
    mkdir -p "$work/.config/tool" "$work/.local/bin"
    for i in $(seq 1 40); do printf 'setting %s\n' "$i" >"$work/.config/tool/s$i.conf"; done
    head -c 600000 /dev/urandom | base64 >"$work/.config/tool/large.txt"
    printf '#!/bin/sh\necho run\n' >"$work/.local/bin/run"
    chmod 0755 "$work/.local/bin/run"
    printf 'export SRC=1\n' >"$work/.profile"
    e2e_tool git init -q "$work"
    e2e_tool git -C "$work" symbolic-ref HEAD refs/heads/main
    e2e_tool git -C "$work" add -A
    e2e_tool git -C "$work" commit -q -m source
}

op_import() {
    local m=i$1 src=$E2E_ROOT/src/import before after line missing=0
    fk_import_source
    e2e_machines "$m"
    before=$(e2e_manifest_dir "$src")
    fk_kill "$m" 500 import --no-bootstrap "$src"
    fk_age "$m"
    fk_retry "$m" 'already has files or history' import --no-bootstrap "$src"
    e2e_dm_rc 0 "$m" status
    after=$(e2e_manifest_dir "$src")
    e2e_check "$m: the import source is unchanged" test "$before" = "$after"
    while IFS= read -r line; do
        grep -qxF -- "$line" < <(e2e_git "$m" ls-tree -r HEAD) || {
            missing=1
            e2e_violation "$m: the import holds ${line#*$'\t'} as the source has it"
        }
    done < <(e2e_tool git -C "$src" ls-tree -r HEAD)
    [[ $missing == 1 ]] || e2e_pass "$m: the import holds every source file with its mode"
    fk_swept "$m"
    fk_repo_invariants "$m"
}

# The first kill comes right after the clone returns, the others at random points.
op_init() {
    local n=$1 m=j$1 args moved=''
    e2e_machines "$m"
    args=(init "$(e2e_remote_url)" --host "join$n" --age-key-file "$key_a")
    if [[ $n == 1 ]]; then fk_kill_after_clone "$m" "${args[@]}"; else fk_kill "$m" 700 "${args[@]}"; fi
    # Only a run that said it cloned has moved the clone into place.
    if grep -q '^cloned ' "$E2E_ROOT/last.err"; then moved='a repo already exists'; fi
    fk_whole "$m"
    fk_age "$m"
    fk_retry "$m" "$moved" "${args[@]}"
    if [[ $E2E_RC != 0 && -n $moved ]]; then
        e2e_run_printed "$m" "$(e2e_printed '^dotman apply$')"
    fi
    if [[ -n $moved && ! -f "$(e2e_config "$m")/age-identity.txt" ]]; then
        # FAULT-1: a kill after the clone moved into place loses --age-key-file, and --host too when
        # it came before the machine config; the rerun only names apply. Import the key by hand so
        # the rest is still checked.
        known=$((known + 1))
        printf 'KNOWN [%s] FAULT-1: %s: the retried init dropped --age-key-file\n' "$E2E_NAME" "$m"
        e2e_dm_rc 0 "$m" key import "$key_a"
    fi
    fk_converge "$m" "$m"
}

op_key() {
    local n=$1 m=k$1 identity recipient
    e2e_machines "$m"
    e2e_join "$m" "key$n"
    fk_kill "$m" 70 key init
    fk_age "$m"
    fk_retry "$m" 'an age identity already exists' key init
    identity="$(e2e_config "$m")/age-identity.txt"
    e2e_identity_known "$identity"
    recipient=$(e2e_tool age-keygen -y "$identity" 2>/dev/null) || recipient=
    e2e_check "$m: the identity key init left is whole" test -n "$recipient"
    e2e_check "$m: its recipient is in .dotman.toml" grep -qF -- "${recipient:-no recipient}" "$(e2e_repo "$m")/.dotman.toml"
    # a re-encrypts to the new recipient, and the machine can then decrypt.
    fk_converge "$m" a "$m"
}

fk_setup
for op in "${ops[@]}"; do
    began=$SECONDS
    for ((n = 1; n <= iters; n++)); do
        case $op in
            sync) op_sync "$n" ;;
            apply) op_apply "$n" ;;
            add) op_add "$n" ;;
            import) op_import "$n" ;;
            init) op_init "$n" ;;
            key) op_key "$n" ;;
            *) e2e_die "no operation '$op'; E2E_KILL_OPS takes sync apply add import init key" ;;
        esac
    done
    printf 'time  [%s] %s: %s kills in %ss\n' "$E2E_NAME" "$op" "$iters" $((SECONDS - began))
done
printf 'kills [%s] %s landed mid-run, %s runs finished first, %s hit a known bug (seed %s)\n' \
    "$E2E_NAME" "$killed" "$finished" "$known" "$seed"

e2e_finish
