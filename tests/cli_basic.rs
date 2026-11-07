mod common;

use common::{TestEnv, mode_of, tree_checksum};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const REMOTE_URL: &str = "https://example.invalid/dots.git";

#[test]
fn init_twice_leaves_every_tree_unchanged() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    assert!(env.repo().join(".git/dotman-id").is_file());
    assert!(env.repo().join(".dotman.toml").is_file());
    let before = env.four_trees();
    let config_before = tree_checksum(&env.xdg_config());
    env.ok(&["init"]);
    assert_eq!(env.four_trees(), before);
    assert_eq!(tree_checksum(&env.xdg_config()), config_before);
}

#[test]
fn init_remote_sets_and_replaces_origin() {
    let env = TestEnv::new();
    let repo_arg = env.repo().to_string_lossy().into_owned();
    let origin = || {
        env.git(&["-C", &repo_arg, "config", "--get", "remote.origin.url"])
            .trim()
            .to_owned()
    };
    env.ok(&["init", "--remote", REMOTE_URL]);
    assert_eq!(origin(), REMOTE_URL);
    let other = "https://example.invalid/other.git";
    env.ok(&["init", "--remote", other]);
    assert_eq!(origin(), other);
    env.fails(&["init", "--remote=--upload-pack=x"]);
}

#[test]
fn init_dry_run_creates_nothing() {
    let env = TestEnv::new();
    let before = env.four_trees();
    let err = env.ok(&["init", "--dry-run", "--remote", REMOTE_URL]);
    assert!(err.contains("would create the repo"), "{err}");
    assert_eq!(env.four_trees(), before);
    assert!(!env.repo().exists() && !env.state_dir().exists());
}

#[test]
fn directory_preview_counts_match_the_fixture() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"x", 0o644);
    env.write(".config/app/a.conf", &[b'a'; 10], 0o644);
    env.write(".config/app/b.conf", &[b'b'; 20], 0o644);
    env.write(".config/app/private.txt", &[b'p'; 5], 0o600);
    let mut blob = vec![b'z'; 100];
    blob.insert(0, 0);
    blob.truncate(100);
    env.write(".config/app/blob.bin", &blob, 0o644);
    env.write(".config/app/sub/c.conf", &[b'c'; 30], 0o644);
    env.write(".config/app/sub/big.dat", &[b'd'; 300], 0o644);
    env.write(".config/app/sub/editor.swp", b"ignored", 0o644);
    env.symlink(".config/app/dangling", "/nonexistent/target");
    env.symlink(
        ".config/app/to-bashrc",
        env.home().join(".bashrc").as_path(),
    );

    let err = env.ok(&["add", ".config/app"]);
    for expected in [
        "files: 7",
        "total size: 465 bytes",
        "binary files: 1",
        "files at mode 0600: 1",
        "skipped: 1",
        ".config/app/dangling: dangling symlink",
        "300 bytes  .config/app/sub/big.dat",
        "100 bytes  .config/app/blob.bin",
        "10 bytes  .config/app/a.conf",
    ] {
        assert!(err.contains(expected), "missing `{expected}` in:\n{err}");
    }
    assert!(!err.contains("5 bytes  .config/app/private.txt"), "{err}");
    assert!(
        !err.contains("editor.swp"),
        "ignored files stay out:\n{err}"
    );
    assert_eq!(env.repo_config().dirs.paths, [".config/app"]);
    assert_eq!(
        env.porcelain("list").len(),
        7,
        "the seven previewed entries are managed"
    );
}

#[test]
fn absolute_symlink_targets_become_relative_and_dangling_links_are_skipped() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"x", 0o644);
    env.symlink(".config/inside", env.home().join(".bashrc").as_path());
    env.symlink(".config/outside", "/etc/hostname");
    env.symlink(".config/relative", "../.bashrc");
    env.symlink("dangling", "/nonexistent/target");

    env.ok(&[
        "add",
        ".config/inside",
        ".config/outside",
        ".config/relative",
    ]);
    let err = env.ok(&["add", "dangling"]);
    assert!(err.contains("dangling symlink"), "{err}");

    let target = |key: &str| fs::read_link(env.repo().join(key)).expect("read_link");
    assert_eq!(target(".config/inside"), Path::new("../.bashrc"));
    assert_eq!(target(".config/outside"), Path::new("/etc/hostname"));
    assert_eq!(target(".config/relative"), Path::new("../.bashrc"));
    assert!(fs::symlink_metadata(env.repo().join("dangling")).is_err());
    assert_eq!(
        env.token_of("list", ".config/inside").as_deref(),
        Some("symlink")
    );
}

#[test]
fn private_file_and_directory_modes_are_recorded() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".ssh/config", b"Host *\n", 0o600);
    fs::set_permissions(env.home().join(".ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    env.write(".local/bin/run", b"#!/bin/sh\n", 0o755);
    env.write(".profile", b"x", 0o644);

    env.ok(&["add", ".ssh/config", ".local/bin/run", ".profile"]);
    let modes = env.repo_config().modes;
    assert_eq!(modes.get(".ssh/config").map(String::as_str), Some("0600"));
    assert_eq!(modes.get(".ssh/").map(String::as_str), Some("0700"));
    assert!(!modes.contains_key(".local/bin/run"), "{modes:?}");
    assert!(!modes.contains_key(".profile"), "{modes:?}");
    assert_eq!(mode_of(&env.repo().join(".local/bin/run")), 0o755);
    assert_eq!(mode_of(&env.repo().join(".ssh/config")), 0o644);
    for key in [".ssh/config", ".local/bin/run", ".profile"] {
        assert_eq!(
            env.token_of("status", key).as_deref(),
            Some("clean"),
            "{key}"
        );
    }
}

#[test]
fn forget_keeps_the_file_in_home() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"export A=1\n", 0o644);
    env.write(".ssh/config", b"Host *\n", 0o600);
    fs::set_permissions(env.home().join(".ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    env.ok(&["add", ".bashrc", ".ssh/config"]);

    env.ok(&["forget", ".bashrc", ".ssh/config"]);
    assert_eq!(
        fs::read(env.home().join(".bashrc")).unwrap(),
        b"export A=1\n"
    );
    assert_eq!(
        fs::read(env.home().join(".ssh/config")).unwrap(),
        b"Host *\n"
    );
    assert!(!env.repo().join(".bashrc").exists());
    assert!(!env.repo().join(".ssh").exists());
    assert_eq!(env.porcelain("list"), Vec::new());
    assert_eq!(env.porcelain("status"), Vec::new());
    assert!(env.repo_config().modes.is_empty());
    assert!(env.fails(&["forget", ".bashrc"]).contains("not managed"));
}

#[test]
fn forget_directory_removes_its_files_and_entry() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/app/a.conf", b"a", 0o644);
    env.write(".config/app/sub/b.conf", b"b", 0o644);
    env.write(".zshrc", b"z", 0o644);
    env.ok(&["add", ".config/app", ".zshrc"]);
    env.ok(&["forget", ".config/app"]);
    let left: Vec<String> = env.porcelain("list").into_iter().map(|(_, k)| k).collect();
    assert_eq!(left, [".zshrc"]);
    assert_eq!(env.repo_config().dirs.paths, Vec::<String>::new());
    assert!(env.home().join(".config/app/sub/b.conf").is_file());
    assert!(!env.repo().join(".config").exists());
}

#[test]
fn status_porcelain_reports_rows() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"one\n", 0o644);
    env.write(".zshrc", b"zsh\n", 0o644);
    env.ok(&["add", ".bashrc", ".zshrc"]);
    assert_eq!(env.token_of("status", ".bashrc").as_deref(), Some("clean"));

    env.write(".bashrc", b"two\n", 0o644);
    fs::remove_file(env.home().join(".zshrc")).unwrap();
    assert_eq!(
        env.token_of("status", ".bashrc").as_deref(),
        Some("changed-locally")
    );
    assert_eq!(
        env.token_of("status", ".zshrc").as_deref(),
        Some("deleted-locally")
    );

    let out = env.run(&["status", "--porcelain"]);
    assert!(out.stdout.ends_with(b"\0"));
    let human = env.run(&["status"]);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("changed-locally") && text.contains(".bashrc"),
        "{text}"
    );
}

#[test]
fn status_and_list_change_nothing() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"one\n", 0o644);
    env.ok(&["add", ".bashrc"]);
    env.write(".bashrc", b"two\n", 0o644);
    let before = env.four_trees();
    env.ok(&["status"]);
    env.ok(&["list"]);
    assert_eq!(env.four_trees(), before);
}

#[test]
fn own_files_and_non_nfc_names_are_refused() {
    let env = TestEnv::new();
    let inside = |args: &[&str]| {
        let mut cmd = env.dot();
        cmd.env("DOTMAN_REPO_PATH", env.home().join(".dotfiles"))
            .env("XDG_STATE_HOME", env.home().join(".local/state"))
            .args(args);
        cmd.output().expect("run dot")
    };
    assert!(inside(&["init"]).status.success());
    let refused = |path: &str| {
        let out = inside(&["add", path]);
        assert_eq!(out.status.code(), Some(1), "{path}");
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            err.contains("dotman never manages its own files"),
            "{path}: {err}"
        );
    };
    refused(".dotfiles");
    refused(".dotfiles/.dotman.toml");
    refused(".local/state/dotman");

    let nfd = "e\u{301}.txt";
    env.write(nfd, b"x", 0o644);
    let before = tree_checksum(&env.home());
    let out = inside(&["add", nfd]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("not NFC-normalized"), "{err}");
    assert_eq!(tree_checksum(&env.home()), before);
}

#[test]
fn non_nfc_names_inside_a_directory_are_skipped_and_reported() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write("dir/fine.txt", b"x", 0o644);
    env.write("dir/e\u{301}.txt", b"x", 0o644);
    let err = env.ok(&["add", "dir"]);
    assert!(
        err.contains("files: 1") && err.contains("skipped: 1"),
        "{err}"
    );
    assert!(err.contains("not NFC-normalized"), "{err}");
}

#[test]
fn dry_runs_leave_all_four_trees_equal() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"one\n", 0o644);
    env.write(".config/app/a.conf", b"a", 0o600);
    let before = env.four_trees();
    let err = env.ok(&["add", "--dry-run", ".bashrc", ".config/app"]);
    assert!(err.contains("would add"), "{err}");
    assert_eq!(env.four_trees(), before);

    env.ok(&["add", ".bashrc", ".config/app"]);
    let before = env.four_trees();
    let err = env.ok(&["forget", "--dry-run", ".bashrc", ".config/app"]);
    assert!(err.contains("forget"), "{err}");
    assert_eq!(env.four_trees(), before);
    assert_eq!(env.porcelain("list").len(), 2);
}

#[test]
fn a_symlinked_worktree_parent_is_never_written_through() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let outside = env.root().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, env.repo().join(".config")).unwrap();
    env.write(".config/x", b"secret", 0o644);
    let err = env.fails(&["add", ".config/x"]);
    assert!(err.contains("could not add"), "{err}");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn commands_need_an_initialised_repo() {
    let env = TestEnv::new();
    env.write(".bashrc", b"x", 0o644);
    for args in [
        &["add", ".bashrc"][..],
        &["list"],
        &["status"],
        &["forget", ".bashrc"],
    ] {
        assert!(env.fails(args).contains("dot init"), "{args:?}");
    }
    assert!(!env.repo().exists());
}
