#!/usr/bin/env bash
# Backups: each replaced file is backed up with its mode, a mode-only change is recorded, none pruned.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/17-backups
e2e_machines a b

e2e_write a .bashrc $'export V=0\n'
e2e_write a .ssh/config $'Host *\n' 600
e2e_write a .local/bin/tool $'#!/bin/sh\necho 0\n' 755
e2e_init a alpha
e2e_dm_rc 0 a add .bashrc .ssh/config .local/bin/tool
e2e_dm_rc 0 a sync
e2e_join b beta

# Lists b's backup runs.
runs() {
    { find "$(e2e_backups b)" -mindepth 1 -maxdepth 1 -type d 2>/dev/null || true; } | LC_ALL=C sort
}

# Prints the backup run on b that the earlier listing SEEN lacks; names alone do not order runs.
new_run() {
    comm -13 <(printf '%s\n' "$1") <(runs) | sed -n 1p
}

# b never edited these files: each is replaced as it was last applied, which is still backed up.
for v in 1 2 3; do
    e2e_write a .bashrc "export V=$v
"
    e2e_write a .ssh/config "Host *
  Port 2$v
" 600
    e2e_dm_rc 0 a sync
    seen=$(runs)
    e2e_dm_rc 0 b sync
    run=$(new_run "$seen")
    [[ $v != 1 ]] || first=$run
    prev=$((v - 1))
    e2e_check "run $v backs up .bashrc as it was" grep -qx "export V=$prev" "$run/.bashrc"
    e2e_check "run $v backs up .ssh/config" test -f "$run/.ssh/config"
    e2e_check "run $v records .ssh/config's mode" grep -qP '^\.ssh/config\t0600$' "$run/.dotman/modes.txt"
    e2e_check "run $v records .bashrc's mode" grep -qP '^\.bashrc\t0644$' "$run/.dotman/modes.txt"
    e2e_check "the backup copy is private" test "$(stat -c '%a' "$run/.ssh/config")" = 600
done
e2e_check "three backup runs, none pruned" test "$(runs | wc -l)" = 3
e2e_check "the first run still holds V=0" grep -qx 'export V=0' "$first/.bashrc"

# A mode-only change: no copy, only the mode.
chmod 700 "$(e2e_home a)/.local/bin/tool"
e2e_dm_rc 0 a sync
seen=$(runs)
e2e_dm_rc 0 b sync
e2e_mode_is b .local/bin/tool 700
e2e_err_has 'apply .local/bin/tool (mode 0755 to 0700)'
e2e_err_has 'the old mode is recorded in '
run=$(new_run "$seen")
e2e_check "the mode-only run records the old mode" grep -qP '^\.local/bin/tool\t0755$' "$run/.dotman/modes.txt"
e2e_check "the mode-only run copies nothing" test ! -e "$run/.local/bin/tool"
e2e_check "four backup runs, none pruned" test "$(runs | wc -l)" = 4

e2e_dm_rc 0 b doctor
e2e_out_has 'from 4 runs'

e2e_finish
