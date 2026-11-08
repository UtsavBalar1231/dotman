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
    env.git(&["-C", &work_arg, "init", "-q", "-b", "main"]);
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
