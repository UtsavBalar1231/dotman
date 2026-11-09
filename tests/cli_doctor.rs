mod common;

use common::{TestEnv, tree_checksum};
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn doctor(env: &TestEnv, args: &[&str]) -> (Option<i32>, String) {
    let mut full = vec!["doctor"];
    full.extend_from_slice(args);
    let out = env.run(&full);
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn line<'a>(out: &'a str, tag: &str, needle: &str) -> Option<&'a str> {
    out.lines()
        .find(|l| l.starts_with(tag) && l.contains(needle))
}

fn healthy() -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env
}

#[test]
fn a_healthy_environment_exits_0() {
    let env = healthy();
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(!out.lines().any(|l| l.starts_with("fail")), "{out}");
    assert!(line(&out, "pass", "git identity").is_some(), "{out}");
    assert!(line(&out, "info", "backups").is_some(), "{out}");
}

#[test]
fn a_missing_identity_fails() {
    let env = healthy();
    fs::write(env.home().join(".gitconfig"), "").expect("write .gitconfig");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(line(&out, "fail", "git identity").is_some(), "{out}");
}

#[test]
fn a_corrupt_state_is_named_and_fails() {
    let env = healthy();
    let state = env.state_dir().join("state.toml");
    fs::create_dir_all(env.state_dir()).expect("create state dir");
    fs::write(&state, "not = [toml").expect("write state");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        line(&out, "fail", &state.to_string_lossy()).is_some(),
        "{out}"
    );
}

#[test]
fn a_wide_state_dir_warns() {
    let env = healthy();
    fs::create_dir_all(env.state_dir()).expect("create state dir");
    fs::set_permissions(env.state_dir(), fs::Permissions::from_mode(0o755)).expect("chmod");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(line(&out, "warn", "state dir").is_some(), "{out}");
}

#[test]
fn online_reaches_a_file_remote() {
    let env = healthy();
    let (code, out) = doctor(&env, &["--online"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(line(&out, "pass", "remote reachable").is_some(), "{out}");
}

/// A healthy repo with `.bashrc` committed and pushed.
fn committed() -> TestEnv {
    let env = healthy();
    env.write(".bashrc", b"x\n", 0o644);
    env.ok(&["add", ".bashrc"]);
    env.ok(&["sync"]);
    env
}

#[test]
fn damaged_objects_fail_the_object_check() {
    let env = committed();
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(line(&out, "pass", "repo objects").is_some(), "{out}");

    let objects = env.repo().join(".git/objects");
    let loose = tree_checksum(&objects)
        .into_keys()
        .filter(|name| name.len() == 41 && name.as_bytes().get(2) == Some(&b'/'));
    for name in loose {
        let file = objects.join(name);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("chmod");
        fs::write(&file, b"").expect("truncate");
    }
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    let failed = line(&out, "fail", "repo objects").unwrap_or_default();
    assert!(
        failed.contains("git fsck failed") && failed.contains("init <url>"),
        "{out}"
    );
}

#[test]
fn a_half_made_repo_is_named_with_a_fix_that_keeps_commits() {
    let repo_line = |env: &TestEnv| {
        let (code, out) = doctor(env, &[]);
        assert_eq!(code, Some(1), "{out}");
        assert!(
            line(&out, "fail", "repo id").is_none(),
            "no circular hint: {out}"
        );
        line(&out, "fail", "is incomplete")
            .unwrap_or_default()
            .to_owned()
    };
    let repo = |env: &TestEnv| env.repo().display().to_string();

    let env = TestEnv::new();
    env.ok(&["init"]);
    fs::remove_file(env.repo().join(".git/dotman-id")).expect("remove id");
    let failed = repo_line(&env);
    assert!(failed.contains(".git/dotman-id is missing"), "{failed}");
    assert!(
        failed.contains(&format!("remove {} and rerun", repo(&env))),
        "{failed}"
    );

    let env = TestEnv::new();
    env.ok(&["init"]);
    fs::remove_file(env.repo().join(".git/HEAD")).expect("remove HEAD");
    let failed = repo_line(&env);
    assert!(failed.contains(".git/HEAD is missing"), "{failed}");
    assert!(failed.contains("rerun dotman init"), "{failed}");

    let env = committed();
    fs::remove_file(env.repo().join(".git/dotman-id")).expect("remove id");
    let failed = repo_line(&env);
    assert!(failed.contains("holds commits, so move"), "{failed}");
    assert!(
        !failed.contains(&format!("remove {}", repo(&env))),
        "{failed}"
    );

    let env = committed();
    let lock = env.repo().join(".git/config.lock");
    fs::write(&lock, "").expect("write lock");
    let failed = repo_line(&env);
    assert!(failed.contains("config.lock is left"), "{failed}");
    assert!(
        failed.contains(&format!("remove {}", lock.display())),
        "{failed}"
    );
}

#[test]
fn a_readable_age_identity_warns() {
    let env = healthy();
    let identity = env.xdg_config().join("dotman/age-identity.txt");
    fs::create_dir_all(identity.parent().expect("parent")).expect("mkdir");
    fs::write(&identity, "AGE-SECRET-KEY-1QQQQ\n").expect("write identity");
    fs::set_permissions(&identity, fs::Permissions::from_mode(0o600)).expect("chmod");
    let (_, out) = doctor(&env, &[]);
    assert!(line(&out, "warn", "age identity").is_none(), "{out}");

    fs::set_permissions(&identity, fs::Permissions::from_mode(0o644)).expect("chmod");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
    let warned = line(&out, "warn", "age identity").unwrap_or_default();
    assert!(
        warned.contains("mode 0644") && warned.contains("chmod 600"),
        "{out}"
    );
}

#[test]
fn the_old_store_and_an_old_dot_on_path_warn() {
    let mut env = healthy();
    let bin = env.root().join("fakebin");
    fs::create_dir_all(&bin).expect("mkdir");
    env.prepend_path(&bin);
    let dot = |body: &str| env.write_script("fakebin/dot", &format!("#!/bin/sh\n{body}\n"));

    dot("echo dot 0.2.0");
    let (_, out) = doctor(&env, &[]);
    assert!(line(&out, "warn", "legacy store").is_none(), "{out}");
    assert!(line(&out, "warn", "dot:").is_none(), "{out}");

    fs::create_dir_all(env.home().join(".dotman")).expect("mkdir");
    fs::write(env.home().join(".dotman/index.bin"), b"").expect("write index");
    dot("echo dot 0.0.1");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
    let store = line(&out, "warn", "legacy store").unwrap_or_default();
    assert!(
        store.contains("dotman import ") && store.contains(".dotman`"),
        "{out}"
    );
    let old = line(&out, "warn", "dot:").unwrap_or_default();
    assert!(old.contains("cargo uninstall dotman"), "{out}");

    dot("exec sleep 30");
    let started = std::time::Instant::now();
    let (_, out) = doctor(&env, &[]);
    assert!(started.elapsed().as_secs() < 20, "the probe is bounded");
    assert!(line(&out, "warn", "dot:").is_none(), "{out}");
}

#[test]
fn an_unapproved_or_changed_hook_table_warns() {
    let env = healthy();
    let (_, out) = doctor(&env, &[]);
    assert!(!out.contains("hooks"), "no table, no line: {out}");

    env.write_repo(".dotman.toml", b"[hooks]\n\"a\" = \"true\"\n");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
    let warned = line(&out, "warn", "hooks: not approved").unwrap_or_default();
    assert!(warned.contains("--approve-hooks"), "{out}");

    env.ok(&["apply", "--approve-hooks"]);
    let (_, out) = doctor(&env, &[]);
    assert!(line(&out, "pass", "hooks approved").is_some(), "{out}");

    env.write_repo(".dotman.toml", b"[hooks]\n\"a\" = \"false\"\n");
    let (_, out) = doctor(&env, &[]);
    assert!(
        line(&out, "warn", "hooks: the table changed").is_some(),
        "{out}"
    );

    env.write_repo(".dotman.toml", b"[hooks]\n\"a\" = \"x\\ry\"\n");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(line(&out, "fail", "control character").is_some(), "{out}");
}

#[test]
fn backups_are_counted_and_plaintext_copies_are_named() {
    let env = committed();
    env.write(".bashrc", b"local\n", 0o644);
    env.write_repo(".bashrc", b"repo\n");
    env.ok(&["apply", "--take-repo"]);
    let (_, out) = doctor(&env, &[]);
    assert!(line(&out, "info", " from 1 run, in ").is_some(), "{out}");
    assert!(!out.contains("plaintext"), "{out}");

    let run = env.state_dir().join("backups/20200101T000000Z-1/.dotman");
    fs::create_dir_all(&run).expect("mkdir");
    fs::write(run.join("age-identity.txt"), "AGE-SECRET-KEY-1QQQQ\n").expect("write");
    let (_, out) = doctor(&env, &[]);
    assert!(
        line(&out, "info", "files from 2 runs, in ").is_some(),
        "{out}"
    );
    let plain = line(&out, "info", "plaintext").unwrap_or_default();
    assert!(plain.starts_with("info  backups: 1 of them"), "{out}");
}

#[test]
fn a_missing_repo_fails_but_other_checks_still_run() {
    let env = TestEnv::new();
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(line(&out, "fail", "repo: not found").is_some(), "{out}");
    assert!(line(&out, "pass", "git identity").is_some(), "{out}");
}
