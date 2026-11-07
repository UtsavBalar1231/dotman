//! Identity comes from `DOTMAN_HOST`, `DOTMAN_PROFILES` and the machine config, so the detected
//! identity of the machine running the tests never decides an outcome.

mod common;

use common::TestEnv;
use dotman::state::State;
use std::fs;
use std::process::Output;

fn dot(env: &TestEnv, vars: &[(&str, &str)], args: &[&str]) -> Output {
    env.run_with(vars, args)
}

/// Asserts the exit code and returns stderr.
fn exits(out: &Output, code: i32) -> String {
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(code), "{err}");
    err
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn token(env: &TestEnv, vars: &[(&str, &str)], key: &str) -> Option<String> {
    env.porcelain_with(vars, "status")
        .into_iter()
        .find(|(_, k)| k == key)
        .map(|(token, _)| token)
}

// An unreadable state reads as no snapshot, which the assertions on the host then reject.
fn snapshot_host(env: &TestEnv) -> Option<String> {
    State::read(&env.state_dir().join("state.toml"))
        .ok()
        .flatten()
        .and_then(|state| state.identity)
        .map(|identity| identity.host)
}

#[test]
fn dotman_profiles_work_selects_the_profile_variant() {
    let key = ".config/app/x.conf";
    let make = || {
        let env = TestEnv::new();
        env.ok(&["init"]);
        env.write_repo(key, b"plain\n");
        env.write_repo(".config/app/x{profile=work}.conf", b"work\n");
        env
    };
    let work = make();
    let profiles = [("DOTMAN_PROFILES", "work")];
    exits(&dot(&work, &profiles, &["apply"]), 0);
    assert_eq!(work.read(key), b"work\n");
    assert_eq!(token(&work, &profiles, key).as_deref(), Some("clean"));
    let listed = stdout(&dot(&work, &profiles, &["list"]));
    assert!(
        listed.contains(".config/app/x.conf  (.config/app/x{profile=work}.conf)"),
        "{listed}"
    );

    let plain = make();
    exits(&dot(&plain, &[], &["apply"]), 0);
    assert_eq!(plain.read(key), b"plain\n");
}

#[test]
fn a_changed_host_waits_for_accept_identity_then_applies_its_variant() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let config = env.xdg_config().join("dotman");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("config.toml"), "[identity]\nuser = \"u\"\n").unwrap();
    let key = ".config/m.conf";
    env.write_repo(key, b"plain\n");
    env.write_repo(".config/m{host=a}.conf", b"a\n");
    env.write_repo(".config/m{host=b}.conf", b"b\n");
    let (a, b) = ([("DOTMAN_HOST", "a")], [("DOTMAN_HOST", "b")]);

    let before = env.four_trees();
    exits(&dot(&env, &a, &["apply", "--dry-run"]), 0);
    assert_eq!(env.four_trees(), before);
    exits(&dot(&env, &a, &["apply"]), 0);
    assert_eq!(env.read(key), b"a\n");
    assert_eq!(snapshot_host(&env).as_deref(), Some("a"));

    assert_eq!(token(&env, &b, key).as_deref(), Some("identity-changed"));
    let err = exits(&dot(&env, &b, &["apply"]), 3);
    assert!(err.contains("host a -> b"), "{err}");
    assert!(err.contains("--accept-identity"), "{err}");
    assert_eq!(env.read(key), b"a\n");
    let before = env.four_trees();
    exits(
        &dot(&env, &b, &["apply", "--accept-identity", "--dry-run"]),
        0,
    );
    assert_eq!(env.four_trees(), before);
    assert_eq!(snapshot_host(&env).as_deref(), Some("a"));

    exits(&dot(&env, &b, &["apply", "--accept-identity"]), 0);
    assert_eq!(env.read(key), b"b\n");
    assert_eq!(snapshot_host(&env).as_deref(), Some("b"));
    assert_eq!(token(&env, &b, key).as_deref(), Some("clean"));

    // A more specific variant arriving with the same identity is applied without asking.
    env.write_repo(".config/m{host=b+user=u}.conf", b"b and u\n");
    assert_eq!(token(&env, &b, key).as_deref(), Some("changed-in-repo"));
    exits(&dot(&env, &b, &["apply"]), 0);
    assert_eq!(env.read(key), b"b and u\n");
}

#[test]
fn a_local_edit_is_captured_into_the_variant_it_came_from() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let me = [("DOTMAN_HOST", "me")];
    let key = ".config/x.conf";
    let plain = env.repo().join(key);
    let variant = env.repo().join(".config/x{host=me}.conf");
    env.write(key, b"v1\n", 0o600);

    let err = exits(&dot(&env, &me, &["add", "--for", "host=me", key]), 0);
    assert!(
        err.contains("add .config/x.conf as .config/x{host=me}.conf"),
        "{err}"
    );
    assert!(!plain.exists());
    assert_eq!(fs::read(&variant).unwrap(), b"v1\n");
    let modes = env.repo_config().modes;
    assert_eq!(
        modes.get(".config/x{host=me}.conf").map(String::as_str),
        Some("0600")
    );
    assert_eq!(token(&env, &me, key).as_deref(), Some("clean"));

    env.write(key, b"v2\n", 0o600);
    assert_eq!(token(&env, &me, key).as_deref(), Some("changed-locally"));
    exits(&dot(&env, &me, &["sync"]), 0);
    assert_eq!(fs::read(&variant).unwrap(), b"v2\n");
    assert!(!plain.exists());
    assert_eq!(token(&env, &me, key).as_deref(), Some("clean"));
    let repo = env.repo().to_string_lossy().into_owned();
    let subject = env.git(&["-C", &repo, "log", "-1", "--format=%s"]);
    assert!(subject.starts_with("sync(me): "), "{subject}");

    env.write(key, b"v3\n", 0o600);
    exits(&dot(&env, &me, &["add", key]), 0);
    assert_eq!(fs::read(&variant).unwrap(), b"v3\n");
    assert!(!plain.exists());
}

#[test]
fn a_variant_for_another_host_leaves_this_one_alone_and_forget_removes_both() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let me = [("DOTMAN_HOST", "me")];
    env.write(".zshrc", b"mine\n", 0o644);
    exits(&dot(&env, &me, &["add", ".zshrc"]), 0);
    let err = exits(
        &dot(&env, &me, &["add", "--for", "host=other", ".zshrc"]),
        0,
    );
    assert!(err.contains("does not apply .zshrc{host=other}"), "{err}");
    assert_eq!(
        fs::read(env.repo().join(".zshrc{host=other}")).unwrap(),
        b"mine\n"
    );
    assert_eq!(token(&env, &me, ".zshrc").as_deref(), Some("clean"));
    let other = [("DOTMAN_HOST", "other")];
    assert_eq!(
        token(&env, &other, ".zshrc").as_deref(),
        Some("identity-changed")
    );

    for bad in ["tmpl", "host=a+age", "host", "bogus=1"] {
        let err = exits(&dot(&env, &me, &["add", "--for", bad, ".zshrc"]), 1);
        assert!(err.contains("is not a list of conditions"), "{bad}: {err}");
    }
    env.write(".config/d/a", b"a\n", 0o644);
    let err = exits(
        &dot(&env, &me, &["add", "--for", "host=me", ".config/d"]),
        1,
    );
    assert!(err.contains("--for takes files"), "{err}");

    exits(&dot(&env, &me, &["forget", ".zshrc"]), 0);
    assert!(!env.repo().join(".zshrc").exists());
    assert!(!env.repo().join(".zshrc{host=other}").exists());
    assert_eq!(env.read(".zshrc"), b"mine\n");
}

#[test]
fn a_variant_target_under_a_managed_dir_is_never_new() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let me = [("DOTMAN_HOST", "me")];
    let key = ".config/hypr/monitors.conf";
    env.write(".config/hypr/hyprland.conf", b"main\n", 0o644);
    exits(&dot(&env, &me, &["add", ".config/hypr"]), 0);
    env.write(key, b"laptop\n", 0o644);
    assert_eq!(token(&env, &me, key).as_deref(), Some("new"));

    exits(&dot(&env, &me, &["add", "--for", "host=me", key]), 0);
    assert_eq!(token(&env, &me, key).as_deref(), Some("clean"));
    exits(&dot(&env, &me, &["sync"]), 0);
    assert!(!env.repo().join(key).exists());
    assert!(
        env.repo()
            .join(".config/hypr/monitors{host=me}.conf")
            .exists()
    );

    let other = [("DOTMAN_HOST", "other")];
    assert_eq!(
        token(&env, &other, key).as_deref(),
        Some("identity-changed")
    );
}

#[test]
fn x_beside_x_tmpl_is_ambiguous_and_skipped() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write_repo("x", b"plain\n");
    env.write_repo("x{tmpl}", b"template\n");
    env.write_repo("y", b"y\n");
    assert_eq!(token(&env, &[], "x").as_deref(), Some("ambiguous"));

    let err = exits(&env.run(&["apply"]), 3);
    assert!(err.contains("x: x, x{tmpl} are equally specific"), "{err}");
    assert!(!env.home().join("x").exists());
    assert_eq!(env.read("y"), b"y\n");
    let err = exits(&env.run(&["diff"]), 0);
    assert!(err.contains("x: ambiguous, not compared"), "{err}");

    let doctor = stdout(&env.run(&["doctor"]));
    assert!(
        doctor
            .lines()
            .any(|l| l.starts_with("warn") && l.contains("variants: x: x, x{tmpl}")),
        "{doctor}"
    );
}

#[test]
fn tmpl_with_age_is_invalid_and_never_applied() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    let key = ".config/kitty/kitty.conf";
    env.write_repo(key, b"plain\n");
    env.write_repo(".config/kitty/kitty{tmpl+age}.conf", b"ciphertext\n");
    assert_eq!(token(&env, &[], key).as_deref(), Some("invalid"));

    let err = exits(&env.run(&["apply"]), 3);
    assert!(err.contains("at most one of `tmpl` and `age`"), "{err}");
    assert!(!env.home().join(key).exists());

    let out = env.run(&["doctor"]);
    let doctor = stdout(&out);
    assert!(
        doctor.lines().any(|l| l.starts_with("warn")
            && l.contains("kitty{tmpl+age}.conf: a block takes at most one")),
        "{doctor}"
    );
    assert!(!doctor.lines().any(|l| l.starts_with("fail")), "{doctor}");
}

#[test]
fn brace_names_that_are_not_blocks_are_ordinary_files() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let xpi = ".mozilla/ext/{ca546566-e370-4cde-98d3-acc110c5967d}.xpi";
    env.write(xpi, b"zip\n", 0o644);
    env.write("a{b}.conf", b"ab\n", 0o644);
    env.ok(&["add", xpi, "a{b}.conf"]);
    let keys: Vec<String> = env.porcelain("list").into_iter().map(|(_, k)| k).collect();
    assert_eq!(keys, [xpi, "a{b}.conf"]);
    for key in &keys {
        assert_eq!(
            env.token_of("status", key).as_deref(),
            Some("clean"),
            "{key}"
        );
    }

    env.write("x{os=linux}", b"x\n", 0o644);
    let err = env.fails(&["add", "x{os=linux}"]);
    assert!(err.contains("holds a variant block"), "{err}");
    assert!(!env.repo().join("x{os=linux}").exists());
}
