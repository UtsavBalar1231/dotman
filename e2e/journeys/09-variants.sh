#!/usr/bin/env bash
# Variants and templates: the printed --for host= hints, for a plain and an {age} file; {tmpl} per host.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/09-variants
e2e_machines a b

e2e_write a .gitconfig.local $'[user]\n\temail = me@home.invalid\n'
e2e_write a .netrc.local $'machine home login a\n' 600
e2e_mode_cap .netrc.local 600
e2e_init a alpha
e2e_dm_rc 0 a key init
key="$(e2e_config a)/age-identity.txt"
e2e_identity_known "$key"
e2e_dm_rc 0 a add .gitconfig.local
e2e_dm_rc 0 a add --encrypt .netrc.local
e2e_write_repo a '.prompt{tmpl}' $'PS1="{{ host }} ({{ os }}) $ "\n'
e2e_dm_rc 0 a sync
e2e_dm_rc 0 a apply --take-repo .prompt
e2e_file_is a .prompt $'PS1="alpha (linux) $ "\n'

# b has its own versions of both files before it joins.
e2e_write b .gitconfig.local $'[user]\n\temail = me@work.invalid\n'
e2e_write b .netrc.local $'machine work login b\n' 600
e2e_dm_rc 3 b init "$(e2e_remote_url)" --host beta --age-key-file "$key"
e2e_file_is b .prompt $'PS1="beta (linux) $ "\n'
e2e_dm_rc 3 b sync
e2e_err_has '--keep-local captures this machine'
e2e_check "the encrypted file's hint names --encrypt" \
    grep -q -- 'add --encrypt --for host=beta .*\.netrc\.local' "$E2E_ROOT/last.err"
e2e_run_recipe b 'add (--encrypt )?--for host=beta ' 2
e2e_dm_rc 0 b sync

e2e_remote_tracks '.gitconfig{host=beta}.local'
e2e_remote_tracks '.netrc{host=beta+age}.local'
e2e_remote_never_holds 'login b'
e2e_remote_never_holds 'login a'
e2e_dm_rc 0 a sync
for m in a b; do
    e2e_dm_rc 0 "$m" status
    e2e_err_has '3 managed, 3 clean, 0 to look at'
done
e2e_file_is a .gitconfig.local $'[user]\n\temail = me@home.invalid\n'
e2e_file_is b .gitconfig.local $'[user]\n\temail = me@work.invalid\n'
e2e_file_is a .netrc.local $'machine home login a\n'
e2e_file_is b .netrc.local $'machine work login b\n'

# A local edit to a rendered file is template drift, never captured into the template.
e2e_write b .prompt $'PS1="> "\n'
e2e_dm_rc 3 b sync
e2e_err_has 'template-drift'
e2e_run_printed b "$(e2e_printed 'apply --take-repo')"
e2e_file_is b .prompt $'PS1="beta (linux) $ "\n'
e2e_check "the template source is unchanged" test \
    "$(e2e_remote_show '.prompt{tmpl}')" = 'PS1="{{ host }} ({{ os }}) $ "'

e2e_finish
