#!/usr/bin/env bash
# Stow farm: tool launchers are no farm, a failed --swap restores every link, and the printed --swap then works.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin import/05-farm
e2e_machines a

home=$(e2e_home a)
# Two tool launchers, as npm or pipx put them in ~/.local/bin, and nothing else linked.
for tool in a b; do
    mkdir -p "$home/.tool/pkg/$tool/dist" "$home/.local/bin"
    printf '#!/usr/bin/env node\n' >"$home/.tool/pkg/$tool/dist/cli.js"
    chmod 0755 "$home/.tool/pkg/$tool/dist/cli.js"
    ln -s "../../.tool/pkg/$tool/dist/cli.js" "$home/.local/bin/tool-$tool"
done
before=$(e2e_manifest_dir "$home")
e2e_dm_rc 1 a import
e2e_err_has 'none found'
e2e_check "\$HOME is unchanged" test "$before" = "$(e2e_manifest_dir "$home")"

# A stow farm beside them.
farm=$home/dotfiles
mkdir -p "$farm/zsh" "$farm/tmux" "$farm/scripts/.local/bin" "$farm/nvim/.config/nvim/lua" "$farm/ro/.ro"
printf 'export A=1\n' >"$farm/zsh/.zshrc"
printf 'set -g mouse on\n' >"$farm/tmux/.tmux.conf"
printf '#!/bin/sh\necho hi\n' >"$farm/scripts/.local/bin/hello"
chmod 0755 "$farm/scripts/.local/bin/hello"
printf 'vim.o.number = true\n' >"$farm/nvim/.config/nvim/init.lua"
printf 'return {}\n' >"$farm/nvim/.config/nvim/lua/opts.lua"
chmod 0600 "$farm/nvim/.config/nvim/lua/opts.lua"
printf 'read only\n' >"$farm/ro/.ro/conf"
mkdir -p "$home/.config" "$home/.ro"
ln -s dotfiles/zsh/.zshrc "$home/.zshrc"
ln -s dotfiles/tmux/.tmux.conf "$home/.tmux.conf"
ln -s ../../dotfiles/scripts/.local/bin/hello "$home/.local/bin/hello"
ln -s ../dotfiles/nvim/.config/nvim "$home/.config/nvim"
ln -s ../dotfiles/ro/.ro/conf "$home/.ro/conf"
farm_before=$(e2e_manifest_dir "$farm")

e2e_dm_rc 0 a import --no-bootstrap
e2e_err_has "imported from the symlink farm $farm"
e2e_err_lacks .tool
tree=$(e2e_git a ls-tree -r --name-only HEAD)
for key in .zshrc .tmux.conf .local/bin/hello .config/nvim/init.lua .config/nvim/lua/opts.lua .ro/conf; do
    e2e_check "the import tracks $key" grep -qxF -- "$key" <<<"$tree"
done
e2e_check "no tool launcher is imported" test -z "$(grep -F tool- <<<"$tree" || true)"
swap=$(e2e_printed '^dotman import --swap$')
e2e_check "the import prints the swap" test -n "$swap"

# A directory the swap cannot write to makes it fail part way; every link comes back.
chmod 0555 "$home/.ro"
home_before=$(e2e_manifest_dir "$home")
e2e_run_printed a "$swap" 1
e2e_check "the failed swap names the file" grep -qF 'could not replace .ro/conf' "$E2E_ROOT/recipe.out"
e2e_check "the failed swap restores the links" grep -qE '[0-9]+ links restored' "$E2E_ROOT/recipe.out"
e2e_check "\$HOME is as it was before the swap" test "$home_before" = "$(e2e_manifest_dir "$home")"
chmod 0755 "$home/.ro"

e2e_run_printed a "$(e2e_printed '^dotman status$')"
e2e_run_printed a "$swap"
e2e_run_printed a "$(e2e_printed '^dotman apply$')"
for key in .zshrc .tmux.conf .local/bin/hello .ro/conf; do
    e2e_check "\$HOME/$key is a regular file" test -f "$home/$key" -a ! -L "$home/$key"
done
e2e_check "\$HOME/.config/nvim is a directory" test -d "$home/.config/nvim" -a ! -L "$home/.config/nvim"
e2e_file_is a .zshrc $'export A=1\n'
e2e_file_is a .config/nvim/init.lua $'vim.o.number = true\n'
e2e_mode_is a .local/bin/hello 755
e2e_mode_is a .config/nvim/lua/opts.lua 600
e2e_check "the tool launchers are still links" test -L "$home/.local/bin/tool-a" -a -L "$home/.local/bin/tool-b"
e2e_check "the farm is unchanged" test "$farm_before" = "$(e2e_manifest_dir "$farm")"
e2e_dm_rc 0 a status
e2e_err_has '6 managed, 6 clean, 0 to look at'

e2e_finish
