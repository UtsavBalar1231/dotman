#!/usr/bin/env bash
# [ignore] and a tracked .gitignore in a managed dir: junk is never committed, a credential in a
# file the .gitignore matches is held, and a file added by name is committed.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin journeys/11-ignore
e2e_machines a b

e2e_write a .config/nvim/init.lua $'vim.o.number = true\n'
e2e_write a .config/nvim/.gitignore $'*.log\nplugin/\n'
e2e_write a .config/nvim/.init.lua.swp 'swap'
e2e_write a .config/nvim/node_modules/x/index.js 'junk'
e2e_init a alpha
e2e_dm_rc 0 a add .config/nvim
e2e_dm_rc 0 a sync
e2e_remote_tracks .config/nvim/init.lua
e2e_remote_tracks .config/nvim/.gitignore
e2e_remote_lacks .config/nvim/.init.lua.swp
e2e_remote_lacks .config/nvim/node_modules/x/index.js

# New junk under the managed dir stays out at sync too, and so does a custom pattern.
e2e_write_repo a .dotman.toml "$(sed 's|^    "\*\*/\*\.rej",$|    "**/*.rej",\n    "**/cache.json",|' "$(e2e_repo a)/.dotman.toml")
"
e2e_check "the custom pattern is in place" grep -qF '"**/cache.json"' "$(e2e_repo a)/.dotman.toml"
e2e_write a .config/nvim/cache.json '{}'
e2e_write a .config/nvim/lua/opts.lua $'vim.o.wrap = false\n'
e2e_dm_rc 0 a sync
e2e_remote_tracks .config/nvim/lua/opts.lua
e2e_remote_lacks .config/nvim/cache.json

# A file the tracked .gitignore matches goes through the guards like any other.
token=$(e2e_token)
mkdir -p "$(e2e_repo a)/.config/nvim/plugin"
printf 'token: %s\n' "$token" >"$(e2e_repo a)/.config/nvim/creds.log"
printf 'token: %s\n' "$token" >"$(e2e_repo a)/.config/nvim/plugin/tok"
e2e_dm_rc 0 a status
e2e_out_has 'held in the repo'
e2e_out_has '.config/nvim/creds.log: line 1 holds a github-token'
e2e_out_has '.config/nvim/plugin/tok: line 1 holds a github-token'
e2e_dm_rc 3 a sync
e2e_err_lacks "$token"
e2e_remote_lacks .config/nvim/creds.log
e2e_remote_lacks .config/nvim/plugin/tok
rm -- "$(e2e_repo a)/.config/nvim/creds.log" "$(e2e_repo a)/.config/nvim/plugin/tok"

# A file [ignore] matches is committed when it is added by name, with a warning.
e2e_dm_rc 0 a add .config/nvim/cache.json
e2e_err_has 'cache.json'
e2e_dm_rc 0 a sync
e2e_remote_tracks .config/nvim/cache.json
e2e_remote_lacks .config/nvim/.init.lua.swp

e2e_join b beta
e2e_file_is b .config/nvim/cache.json '{}'
e2e_file_is b .config/nvim/lua/opts.lua $'vim.o.wrap = false\n'
e2e_absent b .config/nvim/.init.lua.swp
e2e_absent b .config/nvim/node_modules

e2e_finish
