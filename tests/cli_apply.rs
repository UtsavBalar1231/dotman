mod common;

use common::{TestEnv, mode_of, tree_checksum};
use dotman::lock::Lock;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Output;

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Runs `dotman`, asserts exit 3 and returns stderr.
fn attention(env: &TestEnv, args: &[&str]) -> String {
    let out = env.run(args);
    assert_eq!(out.status.code(), Some(3), "dot {args:?}: {}", stderr(&out));
    stderr(&out)
}

fn token(env: &TestEnv, key: &str) -> Option<String> {
    env.token_of("status", key)
}

/// A repo with `files` added, so R, H and S agree for each.
fn added(files: &[(&str, &[u8], u32)]) -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let mut args = vec!["add"];
    for (key, bytes, mode) in files {
        env.write(key, bytes, *mode);
        args.push(key);
    }
    env.ok(&args);
    env
}

#[test]
fn row_1_gone_everywhere_drops_the_state() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    fs::remove_file(env.repo().join(".bashrc")).unwrap();
    fs::remove_file(env.home().join(".bashrc")).unwrap();
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("gone"));
    env.ok(&["apply"]);
    assert_eq!(token(&env, ".bashrc"), None);
    assert!(!env.home().join(".bashrc").exists());
}

#[test]
fn row_2_forgotten_upstream_keeps_the_file_and_drops_the_state() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    fs::remove_file(env.repo().join(".bashrc")).unwrap();
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("forgotten"));
    let err = env.ok(&["apply"]);
    assert!(err.contains(".bashrc: forgotten upstream"), "{err}");
    assert_eq!(env.read(".bashrc"), b"one\n");
    assert_eq!(token(&env, ".bashrc"), None);
}

#[test]
fn row_3_forgotten_with_local_edits_is_reported_and_kept() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    fs::remove_file(env.repo().join(".bashrc")).unwrap();
    env.write(".bashrc", b"edited\n", 0o644);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("forgotten-edited"));
    let err = attention(&env, &["apply", "--take-repo"]);
    assert!(err.contains("`dotman add` keeps it"), "{err}");
    assert_eq!(env.read(".bashrc"), b"edited\n");
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("forgotten-edited"));
}

#[test]
fn row_4_clean_records_a_missing_state() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
    fs::remove_file(env.state_dir().join("state.toml")).unwrap();
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
    env.ok(&["apply"]);
    env.write(".bashrc", b"edited\n", 0o644);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("changed-locally"));
}

#[test]
fn row_5_not_applied_is_written() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".config/app/a.conf", b"a\n");
    assert_eq!(
        token(&env, ".config/app/a.conf").as_deref(),
        Some("not-applied")
    );
    env.ok(&["apply"]);
    assert_eq!(env.read(".config/app/a.conf"), b"a\n");
    assert_eq!(token(&env, ".config/app/a.conf").as_deref(), Some("clean"));
    assert!(!env.state_dir().join("backups").exists());
}

#[test]
fn row_6_deleted_locally_waits_for_restore_or_forget() {
    let env = added(&[(".bashrc", b"one\n", 0o644), (".zshrc", b"z\n", 0o600)]);
    assert!(env.repo_config().modes.contains_key(".zshrc"));
    fs::remove_file(env.home().join(".bashrc")).unwrap();
    fs::remove_file(env.home().join(".zshrc")).unwrap();
    for key in [".bashrc", ".zshrc"] {
        assert_eq!(
            token(&env, key).as_deref(),
            Some("deleted-locally"),
            "{key}"
        );
    }

    let err = attention(&env, &["apply"]);
    assert!(err.contains("local deletions left undecided (2)"), "{err}");
    assert!(
        err.contains("--forget-deleted or --restore-deleted"),
        "{err}"
    );
    assert!(!env.home().join(".bashrc").exists());

    env.ok(&["apply", "--restore-deleted", ".bashrc"]);
    assert_eq!(env.read(".bashrc"), b"one\n");
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
    assert_eq!(token(&env, ".zshrc").as_deref(), Some("deleted-locally"));

    env.ok(&["apply", "--forget-deleted"]);
    assert!(!env.repo().join(".zshrc").exists());
    assert!(!env.home().join(".zshrc").exists());
    assert_eq!(token(&env, ".zshrc"), None);
    assert!(!env.repo_config().modes.contains_key(".zshrc"));
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
}

#[test]
fn row_7_deleted_and_changed_upstream_is_a_conflict() {
    let env = added(&[(".bashrc", b"one\n", 0o644), (".zshrc", b"z\n", 0o644)]);
    for (key, upstream) in [(".bashrc", b"two\n".as_slice()), (".zshrc", b"z 2\n")] {
        fs::remove_file(env.home().join(key)).unwrap();
        env.write_repo(key, upstream);
        assert_eq!(
            token(&env, key).as_deref(),
            Some("deleted-changed-upstream")
        );
    }
    let err = attention(&env, &["apply", "--restore-deleted"]);
    assert!(err.contains("conflicts left undecided (2)"), "{err}");

    env.ok(&["apply", "--take-repo", ".bashrc"]);
    assert_eq!(env.read(".bashrc"), b"two\n");
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));

    env.ok(&["apply", "--keep-local", ".zshrc"]);
    assert!(!env.home().join(".zshrc").exists());
    assert_eq!(token(&env, ".zshrc").as_deref(), Some("deleted-locally"));
}

#[test]
fn row_8_existing_file_is_backed_up_before_the_repo_version() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".bashrc", b"repo\n");
    env.write(".bashrc", b"mine\n", 0o600);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("existing"));

    let err = attention(&env, &["apply"]);
    assert!(err.contains("existing files left undecided (1)"), "{err}");
    assert_eq!(env.read(".bashrc"), b"mine\n");

    let err = env.ok(&["apply", "--take-repo"]);
    assert!(err.contains("backed up to"), "{err}");
    assert_eq!(env.read(".bashrc"), b"repo\n");
    assert_eq!(mode_of(&env.home().join(".bashrc")), 0o644);
    assert_eq!(env.backups(".bashrc"), [(b"mine\n".to_vec(), 0o600)]);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
}

#[test]
fn row_9_changed_locally_is_reported_and_never_overwritten() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    env.write(".bashrc", b"edited\n", 0o644);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("changed-locally"));
    let err = env.ok(&["apply", "--take-repo"]);
    assert!(err.contains(".bashrc: changed locally"), "{err}");
    assert_eq!(env.read(".bashrc"), b"edited\n");
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("changed-locally"));
}

#[test]
fn row_10_changed_in_repo_is_written_without_a_backup() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    env.write_repo(".bashrc", b"two\n");
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("changed-in-repo"));
    env.ok(&["apply"]);
    assert_eq!(env.read(".bashrc"), b"two\n");
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
    assert!(!env.state_dir().join("backups").exists());
}

#[test]
fn an_absolute_link_into_home_is_replaced_when_the_repo_changes_it() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"x\n", 0o644);
    env.symlink(".config/link", env.home().join(".bashrc"));
    env.ok(&["add", ".config/link"]);
    let repo_link = env.repo().join(".config/link");
    assert_eq!(fs::read_link(&repo_link).unwrap(), Path::new("../.bashrc"));
    fs::remove_file(&repo_link).unwrap();
    symlink("../.profile", &repo_link).unwrap();
    assert_eq!(
        token(&env, ".config/link").as_deref(),
        Some("changed-in-repo")
    );
    env.ok(&["apply"]);
    assert_eq!(
        fs::read_link(env.home().join(".config/link")).unwrap(),
        Path::new("../.profile")
    );
    assert_eq!(token(&env, ".config/link").as_deref(), Some("clean"));
}

#[test]
fn row_11_both_changed_is_a_conflict() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    env.write_repo(".bashrc", b"repo\n");
    env.write(".bashrc", b"mine\n", 0o644);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("both-changed"));

    let err = attention(&env, &["apply"]);
    assert!(err.contains("conflicts left undecided (1)"), "{err}");
    assert!(err.contains("--keep-local or --take-repo"), "{err}");
    assert_eq!(env.read(".bashrc"), b"mine\n");

    env.ok(&["apply", "--take-repo"]);
    assert_eq!(env.read(".bashrc"), b"repo\n");
    assert_eq!(env.backups(".bashrc"), [(b"mine\n".to_vec(), 0o600)]);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
}

#[test]
fn blocked_directory_is_reported_and_left_alone() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".config/app.conf", b"x\n");
    fs::create_dir_all(env.home().join(".config/app.conf/inner")).unwrap();
    assert_eq!(token(&env, ".config/app.conf").as_deref(), Some("blocked"));
    let before = tree_checksum(&env.home());
    let err = attention(&env, &["apply", "--take-repo"]);
    assert!(err.contains("a directory is in the way"), "{err}");
    assert_eq!(tree_checksum(&env.home()), before);
    assert_eq!(token(&env, ".config/app.conf").as_deref(), Some("blocked"));
}

#[test]
fn fresh_machine_non_interactive_init_skips_existing_files() {
    let a = added(&[(".bashrc", b"from A\n", 0o644), (".vimrc", b"vim\n", 0o644)]);
    a.push_repo();
    let b = a.second_device();
    b.write(".bashrc", b"B's own\n", 0o644);

    let err = attention(&b, &["init", &a.remote_url()]);
    assert!(err.contains("existing files left undecided (1)"), "{err}");
    assert!(err.contains("  .bashrc"), "{err}");
    assert_eq!(b.read(".bashrc"), b"B's own\n");
    assert_eq!(b.read(".vimrc"), b"vim\n");
    assert_eq!(token(&b, ".bashrc").as_deref(), Some("existing"));
    assert!(!b.state_dir().join("backups").exists());
    assert!(
        b.fails(&["init", &a.remote_url()])
            .contains("a repo already exists")
    );
}

#[test]
fn fresh_machine_take_repo_backs_up_the_original() {
    let a = added(&[(".bashrc", b"from A\n", 0o644)]);
    a.push_repo();
    let b = a.second_device();
    b.write(".bashrc", b"B's own\n", 0o644);

    let err = b.ok(&["init", &a.remote_url(), "--take-repo"]);
    assert!(
        err.contains("1 existing"),
        "the status summary comes first: {err}"
    );
    assert_eq!(b.read(".bashrc"), b"from A\n");
    assert_eq!(b.backups(".bashrc"), [(b"B's own\n".to_vec(), 0o600)]);
    assert_eq!(token(&b, ".bashrc").as_deref(), Some("clean"));
}

#[test]
fn fresh_machine_keep_local_leaves_the_file_changed_locally() {
    let a = added(&[(".bashrc", b"from A\n", 0o644)]);
    a.push_repo();
    let b = a.second_device();
    b.write(".bashrc", b"B's own\n", 0o644);

    b.ok(&["init", &a.remote_url(), "--keep-local"]);
    assert_eq!(b.read(".bashrc"), b"B's own\n");
    assert_eq!(token(&b, ".bashrc").as_deref(), Some("changed-locally"));
    assert!(!b.state_dir().join("backups").exists());
}

#[test]
fn hostile_symlinks_never_carry_a_write_outside_home() {
    let a = TestEnv::new();
    let outside = a.root().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("kept"), b"kept\n").unwrap();
    fs::write(a.root().join("outside-file"), b"outside\n").unwrap();
    a.ok(&["init"]);
    symlink(&outside, a.repo().join(".config")).unwrap();
    a.write_repo(".bashrc", b"repo\n");
    a.push_repo();
    let outside_before = tree_checksum(&outside);

    let b = a.second_device();
    b.symlink(".bashrc", a.root().join("outside-file"));
    let err = attention(&b, &["init", &a.remote_url(), "--take-repo"]);
    assert!(err.contains(".bashrc: is a symlink"), "{err}");
    assert_eq!(token(&b, ".bashrc").as_deref(), Some("symlink-at-target"));
    assert_eq!(
        fs::read(a.root().join("outside-file")).unwrap(),
        b"outside\n"
    );
    assert_eq!(fs::read_link(b.home().join(".config")).unwrap(), outside);

    // Upstream turns `.config` into a directory with a file in it; this HOME still has the link.
    fs::remove_file(b.repo().join(".config")).unwrap();
    b.write_repo(".config/x", b"pwned\n");
    let err = attention(&b, &["apply", "--take-repo"]);
    assert!(
        err.contains("`.config` is a symlink or not a directory"),
        "{err}"
    );
    assert_eq!(token(&b, ".config/x").as_deref(), Some("blocked"));
    assert_eq!(tree_checksum(&outside), outside_before);
    assert!(!outside.join("x").exists());
}

#[test]
fn recorded_modes_are_applied_without_special_bits() {
    let a = TestEnv::new();
    a.ok(&["init"]);
    a.write(".local/bin/tool", b"#!/bin/sh\n", 0o755);
    a.write(".ssh/config", b"Host *\n", 0o600);
    fs::set_permissions(a.home().join(".ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    a.ok(&["add", ".local/bin/tool", ".ssh/config"]);
    let mut config = a.repo_config();
    config
        .modes
        .insert(".local/bin/tool".to_owned(), "4755".to_owned());
    config.save(&a.repo().join(".dotman.toml"), false).unwrap();
    a.push_repo();

    let b = a.second_device();
    b.ok(&["init", &a.remote_url()]);
    assert_eq!(mode_of(&b.home().join(".local/bin/tool")), 0o755);
    assert_eq!(mode_of(&b.home().join(".ssh/config")), 0o600);
    assert_eq!(mode_of(&b.home().join(".ssh")), 0o700);
    assert_eq!(b.read(".ssh/config"), b"Host *\n");
    for key in [".local/bin/tool", ".ssh/config"] {
        assert_eq!(token(&b, key).as_deref(), Some("clean"), "{key}");
    }
}

#[test]
fn a_malformed_mode_fails_a_dry_run() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    let config = env.repo().join(".dotman.toml");
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.contains("\n[modes]\n"), "{text}");
    fs::write(
        &config,
        text.replace("\n[modes]\n", "\n[modes]\n\".bashrc\" = \"rwx\"\n"),
    )
    .unwrap();
    let before = env.four_trees();
    let err = env.fails(&["apply", "--dry-run"]);
    assert_eq!(env.four_trees(), before);
    assert!(err.contains("[modes] entry `.bashrc`"), "{err}");
}

#[test]
fn a_moved_repo_keeps_its_state_and_another_repo_starts_empty() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    env.write(".bashrc", b"edited\n", 0o644);
    let at = |repo: &Path, args: &[&str]| {
        env.dot()
            .env("DOTMAN_REPO_PATH", repo)
            .args(args)
            .output()
            .expect("run dot")
    };
    let status_at = |repo: &Path| {
        String::from_utf8_lossy(&at(repo, &["status", "--porcelain"]).stdout).into_owned()
    };

    let moved = env.root().join("moved");
    fs::rename(env.repo(), &moved).unwrap();
    assert_eq!(status_at(&moved), "changed-locally\t.bashrc\0");

    let other = env.root().join("other");
    assert!(at(&other, &["init"]).status.success());
    fs::write(other.join(".bashrc"), b"one\n").unwrap();
    assert_eq!(status_at(&other), "existing\t.bashrc\0");
}

#[test]
fn dry_run_apply_changes_none_of_the_four_trees() {
    let env = added(&[
        (".bashrc", b"one\n", 0o644),
        (".zshrc", b"z\n", 0o644),
        (".profile", b"p\n", 0o644),
    ]);
    env.write_repo(".bashrc", b"repo\n");
    env.write(".bashrc", b"mine\n", 0o644);
    fs::remove_file(env.home().join(".zshrc")).unwrap();
    env.write_repo(".profile", b"p 2\n");
    env.write_repo(".config/new/app.conf", b"new\n");

    let before = env.four_trees();
    let args = ["apply", "--dry-run", "--take-repo", "--forget-deleted"];
    let err = env.ok(&args);
    for expected in ["would apply", "would back up to", "would forget"] {
        assert!(err.contains(expected), "missing `{expected}` in:\n{err}");
    }
    assert_eq!(env.four_trees(), before);

    env.ok(&["apply", "--take-repo", "--forget-deleted"]);
    assert_ne!(env.four_trees(), before);
}

#[test]
fn dry_run_init_from_a_url_changes_none_of_the_four_trees() {
    let a = added(&[(".bashrc", b"from A\n", 0o644), (".vimrc", b"vim\n", 0o644)]);
    a.push_repo();
    let b = a.second_device();
    b.write(".bashrc", b"B's own\n", 0o644);
    let tmp = b.root().join("tmp");
    fs::create_dir(&tmp).unwrap();

    let before = b.four_trees();
    let out = b
        .dot()
        .env("TMPDIR", &tmp)
        .args(["init", &a.remote_url(), "--dry-run", "--take-repo"])
        .output()
        .unwrap();
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    for expected in ["would clone", "would apply", "would back up to"] {
        assert!(err.contains(expected), "missing `{expected}` in:\n{err}");
    }
    assert_eq!(b.four_trees(), before);
    assert!(!b.repo().exists() && !b.state_dir().exists());
    assert_eq!(
        fs::read_dir(&tmp).unwrap().count(),
        0,
        "the temp clone is removed"
    );
}

#[test]
fn a_second_apply_fails_while_the_lock_is_held() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    env.write_repo(".bashrc", b"two\n");
    let held = Lock::acquire(&env.state_dir().join("lock"), false).unwrap();
    assert!(held.is_held());
    for args in [&["apply"][..], &["apply", "--dry-run"]] {
        let err = env.fails(args);
        assert!(err.contains("another dotman process is running"), "{err}");
    }
    assert_eq!(env.read(".bashrc"), b"one\n");
}

#[test]
fn diff_prints_a_unified_diff_from_home_to_repo() {
    let env = added(&[(".bashrc", b"one\ntwo\n", 0o644), (".zshrc", b"z\n", 0o644)]);
    env.write(".bashrc", b"one\nTWO\n", 0o644);
    env.write_repo(".config/new.conf", b"new\n");
    let tmp = env.root().join("tmp");
    fs::create_dir(&tmp).unwrap();
    let before = env.four_trees();
    let diff = |args: &[&str]| {
        let out = env
            .dot()
            .env("TMPDIR", &tmp)
            .arg("diff")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let all = diff(&[]);
    for expected in [
        "diff --git a/.bashrc b/.bashrc",
        "--- a/.bashrc",
        "+++ b/.bashrc",
        "@@ ",
        "-TWO",
        "+two",
        "+++ b/.config/new.conf",
    ] {
        assert!(all.contains(expected), "missing `{expected}` in:\n{all}");
    }
    assert!(!all.contains(".zshrc"), "{all}");
    assert!(!all.contains('\u{1b}'), "no colour into a pipe:\n{all}");

    let one = diff(&["--no-pager", ".bashrc"]);
    assert!(one.contains("+two") && !one.contains("new.conf"), "{one}");
    assert_eq!(diff(&[".zshrc"]), "");
    assert_eq!(env.four_trees(), before);
    assert_eq!(
        fs::read_dir(&tmp).unwrap().count(),
        0,
        "temp trees are removed"
    );
}
