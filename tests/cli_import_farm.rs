//! `dotman import` on symlink farms, and `dotman import --swap`. Every farm is built with `mkdir` and
//! `ln -s` equivalents in the harness's temp home, and every adoption test checks that the farm is
//! untouched.

mod common;

use common::{Entry, GITCONFIG, TestEnv, accounting, mode_of, stow, tree_checksum};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn link_text(env: &TestEnv, rel: &str) -> PathBuf {
    fs::read_link(env.home().join(rel)).expect("read the link")
}

fn is_link(env: &TestEnv, rel: &str) -> bool {
    fs::symlink_metadata(env.home().join(rel)).is_ok_and(|meta| meta.file_type().is_symlink())
}

fn is_file(env: &TestEnv, rel: &str) -> bool {
    fs::symlink_metadata(env.home().join(rel)).is_ok_and(|meta| meta.file_type().is_file())
}

fn repo_has(env: &TestEnv, key: &str) -> bool {
    fs::symlink_metadata(env.repo().join(key)).is_ok()
}

fn farm_dir(env: &TestEnv, name: &str) -> PathBuf {
    env.home().join(name)
}

fn farm_sum(env: &TestEnv, name: &str) -> BTreeMap<String, Entry> {
    tree_checksum(&farm_dir(env, name))
}

/// The keys of `stow`, with the folded directory as one entry.
const STOW_LINKS: [&str; 5] = [
    ".config/nvim",
    ".gitconfig",
    ".local/bin/hello",
    ".tmux.conf",
    ".zshrc",
];

/// Six files: four file links and two below the folded directory.
const STOW_FILES: usize = 6;

/// A second farm that holds `files` as `<name>/<package>/<file>`, linked into `$HOME`.
fn small_farm(env: &TestEnv, name: &str) {
    for file in ["a", "b"] {
        env.write(
            format!("{name}/pkg/.{name}-{file}"),
            format!("{name} {file}\n").as_bytes(),
            0o644,
        );
        env.symlink(
            format!(".{name}-{file}"),
            format!("{name}/pkg/.{name}-{file}"),
        );
    }
}

/// Puts the permissions back when the test ends, so the temp dir can be removed.
struct Writable(PathBuf);

impl Drop for Writable {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

fn run_import(env: &TestEnv, args: &[&str]) -> (Output, String) {
    let mut full = vec!["import"];
    full.extend_from_slice(args);
    let out = env.run(&full);
    let err = stderr(&out);
    (out, err)
}

/// Home, repo and remote: the trees a run that only reports must leave alone. The state dir is
/// left out because every run that is not a dry run creates its lock file.
fn untouched(env: &TestEnv) -> [BTreeMap<String, Entry>; 3] {
    let [home, repo, _, remote] = env.four_trees();
    [home, repo, remote]
}

fn commit_subject(env: &TestEnv) -> String {
    env.git(&[
        "-C",
        &env.repo().to_string_lossy(),
        "log",
        "-1",
        "--format=%s",
    ])
    .trim()
    .to_owned()
}

#[test]
fn a_stow_farm_is_found_and_every_file_is_imported_with_home_unchanged() {
    let env = TestEnv::new();
    stow(&env);
    let home = tree_checksum(&env.home());
    let farm = farm_sum(&env, "dotfiles");

    let (out, err) = run_import(&env, &[]);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert_eq!(accounting(&err), (STOW_FILES, STOW_FILES, 0), "{err}");
    assert_eq!(tree_checksum(&env.home()), home, "home changed");
    assert_eq!(farm_sum(&env, "dotfiles"), farm, "the farm changed");

    for (key, text) in [
        (".zshrc", "export A=1\n"),
        (".gitconfig", GITCONFIG),
        (".tmux.conf", "set -g mouse on\n"),
        (".local/bin/hello", "#!/bin/sh\necho hi\n"),
        (".config/nvim/init.lua", "vim.o.number = true\n"),
        (".config/nvim/lua/opts.lua", "return {}\n"),
    ] {
        assert_eq!(
            fs::read_to_string(env.repo().join(key)).unwrap_or_default(),
            text,
            "{key}"
        );
    }
    let config = env.repo_config();
    assert_eq!(config.dirs.paths, [".config/nvim"]);
    assert_eq!(
        config
            .modes
            .get(".config/nvim/lua/opts.lua")
            .map(String::as_str),
        Some("0600")
    );
    assert!(!config.modes.contains_key(".local/bin/hello"), "{config:?}");
    assert_eq!(
        commit_subject(&env),
        format!(
            "import: symlink farm {}",
            farm_dir(&env, "dotfiles").display()
        )
    );
    assert!(err.contains("dotman import --swap"), "{err}");
    assert!(err.contains("stow -D"), "{err}");
    assert!(
        err.contains("files: 2"),
        "the directory preview is missing:\n{err}"
    );
    assert!(err.contains("largest:"), "{err}");

    let tokens: BTreeMap<String, String> = env
        .porcelain("status")
        .into_iter()
        .map(|(token, key)| (key, token))
        .collect();
    assert_eq!(
        tokens.get(".zshrc").map(String::as_str),
        Some("symlink-at-target")
    );
    assert_eq!(
        tokens.get(".config/nvim/init.lua").map(String::as_str),
        Some("blocked")
    );
}

#[test]
fn an_explicit_farm_directory_is_imported_and_a_dry_run_changes_nothing() {
    let env = TestEnv::new();
    stow(&env);
    let farm = farm_dir(&env, "dotfiles");
    let before = env.four_trees();

    let (out, err) = run_import(&env, &["--dry-run", &farm.to_string_lossy()]);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert_eq!(accounting(&err), (STOW_FILES, STOW_FILES, 0), "{err}");
    assert!(err.contains("would import 6 files"), "{err}");
    assert_eq!(env.four_trees(), before);
    assert!(!env.repo().exists() && !env.state_dir().exists());

    let (out, err) = run_import(&env, &[&farm.to_string_lossy()]);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert_eq!(accounting(&err), (STOW_FILES, STOW_FILES, 0), "{err}");
    assert!(repo_has(&env, ".config/nvim/init.lua"));
}

#[test]
fn swap_turns_every_link_into_a_regular_file_with_the_same_bytes() {
    let env = TestEnv::new();
    stow(&env);
    env.ok(&["import"]);
    let farm = farm_sum(&env, "dotfiles");
    let originals: Vec<(&str, PathBuf)> = STOW_LINKS
        .iter()
        .map(|key| (*key, link_text(&env, key)))
        .collect();
    let bytes: Vec<(&str, Vec<u8>)> = [
        ".zshrc",
        ".gitconfig",
        ".tmux.conf",
        ".local/bin/hello",
        ".config/nvim/init.lua",
        ".config/nvim/lua/opts.lua",
    ]
    .iter()
    .map(|key| (*key, env.read(key)))
    .collect();

    let (out, err) = run_import(&env, &["--swap"]);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("swapped 5 links"), "{err}");

    for key in STOW_LINKS {
        assert!(!is_link(&env, key), "{key} is still a link");
    }
    assert!(fs::symlink_metadata(env.home().join(".config/nvim")).is_ok_and(|m| m.is_dir()));
    for (key, expected) in &bytes {
        assert!(is_file(&env, key), "{key}");
        assert_eq!(&env.read(key), expected, "{key}");
    }
    assert_eq!(mode_of(&env.home().join(".local/bin/hello")), 0o755);
    assert_eq!(
        mode_of(&env.home().join(".config/nvim/lua/opts.lua")),
        0o600
    );
    assert_eq!(mode_of(&env.home().join(".zshrc")), 0o644);
    assert_eq!(farm_sum(&env, "dotfiles"), farm, "the farm changed");

    let tokens = env.porcelain("status");
    assert!(
        tokens.iter().all(|(token, _)| token == "clean"),
        "status is not clean: {tokens:?}"
    );
    assert_eq!(tokens.len(), STOW_FILES);
    let leftovers: Vec<String> = tree_checksum(&env.home())
        .into_keys()
        .filter(|name| name.contains(".dotman-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    let runs: Vec<PathBuf> = fs::read_dir(env.state_dir().join("backups"))
        .expect("backups exist")
        .map(|run| run.expect("run").path())
        .collect();
    assert_eq!(runs.len(), 1, "{runs:?}");
    let record = runs
        .first()
        .map(|run| run.join(".dotman/links.txt"))
        .unwrap_or_default();
    let lines: Vec<String> = fs::read_to_string(&record)
        .expect("links.txt")
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), 5, "{lines:?}");
    for (key, target) in originals {
        let line = format!("{key}\t{}", target.display());
        assert!(lines.contains(&line), "{line} missing from {lines:?}");
    }
    assert_eq!(mode_of(&record), 0o600);
    assert_eq!(
        mode_of(record.parent().unwrap_or_else(|| Path::new("/"))),
        0o700
    );

    let (out, err) = run_import(&env, &["--swap"]);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("no links to replace"), "{err}");
}

/// A farm made under umask 002 holds 0664 files and 0775 directories. The import records no group
/// or other write bit, as `add` does not: 0664 and 0775 record nothing, 0660 records 0640. The
/// swap writes the repo's modes, so this machine is clean, and a second machine gets 0644 and 0755.
#[test]
fn a_farm_import_never_records_group_or_other_write_bits() {
    let env = TestEnv::new();
    let chmod = |rel: &str, mode: u32| {
        fs::set_permissions(env.home().join(rel), fs::Permissions::from_mode(mode)).expect("chmod");
    };
    env.write("dotfiles/zsh/.zshrc", b"export A=1\n", 0o664);
    env.write("dotfiles/nvim/.config/nvim/init.lua", b"vim\n", 0o664);
    env.write(
        "dotfiles/nvim/.config/nvim/lua/a.lua",
        b"return {}\n",
        0o664,
    );
    env.write("dotfiles/team/.config/team/x.conf", b"x\n", 0o660);
    env.write("dotfiles/bin/.local/bin/tool", b"#!/bin/sh\n", 0o775);
    for dir in [
        "dotfiles/nvim/.config/nvim",
        "dotfiles/nvim/.config/nvim/lua",
        "dotfiles/bin/.local/bin",
    ] {
        chmod(dir, 0o775);
    }
    chmod("dotfiles/team/.config/team", 0o770);
    env.symlink(".zshrc", "dotfiles/zsh/.zshrc");
    env.symlink(".config/nvim", "../dotfiles/nvim/.config/nvim");
    env.symlink(".config/team", "../dotfiles/team/.config/team");
    env.symlink(".local/bin/tool", "../../dotfiles/bin/.local/bin/tool");
    for dir in [".config", ".local", ".local/bin"] {
        chmod(dir, 0o775);
    }

    env.ok(&["import"]);
    let modes: Vec<(String, String)> = env.repo_config().modes.into_iter().collect();
    let expected = [(".config/team/", "0750"), (".config/team/x.conf", "0640")]
        .map(|(key, mode)| (key.to_owned(), mode.to_owned()));
    assert_eq!(modes, expected);

    env.ok(&["import", "--swap", "--yes"]);
    let tokens = env.porcelain("status");
    assert!(
        tokens.iter().all(|(token, _)| token == "clean"),
        "status is not clean: {tokens:?}"
    );
    assert_eq!(tokens.len(), 5);
    for (key, mode) in [
        (".zshrc", 0o644),
        (".config/nvim", 0o755),
        (".config/nvim/init.lua", 0o644),
        (".config/team", 0o750),
        (".config/team/x.conf", 0o640),
        (".local/bin/tool", 0o755),
    ] {
        assert_eq!(mode_of(&env.home().join(key)), mode, "{key}");
    }

    env.ok(&["init", "--remote", &env.remote_url()]);
    env.ok(&["sync"]);
    env.git(&[
        "--git-dir",
        &env.remote_path().to_string_lossy(),
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    let other = env.second_device();
    other.ok(&["init", &env.remote_url()]);
    for (key, mode) in [
        (".zshrc", 0o644),
        (".config/nvim", 0o755),
        (".config/nvim/lua", 0o755),
        (".config/nvim/init.lua", 0o644),
        (".config/team", 0o750),
        (".config/team/x.conf", 0o640),
        (".local/bin/tool", 0o755),
    ] {
        assert_eq!(mode_of(&other.home().join(key)), mode, "{key}");
    }
}

#[test]
fn a_dry_run_swap_leaves_all_four_trees_equal() {
    let env = TestEnv::new();
    stow(&env);
    env.ok(&["import"]);
    let before = env.four_trees();

    let (out, err) = run_import(&env, &["--swap", "--dry-run"]);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("would swap"), "{err}");
    assert!(err.contains("would replace 5 links"), "{err}");
    assert_eq!(env.four_trees(), before);
    assert!(!env.state_dir().join("backups").exists());
    for key in STOW_LINKS {
        assert!(is_link(&env, key), "{key}");
    }
}

#[test]
fn a_failure_restores_every_link_with_its_own_target_text() {
    let env = TestEnv::new();
    stow(&env);
    env.write("dotfiles/ro/conf", b"read only\n", 0o644);
    env.symlink("zro/conf", "../dotfiles/ro/conf");
    env.ok(&["import"]);
    let readonly = Writable(env.home().join("zro"));
    fs::set_permissions(&readonly.0, fs::Permissions::from_mode(0o555)).expect("chmod");
    let home = tree_checksum(&env.home());
    let farm = farm_sum(&env, "dotfiles");

    let (out, err) = run_import(&env, &["--swap"]);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("could not replace zro/conf"), "{err}");
    assert!(err.contains("6 links restored"), "{err}");

    for key in STOW_LINKS {
        assert!(is_link(&env, key), "{key} is not a link again");
    }
    assert_eq!(link_text(&env, ".zshrc"), Path::new("dotfiles/zsh/.zshrc"));
    assert_eq!(
        link_text(&env, ".config/nvim"),
        Path::new("../dotfiles/nvim/.config/nvim")
    );
    assert_eq!(
        link_text(&env, ".local/bin/hello"),
        Path::new("../../dotfiles/scripts/.local/bin/hello")
    );
    assert_eq!(
        tree_checksum(&env.home()),
        home,
        "home differs from before the swap"
    );
    assert_eq!(farm_sum(&env, "dotfiles"), farm);
    let record = fs::read_dir(env.state_dir().join("backups"))
        .expect("backups")
        .map(|run| run.expect("run").path().join(".dotman/links.txt"))
        .any(|file| file.exists());
    assert!(record, "links.txt is gone");
    drop(readonly);

    // The repo and S are unchanged, so a retry after the fix works.
    let (out, err) = run_import(&env, &["--swap"]);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(is_file(&env, "zro/conf"));
}

#[test]
fn links_into_the_nix_store_are_reported_never_imported() {
    let env = TestEnv::new();
    stow(&env);
    let store = env.root().join("nix/store");
    env.write(
        "../nix/store/h1-home-manager-files/.config/foo.conf",
        b"nix\n",
        0o444,
    );
    env.write(
        "../nix/store/h1-home-manager-files/.config/bar.conf",
        b"nix\n",
        0o444,
    );
    let hm = store.join("h1-home-manager-files/.config");
    env.symlink(".config/foo.conf", hm.join("foo.conf"));
    env.symlink(".config/bar.conf", hm.join("bar.conf"));
    let store_var = store.to_string_lossy().into_owned();
    let home = tree_checksum(&env.home());

    let out = env.run_with(&[("DOTMAN_NIX_STORE", &store_var)], &["import"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (STOW_FILES + 2, STOW_FILES, 2), "{err}");
    assert!(err.contains("links into the Nix store"), "{err}");
    assert!(err.contains(".config/foo.conf"), "{err}");
    assert!(!repo_has(&env, ".config/foo.conf") && !repo_has(&env, ".config/bar.conf"));
    assert_eq!(tree_checksum(&env.home()), home);

    let doctor = env.run_with(&[("DOTMAN_NIX_STORE", &store_var)], &["doctor"]);
    let report = stdout(&doctor);
    assert!(
        report
            .lines()
            .any(|line| line.starts_with("warn") && line.contains("DOTMAN_NIX_STORE")),
        "{report}"
    );
}

#[test]
fn the_nix_store_given_as_the_source_reports_every_link_and_imports_nothing() {
    let env = TestEnv::new();
    let store = env.root().join("nix/store");
    let hm = store.join("h1-home-manager-files/.config");
    for name in ["foo.conf", "bar.conf"] {
        env.write(
            format!("../nix/store/h1-home-manager-files/.config/{name}"),
            b"nix\n",
            0o444,
        );
        env.symlink(format!(".config/{name}"), hm.join(name));
    }
    let store_var = store.to_string_lossy().into_owned();
    let before = untouched(&env);

    let out = env.run_with(
        &[("DOTMAN_NIX_STORE", &store_var)],
        &[
            "import",
            &store.join("h1-home-manager-files").to_string_lossy(),
        ],
    );
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (2, 0, 2), "{err}");
    assert_eq!(untouched(&env), before);
    assert!(!env.repo().exists());

    let out = env.run_with(&[("DOTMAN_NIX_STORE", &store_var)], &["import"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("none found"), "{err}");
}

#[test]
fn links_into_system_directories_are_no_farm_and_stay_as_they_are() {
    let env = TestEnv::new();
    let mut entries: Vec<PathBuf> = fs::read_dir("/usr/share")
        .map(|dir| dir.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    entries.sort();
    let [first, second, ..] = entries.as_slice() else {
        eprintln!("skipping: /usr/share has fewer than two entries");
        return;
    };
    env.symlink(".local/share/x", first);
    env.symlink(".local/share/y", second);
    let home = tree_checksum(&env.home());

    let (out, err) = run_import(&env, &[]);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("none found"), "{err}");
    let (out, err) = run_import(&env, &["/usr/share"]);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("system directory"), "{err}");
    assert_eq!(tree_checksum(&env.home()), home);

    // Beside a real farm the links are no entries of it, and the swap leaves them alone.
    stow(&env);
    let home = tree_checksum(&env.home());
    let (out, err) = run_import(&env, &[]);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert_eq!(accounting(&err), (STOW_FILES, STOW_FILES, 0), "{err}");
    assert_eq!(tree_checksum(&env.home()), home);
    let (out, err) = run_import(&env, &["--swap"]);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert_eq!(link_text(&env, ".local/share/x"), first.as_path());
    assert_eq!(link_text(&env, ".local/share/y"), second.as_path());
}

#[test]
fn a_link_whose_content_was_edited_is_reported_by_the_swap_and_kept() {
    let env = TestEnv::new();
    stow(&env);
    env.ok(&["import"]);
    env.write("dotfiles/zsh/.zshrc", b"export A=2 # edited\n", 0o644);

    let (out, err) = run_import(&env, &["--swap"]);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(err.contains("content differs from the repo"), "{err}");
    assert!(err.contains(".zshrc -> dotfiles/zsh/.zshrc"), "{err}");
    assert!(is_link(&env, ".zshrc"));
    assert_eq!(
        fs::read(env.home().join("dotfiles/zsh/.zshrc")).expect("farm file"),
        b"export A=2 # edited\n"
    );
    for key in [
        ".gitconfig",
        ".tmux.conf",
        ".local/bin/hello",
        ".config/nvim",
    ] {
        assert!(!is_link(&env, key), "{key}");
    }
}

#[test]
fn a_folded_directory_with_a_file_the_repo_lacks_is_kept_as_a_link() {
    let env = TestEnv::new();
    stow(&env);
    env.ok(&["import"]);
    env.write("dotfiles/nvim/.config/nvim/new.lua", b"-- new\n", 0o644);

    let (out, err) = run_import(&env, &["--swap"]);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(err.contains("new.lua, which the repo lacks"), "{err}");
    assert!(is_link(&env, ".config/nvim"));
    assert!(!is_link(&env, ".zshrc"));
}

#[test]
fn a_dangling_link_is_reported_and_not_counted_as_converted() {
    let env = TestEnv::new();
    stow(&env);
    env.symlink(".gone", "dotfiles/missing/.gone");
    let home = tree_checksum(&env.home());

    let (out, err) = run_import(&env, &[]);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (STOW_FILES + 1, STOW_FILES, 1), "{err}");
    assert!(err.contains("dangling links"), "{err}");
    assert!(err.contains(".gone -> dotfiles/missing/.gone"), "{err}");
    assert_eq!(tree_checksum(&env.home()), home);
}

#[test]
fn held_files_are_reported_like_add_does() {
    let env = TestEnv::new();
    stow(&env);
    let key = format!("AGE-SECRET-KEY-1{}\n", "Q".repeat(58));
    env.write("dotfiles/keys/.age-key", key.as_bytes(), 0o600);
    env.symlink(".age-key", "dotfiles/keys/.age-key");
    env.write("dotfiles/keys/.netrc", b"machine x\n", 0o600);
    env.symlink(".netrc", "dotfiles/keys/.netrc");
    env.write("dotfiles/nvim/.config/nvim/blob.bin", b"\0\0\0", 0o644);

    let (out, err) = run_import(&env, &[]);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (STOW_FILES + 3, STOW_FILES, 3), "{err}");
    assert!(err.contains("holds an age secret key"), "{err}");
    assert!(err.contains("name matches [guard] secret_names"), "{err}");
    assert!(err.contains("binary"), "{err}");
    assert!(!repo_has(&env, ".age-key") && !repo_has(&env, ".netrc"));
    assert!(!repo_has(&env, ".config/nvim/blob.bin"));
}

#[test]
fn two_farms_exit_3_and_name_the_command_for_each() {
    let env = TestEnv::new();
    small_farm(&env, "one");
    small_farm(&env, "two");
    let before = untouched(&env);

    let (out, err) = run_import(&env, &[]);
    assert_eq!(out.status.code(), Some(3), "{err}");
    for name in ["one", "two"] {
        let command = format!("dotman import {}", farm_dir(&env, name).display());
        assert!(err.contains(&command), "{command} missing from:\n{err}");
    }
    assert!(err.contains("symlink farm"), "{err}");
    assert_eq!(untouched(&env), before);
    assert!(!env.repo().exists());
}

#[test]
fn a_farm_that_is_a_git_checkout_is_still_a_farm() {
    let env = TestEnv::new();
    stow(&env);
    let farm = farm_dir(&env, "dotfiles").to_string_lossy().into_owned();
    env.git(&["-C", &farm, "init", "-q"]);
    env.git(&["-C", &farm, "symbolic-ref", "HEAD", "refs/heads/main"]);
    env.git(&["-C", &farm, "add", "-A"]);
    env.git(&["-C", &farm, "commit", "-q", "-m", "farm"]);
    env.git(&[
        "-C",
        &farm,
        "remote",
        "add",
        "origin",
        "https://example.invalid/me/dotfiles.git",
    ]);
    let sum = farm_sum(&env, "dotfiles");

    let (out, err) = run_import(&env, &[&farm]);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert_eq!(accounting(&err), (STOW_FILES, STOW_FILES, 0), "{err}");
    assert!(err.contains("git history is not imported"), "{err}");
    assert!(
        err.contains("https://example.invalid/me/dotfiles.git"),
        "{err}"
    );
    assert_eq!(farm_sum(&env, "dotfiles"), sum);
    assert!(!repo_has(&env, "zsh/.zshrc"));
}

#[test]
fn files_other_tools_keep_in_a_farm_are_reported_and_never_run() {
    let env = TestEnv::new();
    stow(&env);
    env.write(
        "dotfiles/install.conf.yaml",
        b"- shell:\n    - touch /tmp/never\n",
        0o644,
    );
    env.write("dotfiles/hooks/post-up/x", b"#!/bin/sh\nexit 1\n", 0o755);
    env.write("dotfiles/zsh/.stow-local-ignore", b"README\n", 0o644);

    let (out, err) = run_import(&env, &[]);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("install.conf.yaml (dotbot"), "{err}");
    assert!(err.contains("hooks (rcm hooks)"), "{err}");
    assert!(err.contains("zsh/.stow-local-ignore (Stow)"), "{err}");
}

#[test]
fn a_yadm_link_to_a_double_hash_file_is_swapped() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_machine_config("[identity]\nos = \"linux\"\n");
    env.write_repo(".zshrc{os=linux}", b"linux zshrc\n");
    env.write(".zshrc##o.Linux", b"linux zshrc\n", 0o644);
    env.symlink(".zshrc", ".zshrc##o.Linux");
    assert_eq!(
        env.token_of("status", ".zshrc").as_deref(),
        Some("symlink-at-target")
    );

    let (out, err) = run_import(&env, &["--swap"]);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(is_file(&env, ".zshrc"), "the link is still there");
    assert_eq!(env.read(".zshrc"), b"linux zshrc\n");
    assert_eq!(env.read(".zshrc##o.Linux"), b"linux zshrc\n");
    assert_eq!(env.token_of("status", ".zshrc").as_deref(), Some("clean"));
}

#[test]
fn swap_needs_a_repo_and_takes_no_source() {
    let env = TestEnv::new();
    stow(&env);
    let (out, err) = run_import(&env, &["--swap"]);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("no dotman repo"), "{err}");
    assert!(is_link(&env, ".zshrc"));

    for args in [
        &["--swap", "somewhere"][..],
        &["--swap", "--overlay", "somewhere"],
        &["--swap", "--render-templates"],
    ] {
        let (out, err) = run_import(&env, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {err}");
    }
}

#[test]
fn a_repo_with_files_refuses_a_farm_import() {
    let env = TestEnv::new();
    stow(&env);
    env.ok(&["import"]);
    let (out, err) = run_import(&env, &[]);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("already has files or history"), "{err}");
}

#[test]
fn links_that_share_one_target_or_stay_in_one_directory_are_no_farm() {
    let env = TestEnv::new();
    env.write(".local/share/Steam/steam.sh", b"#!/bin/sh\n", 0o755);
    env.symlink(".steam/root", "../.local/share/Steam");
    env.symlink(".steam/steam", "../.local/share/Steam");
    env.write(".zshrc##o.Linux", b"linux\n", 0o644);
    env.symlink(".zshrc", ".zshrc##o.Linux");
    let home = tree_checksum(&env.home());

    let (out, err) = run_import(&env, &[]);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("none found"), "{err}");
    assert_eq!(tree_checksum(&env.home()), home);
}

#[test]
fn tool_launchers_in_a_bin_directory_are_no_farm_and_a_stow_farm_still_is() {
    let env = TestEnv::new();
    for tool in ["a", "b"] {
        env.write(
            format!(".tool/pkg/{tool}/dist/cli.js"),
            b"#!/usr/bin/env node\n",
            0o755,
        );
        env.symlink(
            format!(".local/bin/tool-{tool}"),
            format!("../../.tool/pkg/{tool}/dist/cli.js"),
        );
    }
    let (out, err) = run_import(&env, &[]);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("none found"), "{err}");

    stow(&env);
    let (out, err) = run_import(&env, &["--dry-run"]);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("imported from the symlink farm"), "{err}");
    assert!(!err.contains(".tool"), "{err}");
}

#[test]
fn a_hand_made_farm_keeps_its_renamed_bin_links_and_its_root() {
    let env = TestEnv::new();
    for (name, text) in [("bashrc", "b\n"), ("zshrc", "z\n")] {
        env.write(format!("dotfiles/shell/{name}"), text.as_bytes(), 0o644);
        env.symlink(format!(".{name}"), format!("dotfiles/shell/{name}"));
    }
    for script in ["a", "b", "c"] {
        env.write(
            format!("dotfiles/scripts/{script}.sh"),
            b"#!/bin/sh\n",
            0o755,
        );
        env.symlink(
            format!(".local/bin/{script}"),
            format!("../../dotfiles/scripts/{script}.sh"),
        );
    }
    let (out, err) = run_import(&env, &["--dry-run"]);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert_eq!(accounting(&err), (5, 5, 0), "{err}");
    let root = env.home().join("dotfiles");
    assert!(
        err.contains(&format!(
            "imported from the symlink farm {};",
            root.display()
        )),
        "{err}"
    );
}

#[test]
fn a_root_under_dev_is_a_system_directory() {
    let env = TestEnv::new();
    env.symlink(".null", "/dev/null");
    env.symlink(".zero", "/dev/zero");
    let (out, err) = run_import(&env, &["/dev"]);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("lies under /dev, a system directory"), "{err}");
}

#[test]
fn a_folded_base_directory_names_the_restow_and_the_other_packages() {
    let env = TestEnv::new();
    env.write("dotfiles/nvim/.config/nvim/init.lua", b"-- init\n", 0o644);
    env.write("dotfiles/zsh/.zshrc", b"export A=1\n", 0o644);
    env.symlink(".config", "dotfiles/nvim/.config");
    env.symlink(".zshrc", "dotfiles/zsh/.zshrc");
    let farm = env.home().join("dotfiles");

    let (out, err) = run_import(&env, &[&farm.to_string_lossy()]);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("contains a base directory such as ~/.config"),
        "{err}"
    );
    let restow = format!(
        "stow -d {} -t {} --no-folding -R nvim",
        farm.display(),
        env.home().display()
    );
    assert!(
        err.contains("Stow folded ~/.config into a link to the package nvim")
            && err.contains(&restow),
        "{err}"
    );
    let zsh = farm.join("zsh");
    assert!(
        err.contains(&format!("dotman import {}", zsh.display())),
        "{err}"
    );
    let (out, err) = run_import(&env, &["--dry-run", &zsh.to_string_lossy()]);
    assert_eq!(out.status.code(), Some(0), "{err}");

    // Given the package itself, the hint still names the package, not its `.config`.
    let (out, err) = run_import(&env, &[&farm.join("nvim").to_string_lossy()]);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("into a link to the package nvim;") && err.contains(&restow),
        "{err}"
    );
    assert!(
        err.contains(&format!("dotman import {}", zsh.display())),
        "{err}"
    );
}
