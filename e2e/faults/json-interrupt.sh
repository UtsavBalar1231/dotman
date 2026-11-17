#!/usr/bin/env bash
# JSON writers interrupted after a stage keep hand edits, safe modes and encrypted objects intact.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin faults/json-interrupt jq setsid
e2e_machines a
e2e_init a alpha
for n in {1..512}; do
    e2e_write a ".config/json/file$n" "base-$n" 0600
done
e2e_write a .private-json 'PRIVATE-json-interrupt-base' 0600
e2e_dm_rc 0 a key init
e2e_dm_rc 0 a add .config/json
e2e_dm_rc 0 a add --encrypt .private-json
e2e_dm_rc 0 a sync
for n in {1..512}; do
    e2e_write a ".config/json/file$n" "hand-edit-$n" 0600
    e2e_write_repo a ".config/json/file$n" "incoming-$n"
done

# The writer must not inherit the FIFO's read end, or closing it cannot raise SIGPIPE.
json_interrupt() {
    e2e_on a bash -s -- "$@" <<'SH'
set -euo pipefail
mode=$1
dir=$(mktemp -d ./json-pipe.XXXXXX)
fifo=$dir/output
mkfifo "$fifo" "$dir/timer"
pid=
watchdog=
cleanup() {
    if [[ -n $pid ]]; then
        kill -KILL -- "-$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    fi
    if [[ -n $watchdog ]]; then
        kill "$watchdog" 2>/dev/null || true
        wait "$watchdog" 2>/dev/null || true
    fi
    rm -rf -- "$dir"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
exec {keep}<>"$fifo"
exec {pipe}<"$fifo"
exec {timer}<>"$dir/timer"
if [[ $mode == apply ]]; then
    setsid dotman apply --json --take-repo {pipe}>&- {keep}>&- >"$fifo" 2>pipe-child.err &
else
    setsid dotman sync --json {pipe}>&- {keep}>&- >"$fifo" 2>kill-child.err &
fi
pid=$!
( exec {pipe}>&- {keep}>&-; read -r -t 60 <&"$timer" || kill -KILL -- "-$pid" 2>/dev/null || true ) &
watchdog=$!
read_record() {
    local char rc
    line=
    while :; do
        if IFS= read -r -n 1 -d '' -t 60 char <&"$pipe"; then
            [[ -n $char ]] || return 2
            [[ $char != $'\n' ]] || break
            line+=$char
        else
            rc=$?
            if [[ $rc == 1 ]]; then
                [[ -z $line ]] && return 1
                return 3
            fi
            return 2
        fi
    done
    [[ -n $line && $(printf '%s' "$line" | jq -Rsj .) == "$line" ]] || return 2
    jq -es 'length == 1 and (.[0] | type == "object" and .record != "result")' <<<"$line" >/dev/null || return 2
}
seen=0
while read_record; do
    if [[ -n ${keep:-} ]]; then exec {keep}>&-; unset keep; fi
    if jq -es --arg stage "$mode" 'length == 1 and .[0].stage == $stage' <<<"$line" >/dev/null; then
        seen=1
        break
    fi
done
[[ $seen == 1 ]]
trap 'jobs -l >>"$dir/jobs"' CHLD
if [[ $mode == apply ]]; then
    exec {pipe}>&-
else
    kill -KILL -- "-$pid"
fi
jobs -l >>"$dir/jobs"
rc=0
wait "$pid" || rc=$?
trap - CHLD
child=$pid
pid=
if [[ $mode == apply ]]; then
    [[ $rc == 141 ]] && grep -Eq "[[:space:]]$child[[:space:]]+Broken pipe([[:space:]]|$)" "$dir/jobs"
else
    [[ $rc == 137 ]]
    grep -Eq "[[:space:]]$child[[:space:]]+Killed([[:space:]]|$)" "$dir/jobs"
    while :; do
        if read_record; then :; else rc=$?; break; fi
    done
    [[ $rc == 1 || $rc == 3 ]]
fi
SH
}

# Enough resolved records after this stage exceed the pipe's capacity.
json_interrupt apply
e2e_pass "JSON apply died by SIGPIPE without a final result"
e2e_dm_rc 0 a apply --take-repo
e2e_write a .private-json 'PRIVATE-json-interrupt-edited' 0600

json_interrupt capture
e2e_pass "JSON sync interrupted without a final result"
e2e_dm_rc 0 a sync
e2e_file_is a .private-json 'PRIVATE-json-interrupt-edited'
e2e_objects_lack "$(e2e_repo a)/.git" PRIVATE-json-interrupt
e2e_remote_never_holds PRIVATE-json-interrupt
e2e_fsck_all
e2e_finish
