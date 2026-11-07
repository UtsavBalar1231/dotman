mod common;

use common::{TestEnv, assert_converged, mode_of};
use std::fs;

const TWO_MIB: usize = 2_097_152;

fn exits(env: &TestEnv, code: i32, args: &[&str]) -> String {
    let out = env.run(args);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(code), "dot {args:?}: {err}");
    err
}

/// A device with `.config/nvim` and `.config/gh` managed as directories, synced once.
fn device() -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".config/nvim/init.lua", b"-- init\n", 0o644);
    env.write(".config/gh/config.yml", b"git_protocol: ssh\n", 0o644);
    env.ok(&["add", ".config/nvim", ".config/gh"]);
    exits(&env, 0, &["sync"]);
    let remote = env.remote_path().to_string_lossy().into_owned();
    env.git(&[
        "--git-dir",
        &remote,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    env
}

fn write_held(env: &TestEnv) {
    env.write(".config/nvim/big.dat", &vec![b'x'; TWO_MIB], 0o644);
    env.write(".config/nvim/blob.bin", &[1, 0, 2, 3], 0o644);
    env.write(".config/gh/hosts.yml", b"github.com:\n", 0o600);
}

fn remote_tree(env: &TestEnv) -> Vec<String> {
    let remote = env.remote_path().to_string_lossy().into_owned();
    env.git(&["--git-dir", &remote, "ls-tree", "-r", "--name-only", "main"])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn tokens(env: &TestEnv) -> Vec<(String, String)> {
    env.porcelain("status")
        .into_iter()
        .filter(|(token, _)| token != "clean")
        .collect()
}

#[test]
fn a_new_file_under_a_managed_dir_reaches_the_other_device() {
    let a = device();
    let b = a.second_device();
    b.ok(&["init", &b.remote_url(), "--take-repo"]);

    a.write(".config/nvim/lua/plugins/foo.lua", b"return {}\n", 0o600);
    assert_eq!(
        a.token_of("status", ".config/nvim/lua/plugins/foo.lua")
            .as_deref(),
        Some("new")
    );
    exits(&a, 0, &["sync"]);
    exits(&b, 0, &["sync"]);

    let foo = b.home().join(".config/nvim/lua/plugins/foo.lua");
    assert_eq!(fs::read(&foo).expect("foo.lua on b"), b"return {}\n");
    assert_eq!(mode_of(&foo), 0o600);
    assert_converged(&a, &b);
    assert_eq!(
        a.token_of("status", ".config/nvim/lua/plugins/foo.lua")
            .as_deref(),
        Some("clean")
    );
}

#[test]
fn big_binary_and_secret_named_files_are_held_and_sync_exits_3_but_pushes_the_rest() {
    let a = device();
    write_held(&a);
    a.write(".config/nvim/foo.lua", b"return {}\n", 0o644);

    let held = [
        ".config/gh/hosts.yml",
        ".config/nvim/big.dat",
        ".config/nvim/blob.bin",
    ];
    let status = tokens(&a);
    for key in held {
        assert!(
            status.contains(&("held".to_owned(), key.to_owned())),
            "{status:?}"
        );
    }
    assert!(status.contains(&("new".to_owned(), ".config/nvim/foo.lua".to_owned())));
    let human = String::from_utf8_lossy(&a.run(&["status"]).stdout).into_owned();
    assert!(human.contains("dot add <file>"), "{human}");
    assert!(
        human.contains("hosts.yml (name matches [guard] secret_names)"),
        "{human}"
    );
    assert!(human.contains("big.dat (over 1 MiB)"), "{human}");
    assert!(human.contains("blob.bin (binary"), "{human}");

    let before = (a.four_trees(), a.remote_refs());
    let err = exits(&a, 3, &["sync", "--dry-run"]);
    assert!(err.contains("would capture .config/nvim/foo.lua"), "{err}");
    assert!(err.contains(".config/gh/hosts.yml"), "{err}");
    assert_eq!((a.four_trees(), a.remote_refs()), before);

    let err = exits(&a, 3, &["sync"]);
    assert!(err.contains("3 new files held back"), "{err}");
    let tree = remote_tree(&a);
    assert!(
        tree.contains(&".config/nvim/foo.lua".to_owned()),
        "{tree:?}"
    );
    for key in held {
        assert!(!tree.contains(&key.to_owned()), "{key} was pushed");
    }
    assert_eq!(exits(&a, 3, &["sync"]).matches("held back").count(), 1);
}

#[test]
fn dot_add_releases_a_held_file() {
    let a = device();
    write_held(&a);
    exits(&a, 3, &["sync"]);

    a.ok(&["add", ".config/gh/hosts.yml"]);
    assert_eq!(
        a.token_of("status", ".config/gh/hosts.yml").as_deref(),
        Some("clean")
    );
    exits(&a, 3, &["sync"]);
    assert!(remote_tree(&a).contains(&".config/gh/hosts.yml".to_owned()));
    assert_eq!(
        a.token_of("status", ".config/nvim/big.dat").as_deref(),
        Some("held")
    );
}

#[test]
fn nested_git_dirs_are_never_listed_and_an_ignore_pattern_hides_a_file() {
    let a = device();
    a.write(".config/nvim/plugin/.git/config", b"[core]\n", 0o644);
    a.write(".config/nvim/plugin/init.lua", b"-- p\n", 0o644);
    a.write(".config/nvim/scratch.txt", b"scratch\n", 0o644);
    a.write(".config/nvim/.dotman-tmp-x", b"tmp\n", 0o644);
    a.symlink(".config/nvim/dangling", "nowhere");

    let keys =
        |env: &TestEnv| -> Vec<String> { tokens(env).into_iter().map(|(_, key)| key).collect() };
    assert_eq!(
        keys(&a),
        [".config/nvim/plugin/init.lua", ".config/nvim/scratch.txt"]
    );
    let err = a.ok(&["status"]);
    assert!(err.contains("dangling: dangling symlink"), "{err}");

    let mut config = a.repo_config();
    config.ignore.patterns.push("**/scratch.txt".to_owned());
    config
        .save(&a.repo().join(".dotman.toml"), false)
        .expect("save .dotman.toml");
    assert_eq!(keys(&a), [".config/nvim/plugin/init.lua"]);

    exits(&a, 0, &["sync"]);
    let tree = remote_tree(&a);
    assert!(tree.contains(&".config/nvim/plugin/init.lua".to_owned()));
    assert!(
        !tree.iter().any(|k| k.contains(".git/")
            || k.contains("scratch")
            || k.contains("dangling")
            || k.contains("dotman-tmp")),
        "{tree:?}"
    );
}
