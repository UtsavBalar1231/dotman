//! `dotman add`: the secret-name guard, explicit inputs it cannot store, dry-run exit codes and
//! directories that are already managed.

mod common;

use common::TestEnv;
use dotman::state::{RemotePublic, State};
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
    let secrets = env.home().join(".config/zsh/secrets.zsh");
    assert!(
        err.contains(&format!("add --encrypt {}", secrets.display()))
            && err.contains(&format!("add {}` stores it as it is", secrets.display())),
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
fn another_host_s_encrypted_variant_does_not_let_a_plain_secret_in() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/app/rc", b"rc\n", 0o644);
    env.write(".config/app/.env", b"API_KEY=hunter2\n", 0o600);
    // What `add --encrypt --for host=laptop` leaves: the only source is another host's.
    env.write_repo(".config/app/.env{host=laptop+age}", b"ciphertext\n");

    let err = exits(&env, 3, &["add", "--dry-run", ".config/app"]);
    assert!(err.contains("held .config/app/.env"), "{err}");
    let err = exits(&env, 3, &["add", ".config/app"]);
    assert!(err.contains("held .config/app/.env"), "{err}");
    assert!(!env.repo().join(".config/app/.env").exists());
    assert!(env.repo().join(".config/app/rc").exists());
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
    let secrets = env.home().join(".config/zsh/secrets.zsh");
    assert!(
        err.contains("is stored as plain text")
            && err.contains(&format!("add --encrypt {}`", secrets.display())),
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

    let key = format!("AGE-SECRET-KEY-1{}\n", "Q".repeat(58));
    env.write(".config/age.txt", key.as_bytes(), 0o600);
    let err = exits(&env, 1, &["add", "--dry-run", ".config/age.txt"]);
    assert!(
        err.contains(".config/age.txt: line 1 holds an age secret key"),
        "{err}"
    );
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
fn adding_a_managed_directory_again_previews_only_what_changed() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/app/a.conf", b"a\n", 0o644);
    env.write(".config/app/b.conf", b"b\n", 0o644);
    env.ok(&["add", ".config/app"]);

    let err = env.ok(&["add", ".config/app"]);
    assert!(
        err.contains(".config/app/: 2 files already managed and unchanged"),
        "{err}"
    );
    assert!(!err.contains("total size"), "no preview:\n{err}");

    env.write(".config/app/b.conf", b"b edited\n", 0o644);
    env.write(".config/app/c.conf", b"c\n", 0o644);
    let err = env.ok(&["add", ".config/app"]);
    assert!(
        err.contains(".config/app/: 1 file already managed and unchanged")
            && err.contains("files: 2"),
        "{err}"
    );
    assert_eq!(
        managed(&env),
        [
            ".config/app/a.conf",
            ".config/app/b.conf",
            ".config/app/c.conf"
        ]
    );
    assert_eq!(
        fs::read(env.repo().join(".config/app/b.conf")).expect("read"),
        b"b edited\n"
    );
}

#[test]
fn a_symlink_to_a_directory_is_stored_as_a_link_with_a_hint() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write("my dots/nvim/init.lua", b"-- init\n", 0o644);
    env.symlink(".config/nvim", "../my dots/nvim");
    let err = env.ok(&["add", ".config/nvim"]);
    let target = env.home().join("my dots/nvim");
    assert!(
        err.contains("only the link is stored, not the files in it")
            && err.contains(&format!("add '{}'`", target.display())),
        "{err}"
    );
    assert_eq!(
        env.token_of("list", ".config/nvim").as_deref(),
        Some("symlink")
    );
}

#[test]
fn hold_hints_are_commands_that_work_when_pasted() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/my app/.env", b"KEY=1\n", 0o600);
    env.write(".config/my app/rc", b"rc\n", 0o644);
    env.write("elsewhere", b"x\n", 0o644);
    env.symlink(".config/links/.env", "../../elsewhere");
    let err = exits(&env, 3, &["add", ".config/my app", ".config/links"]);
    let quoted = format!("'{}'", env.home().join(".config/my app/.env").display());
    assert!(
        err.contains(&format!("add --encrypt {quoted}` stores it encrypted")),
        "{err}"
    );
    let link_line = err
        .lines()
        .find(|line| line.contains("held .config/links/.env"))
        .expect("the link is held");
    assert!(
        !link_line.contains("--encrypt") && link_line.contains("stores the link as it is"),
        "{link_line}"
    );

    env.write(".config/app/.env", b"KEY=2\n", 0o600);
    let err = env.ok(&["add", "--for", "host=laptop", ".config/app/.env"]);
    assert!(
        err.contains(&format!(
            "add --encrypt --for 'host=laptop' {}`",
            env.home().join(".config/app/.env").display()
        )),
        "{err}"
    );
}

#[test]
fn a_directory_with_entries_it_cannot_store_exits_3_in_both_runs() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/x/rc", b"rc\n", 0o644);
    let made = Command::new("mkfifo")
        .arg(env.home().join(".config/x/pipe"))
        .status()
        .expect("run mkfifo");
    assert!(made.success());
    for args in [
        &["add", "--dry-run", ".config/x"][..],
        &["add", ".config/x"],
    ] {
        let err = exits(&env, 3, args);
        assert!(
            err.contains("held .config/x/pipe: a socket, FIFO or device"),
            "{err}"
        );
    }

    env.write(".config/locked/a", b"a\n", 0o644);
    let locked = env.home().join(".config/locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
    let readable = fs::read_dir(&locked).is_ok();
    let outs: Vec<(Option<i32>, String)> = [
        &["add", "--dry-run", ".config/locked"][..],
        &["add", ".config/locked"],
    ]
    .iter()
    .map(|args| {
        let out = env.run(args);
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    })
    .collect();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod");
    // root reads anything, so the case means nothing there.
    if !readable {
        for (code, err) in outs {
            assert_eq!(code, Some(3), "{err}");
            assert!(err.contains("held .config/locked: cannot be read"), "{err}");
        }
    }
}

#[test]
fn a_dry_run_refuses_a_symlinked_repo_parent_as_the_real_run_does() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let outside = env.root().join("outside");
    fs::create_dir(&outside).expect("create dir");
    std::os::unix::fs::symlink(&outside, env.repo().join(".config")).expect("link");
    env.write(".config/x", b"secret", 0o644);
    for args in [
        &["add", "--dry-run", ".config/x"][..],
        &["add", ".config/x"],
    ] {
        let err = env.fails(args);
        assert!(
            err.contains("could not add .config/x") && err.contains("is a symlink or a file"),
            "{err}"
        );
    }
    assert_eq!(fs::read_dir(&outside).expect("read").count(), 0);
}

/// An unknown visibility an hour old is checked again before the secret-name guard: once the
/// probe finds the remote public, an explicit secret-named file is refused.
#[test]
fn an_unknown_visibility_an_hour_old_is_checked_again_before_the_guard() {
    let mut env = TestEnv::new();
    env.ok(&["init", "--remote", "https://example.invalid/me/dots.git"]);
    let path = env.state_dir().join("state.toml");
    let mut state = State::read(&path).expect("read").expect("state");
    assert_eq!(state.remote_public, Some(RemotePublic::Unknown));
    state.remote_checked_at = state.remote_checked_at.map(|at| at.saturating_sub(7200));
    state.save(&path, false).expect("save");

    // `ls-remote` answers without credentials, as a public remote does.
    let real = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH");
    let stubs = env.root().join("stubs");
    fs::create_dir(&stubs).expect("stub dir");
    let stub = stubs.join("git");
    let script = format!(
        "#!/bin/sh\ncase \" $* \" in *\" ls-remote \"*) exit 0;; esac\nexec \"{}\" \"$@\"\n",
        real.display()
    );
    fs::write(&stub, script).expect("write stub");
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).expect("chmod");
    env.prepend_path(&stubs);

    env.write(".env", b"X=1\n", 0o600);
    let err = exits(&env, 1, &["add", ".env"]);
    assert!(err.contains("the remote is public"), "{err}");
    assert_eq!(managed(&env), Vec::<String>::new());
}

/// The directory preview is what the confirmation approves, so `-q` does not hide it.
#[test]
fn quiet_still_shows_the_directory_preview() {
    let env = TestEnv::new();
    zsh(&env);
    let err = exits(&env, 3, &["-q", "add", "--yes", ".config/zsh"]);
    assert!(err.contains(".config/zsh/\n  files: 1\n"), "{err}");
    assert!(err.contains("  files at mode 0600: 0"), "{err}");
}

/// `[ignore]` keeps a path out of a directory add; a path named on the command line goes in,
/// with a warning that names the pattern.
#[test]
fn an_ignored_path_goes_in_only_when_named() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/vim/vimrc", b"set nu\n", 0o644);
    env.write(".config/vim/.vimrc.swp", b"swap\n", 0o644);
    let err = exits(&env, 0, &["add", "--yes", ".config/vim"]);
    assert!(!err.contains("matches [ignore]"), "{err}");
    assert_eq!(managed(&env), [".config/vim/vimrc"]);

    let err = exits(&env, 0, &["add", ".config/vim/.vimrc.swp"]);
    assert!(
        err.contains(
            ".config/vim/.vimrc.swp: matches [ignore] **/*.swp; added because you named it"
        ),
        "{err}"
    );
    // Whether `list` then shows it is the index's business, not the add's.
    assert_eq!(
        fs::read(env.repo().join(".config/vim/.vimrc.swp")).expect("stored"),
        b"swap\n"
    );
}
