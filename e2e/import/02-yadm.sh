#!/usr/bin/env bash
# yadm: alternates become variants, a yadm template becomes a dotman one, and the result applies.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR source=../lib.sh
source "$(dirname "$0")/../lib.sh"

e2e_begin import/02-yadm
e2e_machines a

home=$(e2e_home a)
work=$E2E_ROOT/src/yadm
mkdir -p "$work/.config/app"
printf 'alias ll=ls\n' >"$work/.bashrc"
printf 'linux\n' >"$work/.zshrc##o.Linux"
printf 'mac\n' >"$work/.zshrc##o.Darwin"
printf 'work=1\n' >"$work/.config/app/app.conf##c.work"
printf 'work=0\n' >"$work/.config/app/app.conf##default"
# shellcheck disable=SC2016  # yadm's template syntax, not shell
printf '[core]\n{%% if yadm.os == "Linux" %%}\neditor = vim\n{%% else %%}\neditor = nano\n{%% endif %%}\n' \
    >"$work/.toolrc##template"
e2e_tool git init -q "$work"
e2e_tool git -C "$work" symbolic-ref HEAD refs/heads/main
e2e_tool git -C "$work" add -A
e2e_tool git -C "$work" commit -q -m 'yadm files'
# yadm keeps a bare repo whose work tree is $HOME.
yadm_repo="$(e2e_mroot a)/xdg/data/yadm/repo.git"
e2e_tool git clone -q --bare --no-local "$work" "$yadm_repo"
e2e_tool git --git-dir="$yadm_repo" config core.bare false
e2e_tool git --git-dir="$yadm_repo" config core.worktree "$home"
e2e_tool git --git-dir="$yadm_repo" remote remove origin
mkdir -p "$(e2e_mroot a)/xdg/config/yadm"
e2e_tool git config --file "$(e2e_mroot a)/xdg/config/yadm/config" local.class work
# What yadm leaves in $HOME: the tracked files, a link per alternate and the rendered template.
cp -a -- "$work/.bashrc" "$work/.zshrc##o.Linux" "$work/.zshrc##o.Darwin" "$work/.toolrc##template" "$home/"
mkdir -p "$home/.config/app"
cp -a -- "$work/.config/app/app.conf##c.work" "$work/.config/app/app.conf##default" "$home/.config/app/"
ln -s '.zshrc##o.Linux' "$home/.zshrc"
ln -s 'app.conf##c.work' "$home/.config/app/app.conf"
printf '[core]\neditor = vim\n' >"$home/.toolrc"
source_before=$(e2e_manifest_dir "$yadm_repo")

e2e_dm_rc 0 a import --no-bootstrap
e2e_err_has 'other yadm machines'
e2e_err_has "yadm's local settings (profiles work) go into the machine config"
e2e_err_has "yadm's alternate links in \$HOME (2) block the converted files"
tree=$(e2e_git a ls-tree -r --name-only HEAD)
for key in '.zshrc{os=linux}' '.zshrc{os=macos}' '.toolrc{tmpl}' '.config/app/app{profile=work}.conf' .config/app/app.conf .bashrc; do
    e2e_check "the import tracks $key" grep -qxF -- "$key" <<<"$tree"
done
e2e_check "no ## name is left in the tree" test -z "$(grep -F '##' <<<"$tree" || true)"
e2e_check "the template is a dotman one" grep -qF '{% if os == "linux" %}' "$(e2e_repo a)/.toolrc{tmpl}"
e2e_check "the yadm repo is unchanged" test "$source_before" = "$(e2e_manifest_dir "$yadm_repo")"

swap=$(e2e_printed '^dotman import --swap$')
e2e_run_printed a "$(e2e_printed '^dotman status$')"
e2e_run_printed a "$swap"
e2e_run_printed a "$(e2e_printed '^dotman apply$')"
e2e_check "\$HOME/.zshrc is a regular file again" test -f "$home/.zshrc" -a ! -L "$home/.zshrc"
e2e_file_is a .zshrc $'linux\n'
e2e_file_is a .config/app/app.conf $'work=1\n'
e2e_file_is a .toolrc $'[core]\neditor = vim\n'
e2e_dm_rc 0 a status
e2e_err_has '4 managed, 4 clean, 0 to look at'

# Without the class, the default alternate wins once the printed step accepts the new identity.
sed -i '/^profiles/d' "$(e2e_config a)/config.toml"
e2e_dm_rc 3 a apply
e2e_err_has 'identity changed (profiles work -> (none))'
e2e_file_is a .config/app/app.conf $'work=1\n'
e2e_run_printed a "$(e2e_printed '^dotman apply --accept-identity$')"
e2e_file_is a .config/app/app.conf $'work=0\n'
e2e_check "the class's version is in a backup" grep -rqxF 'work=1' "$(e2e_backups a)"

e2e_finish
