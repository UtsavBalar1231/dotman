#!/usr/bin/env bash
# chezmoi, when it is on PATH: modes kept, templates and scripts never run by default, and the result applies.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

if ! command -v chezmoi >/dev/null; then
    e2e_begin import/04-chezmoi
    if [[ ${E2E_REQUIRE_CHEZMOI:-0} == 1 ]]; then
        e2e_fail "chezmoi is on PATH, as E2E_REQUIRE_CHEZMOI=1 asks"
        e2e_finish
    fi
    e2e_skip "chezmoi is not on PATH; E2E_REQUIRE_CHEZMOI=1 makes this a failure"
fi
e2e_begin import/04-chezmoi chezmoi
e2e_machines a b

home=$(e2e_home a)
src=$home/.local/share/chezmoi
marks=$E2E_ROOT/marks
mkdir -p "$src/private_dot_ssh" "$src/dot_local/bin" "$marks"
printf 'export A=1\n' >"$src/dot_bashrc"
printf 'Host x\n' >"$src/private_dot_ssh/private_config"
printf '#!/bin/sh\necho x\n' >"$src/dot_local/bin/executable_x"
printf '{{ output "touch" "%s/template" }}[user]\n\tname = Rendered\n' "$marks" >"$src/dot_gitconfig.tmpl"
printf '#!/bin/sh\ntouch %s/script\n' "$marks" >"$src/run_once_x.sh"
source_before=$(e2e_manifest_dir "$src")

e2e_dm_rc 3 a import --no-bootstrap
e2e_err_has '5 entries: 3 converted, 2 need action'
e2e_err_has 'dot_gitconfig.tmpl'
e2e_err_has 'run_once_x.sh'
e2e_check "no chezmoi template ran" test ! -e "$marks/template"
e2e_check "no chezmoi script ran" test ! -e "$marks/script"
e2e_check "the chezmoi source is unchanged" test "$source_before" = "$(e2e_manifest_dir "$src")"
e2e_check "the repo keeps the exec bit" grep -q '^100755 ' <(e2e_git a ls-tree HEAD .local/bin/x)
e2e_check "[modes] records .ssh/config as 0600" \
    grep -qE '^"?\.ssh/config"? = "0600"' "$(e2e_repo a)/.dotman.toml"

e2e_run_printed a "$(e2e_printed '^dotman apply$')"
e2e_file_is a .bashrc $'export A=1\n'
e2e_mode_is a .ssh/config 600
e2e_mode_is a .ssh 700
e2e_mode_is a .local/bin/x 755
e2e_mode_cap .ssh/config 600
e2e_dm_rc 0 a status
e2e_err_has '3 managed, 3 clean, 0 to look at'

# The converted repo carries the modes to a second machine.
e2e_dm_rc 0 a init --remote "$(e2e_remote_url)"
e2e_dm_rc 0 a sync
e2e_join b beta
e2e_file_is b .bashrc $'export A=1\n'
e2e_mode_is b .ssh/config 600
e2e_mode_is b .local/bin/x 755
e2e_check "no chezmoi template or script ran on either machine" test -z "$(ls -A "$marks")"

e2e_finish
