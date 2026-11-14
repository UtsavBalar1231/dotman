#!/usr/bin/env bash
# Settings changed on both machines between syncs merge with no question: a private file each
# ([modes]), a directory each ([dirs]) and a key each ([age]), and a 0600 file stays 0600 on both.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/20-settings-merge
e2e_machines a b

e2e_write a .bashrc $'export EDITOR=vi\n'
e2e_init a alpha
e2e_dm_rc 0 a add .bashrc
e2e_dm_rc 0 a sync
e2e_join b beta

# [modes]: each machine adds a 0600 file, so each .dotman.toml gains a different entry.
e2e_write a .ssh/from-a $'a key\n' 600
e2e_write b .ssh/from-b $'b key\n' 600
e2e_mode_cap .ssh/from-a 600
e2e_mode_cap .ssh/from-b 600
e2e_dm_rc 0 a add .ssh/from-a
e2e_dm_rc 0 b add .ssh/from-b
e2e_dm_rc 0 a sync
e2e_dm_rc 0 b sync
e2e_err_has 'merged .dotman.toml: 1 setting from origin/main'
e2e_err_lacks 'the pull conflicts'
e2e_dm_rc 0 a sync
for m in a b; do
    for key in from-a from-b; do
        e2e_check "$m: .dotman.toml holds .ssh/$key as 0600" \
            grep -qF "\".ssh/$key\" = \"0600\"" "$(e2e_repo "$m")/.dotman.toml"
        e2e_mode_is "$m" ".ssh/$key" 600
    done
done
e2e_file_is a .ssh/from-b $'b key\n'
e2e_file_is b .ssh/from-a $'a key\n'

# [dirs]: each machine adds a directory. The one a machine added is approved there; the other
# one's new files wait for approval, as they do without a merge.
e2e_write a .config/da/one $'one\n'
e2e_write b .config/db/two $'two\n'
e2e_dm_rc 0 a add .config/da
e2e_dm_rc 0 b add .config/db
e2e_dm_rc 0 a sync
e2e_dm_rc 0 b sync
e2e_err_has 'merged .dotman.toml'
e2e_err_lacks 'the pull conflicts'
e2e_dm_rc 0 a sync
for m in a b; do
    for dir in da db; do
        e2e_check "$m: [dirs] lists .config/$dir" grep -qF "\".config/$dir\"" "$(e2e_repo "$m")/.dotman.toml"
    done
done
e2e_file_is a .config/db/two $'two\n'
e2e_file_is b .config/da/one $'one\n'

# [age]: each machine runs key init, so each adds a different recipient.
e2e_dm_rc 0 a key init
e2e_identity_known "$(e2e_config a)/age-identity.txt"
e2e_dm_rc 0 a sync
e2e_dm_rc 0 b key init
e2e_identity_known "$(e2e_config b)/age-identity.txt"
e2e_dm_rc 0 b sync
e2e_err_has 'merged .dotman.toml: 1 setting from origin/main'
e2e_err_lacks 'the pull conflicts'
e2e_dm_rc 0 a sync
for m in a b; do
    for key in a b; do
        recipient=$(e2e_tool age-keygen -y "$(e2e_config "$key")/age-identity.txt")
        e2e_check "$m: [age] lists the recipient of $key" grep -qF -- "$recipient" "$(e2e_repo "$m")/.dotman.toml"
    done
done

# Each machine holds the same settings, and a file encrypted for both recipients opens on both.
e2e_check "both machines hold the same .dotman.toml" \
    cmp -s "$(e2e_repo a)/.dotman.toml" "$(e2e_repo b)/.dotman.toml"
e2e_write a .secret.env $'TOKEN=one\n' 600
e2e_mode_cap .secret.env 600
e2e_dm_rc 0 a add --encrypt .secret.env
e2e_dm_rc 0 a sync
e2e_dm_rc 0 b sync
e2e_file_is b .secret.env $'TOKEN=one\n'
e2e_mode_is b .secret.env 600

# Both machines write the same new plaintext, so the ciphertexts clash while the files do not:
# it settles with no question, and the file stays 0600.
e2e_write a .secret.env $'TOKEN=two\n' 600
e2e_write b .secret.env $'TOKEN=two\n' 600
e2e_dm_rc 0 a sync
e2e_dm_rc 0 b sync
e2e_err_lacks 'the pull conflicts'
e2e_err_has "saved this machine's .secret{age}.env"
e2e_dm_rc 0 a sync
for m in a b; do
    e2e_file_is "$m" .secret.env $'TOKEN=two\n'
    e2e_mode_is "$m" .secret.env 600
done
e2e_remote_never_holds 'TOKEN=two'

for m in a b; do
    e2e_dm_rc 0 "$m" status
    e2e_err_has ', 0 to look at'
done
e2e_check "both repos are at the remote's main" test \
    "$(e2e_git a rev-parse HEAD)" = "$(e2e_git b rev-parse HEAD)"

e2e_finish
