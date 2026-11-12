#!/usr/bin/env bash
# Legacy store: tracked dirs come back as printed add commands, [dirs] stays empty, and an overlay restores modes.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

fixture="$(cd "$(dirname "$0")/../.." && pwd)/tests/fixtures/legacy-v2/repo"

# Prints N as BYTES little-endian bytes.
le() {
    local i
    for ((i = 0; i < $2; i++)); do
        # shellcheck disable=SC2059  # the format is the escape for one byte
        printf "\\x$(printf '%02x' $((($1 >> (8 * i)) & 255)))"
    done
}

# tracking.bin as the old library wrote it: bincode's legacy encoding of { version: u32,
# tracked_directories: HashSet<PathBuf>, tracked_files: HashSet<PathBuf> }.
tracking_bin() {
    local dir
    le 1 4
    le $# 8
    for dir in "$@"; do
        le ${#dir} 8
        printf '%s' "$dir"
    done
    le 0 8
}

e2e_begin import/01-legacy
e2e_machines a b

legacy=$E2E_ROOT/src/legacy
mkdir -p "$E2E_ROOT/src"
cp -a -- "$fixture" "$legacy"
chmod -R u+w "$legacy"
tracking_bin .local/bin /home/legacyuser/.config /srv/elsewhere >"$legacy/tracking.bin"
before=$(e2e_manifest_dir "$legacy")

e2e_dm_rc 0 a import --no-bootstrap "$legacy"
e2e_err_has 'the old store tracked 2 directories: .config, .local/bin; [dirs] is left empty'
e2e_err_has 'tracked directories that cannot be added: /srv/elsewhere'
e2e_check "[dirs] is empty after the import" \
    grep -qxF 'paths = []' <(grep -A1 -xF '[dirs]' "$(e2e_repo a)/.dotman.toml")
e2e_check "the source is unchanged" test "$before" = "$(e2e_manifest_dir "$legacy")"
e2e_check "the import is the legacy history plus one commit" \
    test "$(e2e_git a rev-list --count HEAD)" -ge 4
# Remember the adds before apply prints over them.
mapfile -t adds < <(e2e_recipe | grep -E '^dotman add ' || true)
e2e_check "two dotman add commands are printed" test "${#adds[@]}" = 2

e2e_run_printed a "$(e2e_printed '^dotman status$')"
e2e_run_printed a "$(e2e_printed '^dotman apply$')"
e2e_file_is a .bashrc $'# bashrc v2\nexport EDITOR=vim\n'
e2e_file_is a .config/side.toml $'side = 1\n'
e2e_mode_is a .ssh/config 600
e2e_mode_is a .local/bin/run 755
e2e_write a .config/tool/state.json $'{}\n'
e2e_dm_rc 0 a status
e2e_err_has '6 managed, 6 clean, 0 to look at'
for cmd in "${adds[@]}"; do e2e_run_printed a "$cmd"; done
e2e_check "[dirs] holds what the printed adds named" \
    grep -qF '".local/bin"' "$(e2e_repo a)/.dotman.toml"
e2e_dm_rc 0 a status
e2e_err_has '7 managed, 7 clean, 0 to look at'

# An overlay onto a repo whose copies lost their modes records them, and apply restores them.
src=$E2E_ROOT/src/overlay
mkdir -p "$src/.ssh" "$src/.local/bin" "$src/.config"
printf '# bashrc v2\nexport EDITOR=vim\n' >"$src/.bashrc"
printf '#!/bin/sh\necho run\n' >"$src/.local/bin/run"
printf 'Host example\n  User legacyuser\n' >"$src/.ssh/config"
: >"$src/.empty"
printf 'abs=true\n' >"$src/.config/abs.conf"
chmod 0644 "$src/.local/bin/run" "$src/.ssh/config"
e2e_tool git init -q "$src"
e2e_tool git -C "$src" symbolic-ref HEAD refs/heads/main
e2e_tool git -C "$src" add -A
e2e_tool git -C "$src" commit -q -m base
e2e_dm_rc 0 b init "$src" --host beta --take-repo
e2e_mode_is b .ssh/config 644
e2e_dm_rc 3 b import --overlay "$legacy"
e2e_err_has '.config/side.toml: not in the repo'
e2e_err_has 'an overlay leaves [dirs] as it is'
e2e_check "the overlay records .ssh/config as 0600" \
    grep -qE '^"?\.ssh/config"? = "0600"' "$(e2e_repo b)/.dotman.toml"
e2e_run_printed b "$(e2e_printed '^dotman apply$')"
e2e_mode_is b .ssh/config 600
e2e_mode_is b .local/bin/run 755
e2e_check "the source is still unchanged" test "$before" = "$(e2e_manifest_dir "$legacy")"

e2e_finish
