//! `dotman add`: the secret-name guard, explicit inputs it cannot store, dry-run exit codes and
//! directories that are already managed.

mod common;

use common::TestEnv;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn exits(env: &TestEnv, code: i32, args: &[&str]) -> String {
    let out = env.run(args);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(code), "dot {args:?}: {err}");
    err
}

fn managed(env: &TestEnv) -> Vec<String> {
    env.porcelain("list")
        .into_iter()
        .map(|(_, key)| key)
        .collect()
}

/// `~/.config/zsh` with an rc file and a token file, in a repo with no remote.
fn zsh(env: &TestEnv) {
    env.ok(&["init"]);
    env.write(".config/zsh/.zshrc", b"source secrets.zsh\n", 0o644);
    env.write(".config/zsh/secrets.zsh", b"export GH_TOKEN=x\n", 0o600);
}

#[test]
fn a_directory_holds_a_secret_named_file_without_a_remote() {
    let env = TestEnv::new();
    zsh(&env);
    let err = exits(&env, 3, &["add", ".config/zsh"]);
    assert!(
        err.contains("held .config/zsh/secrets.zsh: name matches [guard] secret_names"),
        "{err}"
    );
    assert!(
        err.contains("add --encrypt ~/.config/zsh/secrets.zsh")
            && err.contains("add ~/.config/zsh/secrets.zsh` stores it as it is"),
        "{err}"
    );
    assert_eq!(managed(&env), [".config/zsh/.zshrc"]);
    assert_eq!(
        env.token_of("status", ".config/zsh/secrets.zsh").as_deref(),
        Some("held")
    );
    exits(&env, 3, &["sync"]);
    assert!(!env.repo().join(".config/zsh/secrets.zsh").exists());

    // A second add of the directory holds it again; `--allow-plain` takes it.
    exits(&env, 3, &["add", ".config/zsh"]);
    env.ok(&["add", "--allow-plain", ".config/zsh"]);
    assert!(managed(&env).contains(&".config/zsh/secrets.zsh".to_owned()));
}

#[test]
fn an_explicit_secret_named_file_is_added_with_one_warning() {
    let env = TestEnv::new();
    zsh(&env);
    let err = env.ok(&["add", ".config/zsh/secrets.zsh"]);
    assert_eq!(
        err.matches("secret_names").count(),
        1,
        "one warning:\n{err}"
    );
    assert!(
        err.contains("is stored as plain text")
            && err.contains("add --encrypt ~/.config/zsh/secrets.zsh"),
        "{err}"
    );
    assert_eq!(managed(&env), [".config/zsh/secrets.zsh"]);

    // Once managed, adding its directory no longer holds it.
    let err = env.ok(&["add", ".config/zsh"]);
    assert!(!err.contains("held"), "{err}");
    let err = env.ok(&["add", "--allow-plain", ".config/zsh/secrets.zsh"]);
    assert!(!err.contains("secret_names"), "{err}");
}

#[test]
fn an_explicit_input_that_cannot_be_stored_exits_1_and_adds_nothing() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"x\n", 0o644);
    env.symlink(".dangling", "/nonexistent/target");
    let made = Command::new("mkfifo")
        .arg(env.home().join(".fifo"))
        .status()
        .expect("run mkfifo");
    assert!(made.success());
    env.write(".locked", b"secret\n", 0o000);

    let mut cases = vec![
        (".dangling", "dangling symlink"),
        (".fifo", "socket, FIFO or device"),
    ];
    // root reads a 0000 file, so the case means nothing there.
    if fs::read(env.home().join(".locked")).is_err() {
        cases.push((".locked", "unreadable"));
    }
    for (input, why) in cases {
        for dry_run in [false, true] {
            let mut args = vec!["add", ".bashrc", input];
            if dry_run {
                args.push("--dry-run");
            }
            let err = env.fails(&args);
            assert!(err.contains(why), "{input}: {err}");
        }
    }
    assert_eq!(managed(&env), Vec::<String>::new());
}

#[test]
fn a_file_named_like_a_dotman_temp_file_is_refused_with_the_reason() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".dotman-tmp-mine", b"x\n", 0o644);
    let err = env.fails(&["add", ".dotman-tmp-mine"]);
    assert!(
        err.contains(
            ".dotman-tmp-mine: names starting with .dotman-tmp- are dotman's temporary files"
        ),
        "{err}"
    );
    assert_eq!(managed(&env), Vec::<String>::new());
}

#[test]
fn a_dry_run_exits_as_the_real_run_would() {
    let env = TestEnv::new();
    zsh(&env);
    exits(&env, 3, &["add", "--dry-run", ".config/zsh"]);
    assert_eq!(managed(&env), Vec::<String>::new());

    env.write(".config/age.txt", b"AGE-SECRET-KEY-1QQQQ\n", 0o600);
    let err = exits(&env, 1, &["add", "--dry-run", ".config/age.txt"]);
    assert!(err.contains("could not add .config/age.txt"), "{err}");
    exits(&env, 1, &["add", ".config/age.txt"]);
    exits(&env, 0, &["add", "--dry-run", ".config/zsh/.zshrc"]);
}

#[test]
fn a_parent_directory_mode_is_recorded_once_and_never_replaced() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/x/a", b"a\n", 0o644);
    fs::set_permissions(
        env.home().join(".config/x"),
        fs::Permissions::from_mode(0o700),
    )
    .expect("chmod");
    env.ok(&["add", ".config/x/a"]);
    let dir_mode = |key: &str| env.repo_config().modes.get(key).cloned();
    assert_eq!(dir_mode(".config/x/").as_deref(), Some("0700"));

    // Another machine's umask gives 0755 here; its capture keeps the recorded entry.
    fs::set_permissions(
        env.home().join(".config/x"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");
    env.write(".config/x/a", b"edited\n", 0o644);
    env.write(".config/x/b", b"b\n", 0o644);
    env.ok(&["add", ".config/x/a", ".config/x/b"]);
    assert_eq!(dir_mode(".config/x/").as_deref(), Some("0700"));
    assert_eq!(dir_mode(".config/"), None);
}

#[test]
fn a_symlink_to_a_directory_is_stored_as_a_link_with_a_hint() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write("dots/nvim/init.lua", b"-- init\n", 0o644);
    env.symlink(".config/nvim", "../dots/nvim");
    let err = env.ok(&["add", ".config/nvim"]);
    assert!(
        err.contains("only the link is stored, not the files in it")
            && err.contains("add ~/dots/nvim"),
        "{err}"
    );
    assert_eq!(
        env.token_of("list", ".config/nvim").as_deref(),
        Some("symlink")
    );
}
