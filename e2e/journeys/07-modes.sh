#!/usr/bin/env bash
# Modes: 0600 stays 0600, a stricter local mode is kept here only (M5), [modes] applies everywhere.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/07-modes
e2e_machines a b

e2e_write a .ssh/config $'Host *\n  ServerAliveInterval 30\n' 600
e2e_write a .profile $'export PATH=$HOME/bin:$PATH\n' 644
e2e_write a .local/bin/hello $'#!/bin/sh\necho hello\n' 755
e2e_mode_cap .ssh/config 600
e2e_init a alpha
e2e_dm_rc 0 a add .ssh/config .profile .local/bin/hello
e2e_dm_rc 0 a sync
e2e_check "add records the private mode in [modes]" \
    grep -qF '".ssh/config" = "0600"' "$(e2e_repo a)/.dotman.toml"

# b already has a stricter .profile of its own; taking the repo's content keeps b's mode.
e2e_write b .profile $'export PATH=/opt/bin:$PATH\n' 600
e2e_mode_cap .profile 600 b
e2e_dm_rc 3 b init "$(e2e_remote_url)" --host beta
e2e_mode_is b .ssh/config 600
e2e_mode_is b .local/bin/hello 755
e2e_run_printed b "$(e2e_printed 'apply --take-repo')"
e2e_file_is b .profile $'export PATH=$HOME/bin:$PATH\n'
e2e_mode_is b .profile 600
e2e_dm_rc 0 b status
e2e_err_has 'kept at 0600 here; the repo says 0644'
e2e_err_has '3 managed, 3 clean'
e2e_dm_rc 0 b sync
e2e_check_not "the kept mode is not committed" grep -qF '".profile"' "$(e2e_repo b)/.dotman.toml"

e2e_write a .ssh/config $'Host *\n  ServerAliveInterval 60\n' 600
e2e_write a .profile $'export PATH=$HOME/.local/bin:$PATH\n' 644
e2e_dm_rc 0 a sync
e2e_dm_rc 0 b sync
e2e_file_has b .ssh/config 'ServerAliveInterval 60'
e2e_mode_is b .ssh/config 600
e2e_file_is b .profile $'export PATH=$HOME/.local/bin:$PATH\n'
e2e_mode_is b .profile 600
e2e_mode_is a .profile 644

# An explicit [modes] entry is meant for every machine, so it applies over b's kept mode.
e2e_write_repo a .dotman.toml "$(sed 's|^\[modes\]$|[modes]\n".profile" = "0640"|' "$(e2e_repo a)/.dotman.toml")
"
e2e_inv_modes
e2e_mode_uncap .profile
e2e_dm_rc 0 a sync
e2e_mode_is a .profile 640
e2e_dm_rc 0 b sync
e2e_mode_is b .profile 640
e2e_dm_rc 0 b status
e2e_err_lacks 'kept at'

e2e_finish
