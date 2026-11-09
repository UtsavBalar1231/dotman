//! `dotman add`: the secret-name guard, explicit inputs it cannot store, dry-run exit codes and
//! directories that are already managed.

mod common;

use common::TestEnv;

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
