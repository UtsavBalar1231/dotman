//! `dotman init` with no source looks for an existing setup before it creates a repo.

mod common;

use common::TestEnv;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A bare repo with one commit whose worktree is `$HOME`, as yadm and vcsh leave it.
// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn bare_home_repo(env: &TestEnv, dest: &Path) {
    let work = env
        .root()
        .join("work")
        .join(dest.file_name().expect("name"));
    fs::create_dir_all(&work).expect("create work tree");
    let work_arg = work.to_string_lossy().into_owned();
    env.git(&["-C", &work_arg, "init", "-q"]);
    env.git(&["-C", &work_arg, "symbolic-ref", "HEAD", "refs/heads/main"]);
    fs::write(work.join(".bashrc"), "bash\n").expect("write file");
    env.git(&["-C", &work_arg, "add", "-A"]);
    env.git(&["-C", &work_arg, "commit", "-q", "-m", "first"]);
    fs::create_dir_all(dest.parent().expect("parent")).expect("create parent");
    let dest_arg = dest.to_string_lossy().into_owned();
    env.git(&["clone", "-q", "--bare", &work_arg, &dest_arg]);
    env.git(&["-C", &dest_arg, "config", "core.bare", "false"]);
    env.git(&[
        "-C",
        &dest_arg,
        "config",
        "core.worktree",
        &env.home().to_string_lossy(),
    ]);
}

fn yadm_repo(env: &TestEnv) -> PathBuf {
    env.xdg_data().join("yadm/repo.git")
}

#[test]
fn a_non_interactive_init_finds_a_yadm_repo_prints_the_import_and_creates_nothing() {
    let env = TestEnv::new();
    bare_home_repo(&env, &yadm_repo(&env));
    let before = env.four_trees();
    let out = env.run(&["init"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(
        err.contains(&format!("dotman import {}", yadm_repo(&env).display())),
        "{err}"
    );
    assert!(err.contains("dotman init --no-import"), "{err}");
    assert!(!env.repo().exists());
    assert_eq!(env.four_trees(), before);
}

#[test]
fn several_setups_are_all_printed() {
    let env = TestEnv::new();
    bare_home_repo(&env, &yadm_repo(&env));
    let cfg = env.home().join(".cfg");
    bare_home_repo(&env, &cfg);
    let out = env.run(&["init", "--remote", "git@example.invalid:me/dots.git"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    for path in [yadm_repo(&env), cfg] {
        assert!(
            err.contains(&format!("dotman import {}", path.display())),
            "{err}"
        );
    }
    assert!(
        err.contains("dotman init --no-import --remote git@example.invalid:me/dots.git"),
        "{err}"
    );
    assert!(!env.repo().exists());
}

#[test]
fn an_empty_home_creates_the_repo() {
    let env = TestEnv::new();
    let out = env.run(&["init"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(env.repo().join(".git").exists());
}

#[test]
fn no_import_creates_a_fresh_repo_and_leaves_the_yadm_repo_alone() {
    let env = TestEnv::new();
    bare_home_repo(&env, &yadm_repo(&env));
    let yadm_before = common::tree_checksum(&yadm_repo(&env));
    let out = env.run(&["init", "--no-import"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(env.repo().join(".git").exists());
    assert_eq!(common::tree_checksum(&yadm_repo(&env)), yadm_before);
}

#[test]
fn init_on_an_existing_repo_does_no_detection() {
    let env = TestEnv::new();
    assert_eq!(env.run(&["init"]).status.code(), Some(0));
    env.write(".bashrc", b"x\n", 0o644);
    assert_eq!(env.run(&["add", ".bashrc"]).status.code(), Some(0));
    assert_eq!(env.run(&["sync"]).status.code(), Some(0));
    bare_home_repo(&env, &yadm_repo(&env));
    let out = env.run(&["init"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(!err.contains("existing setup"), "{err}");
}

#[test]
fn a_source_and_no_import_conflict() {
    let env = TestEnv::new();
    let out = env.run(&["init", "--no-import", "https://example.invalid/x.git"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

/// A remote that holds `.bashrc` on `main` while its `HEAD` names `master`, which has no commits.
fn remote_with_unborn_head() -> TestEnv {
    let a = TestEnv::new();
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write(".bashrc", b"bash\n", 0o644);
    a.ok(&["add", ".bashrc"]);
    a.ok(&["sync"]);
    point_head_at(&a, "master");
    a
}

fn point_head_at(a: &TestEnv, branch: &str) {
    let remote = a.remote_path().to_string_lossy().into_owned();
    a.git(&[
        "--git-dir",
        &remote,
        "symbolic-ref",
        "HEAD",
        &format!("refs/heads/{branch}"),
    ]);
}

fn staging_left_beside_the_repo(env: &TestEnv) -> bool {
    fs::read_dir(env.xdg_data()).is_ok_and(|dir| {
        dir.filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with(".dotman-init-"))
    })
}

#[test]
fn init_url_applies_the_branch_of_a_remote_whose_head_is_unborn() {
    let a = remote_with_unborn_head();
    let b = a.second_device();
    let err = b.ok(&["init", &a.remote_url()]);
    assert!(
        err.contains(
            "the remote's HEAD names a branch with no commits; checked out main, the only branch it has"
        ),
        "{err}"
    );
    assert_eq!(b.read(".bashrc"), b"bash\n");
    let repo = b.repo().to_string_lossy().into_owned();
    assert_eq!(
        b.git(&["-C", &repo, "symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );

    b.write(".zshrc", b"zsh\n", 0o644);
    b.ok(&["add", ".zshrc"]);
    b.ok(&["sync"]);
    let refs = a.remote_refs();
    assert!(refs.contains("refs/heads/main"), "{refs}");
    assert!(!refs.contains("refs/heads/master"), "{refs}");
}

#[test]
fn init_url_dry_run_plans_that_branch() {
    let a = remote_with_unborn_head();
    let b = a.second_device();
    let err = b.ok(&["init", &a.remote_url(), "--dry-run"]);
    assert!(
        err.contains("would check out main, the only branch it has"),
        "{err}"
    );
    assert!(err.contains(".bashrc"), "{err}");
    assert!(!b.repo().exists());
    assert!(!b.home().join(".bashrc").exists());
    assert!(!staging_left_beside_the_repo(&b));
}

#[test]
fn init_url_refuses_an_unborn_head_with_several_branches_and_creates_nothing() {
    let a = remote_with_unborn_head();
    let remote = a.remote_path().to_string_lossy().into_owned();
    a.git(&["--git-dir", &remote, "branch", "-m", "main", "dots"]);
    a.git(&["--git-dir", &remote, "branch", "other", "dots"]);
    let b = a.second_device();
    let url = a.remote_url();
    for args in [&["init", &url, "--dry-run"][..], &["init", &url]] {
        let err = b.fails(args);
        assert!(
            err.contains("none of its 2 branches is main (dots, other)"),
            "{err}"
        );
        assert!(err.contains("Nothing was cloned"), "{err}");
        assert!(!b.repo().exists());
        assert!(!b.home().join(".bashrc").exists());
        assert!(!staging_left_beside_the_repo(&b));
    }
}

#[test]
fn a_rerun_of_init_url_heals_a_clone_left_on_an_unborn_branch() {
    let a = remote_with_unborn_head();
    point_head_at(&a, "main");
    let b = a.second_device();
    b.ok(&["init", &a.remote_url()]);
    let repo = b.repo().to_string_lossy().into_owned();
    b.git(&["-C", &repo, "symbolic-ref", "HEAD", "refs/heads/master"]);
    b.git(&["-C", &repo, "update-ref", "-d", "refs/heads/main"]);
    let born = b.git_raw(&["-C", &repo, "rev-parse", "-q", "--verify", "HEAD"]);
    assert!(!born.status.success());

    let err = b.ok(&["init", &a.remote_url()]);
    assert!(err.contains("already holds the clone"), "{err}");
    assert!(
        err.contains("the remote's HEAD names a branch with no commits; checked out main"),
        "{err}"
    );
    assert_eq!(
        b.git(&["-C", &repo, "symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    assert_eq!(b.git(&["-C", &repo, "status", "--porcelain"]), "");
    assert!(b.repo().join(".bashrc").is_file());
}
