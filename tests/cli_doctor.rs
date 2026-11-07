mod common;

use common::TestEnv;
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

#[test]
fn a_missing_repo_fails_but_other_checks_still_run() {
    let env = TestEnv::new();
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(line(&out, "fail", "repo: not found").is_some(), "{out}");
    assert!(line(&out, "pass", "git identity").is_some(), "{out}");
}
