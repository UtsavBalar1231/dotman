mod common;

use common::{TestEnv, mode_of, tree_checksum};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const REMOTE_URL: &str = "https://example.invalid/dots.git";

#[test]
fn init_twice_leaves_every_tree_unchanged() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    assert!(env.repo().join(".git/dotman-id").is_file());
    assert!(env.repo().join(".dotman.toml").is_file());
    let before = env.four_trees();
    let config_before = tree_checksum(&env.xdg_config());
    env.ok(&["init"]);
    assert_eq!(env.four_trees(), before);
    assert_eq!(tree_checksum(&env.xdg_config()), config_before);
}

#[test]
fn init_remote_sets_and_replaces_origin() {
    let env = TestEnv::new();
    let repo_arg = env.repo().to_string_lossy().into_owned();
    let origin = || {
        env.git(&["-C", &repo_arg, "config", "--get", "remote.origin.url"])
            .trim()
            .to_owned()
    };
    env.ok(&["init", "--remote", REMOTE_URL]);
    assert_eq!(origin(), REMOTE_URL);
    let other = "https://example.invalid/other.git";
    env.ok(&["init", "--remote", other]);
    assert_eq!(origin(), other);
    env.fails(&["init", "--remote=--upload-pack=x"]);
}

#[test]
fn init_dry_run_creates_nothing() {
    let env = TestEnv::new();
    let before = env.four_trees();
    let err = env.ok(&["init", "--dry-run", "--remote", REMOTE_URL]);
    assert!(err.contains("would create the repo"), "{err}");
    assert_eq!(env.four_trees(), before);
    assert!(!env.repo().exists() && !env.state_dir().exists());
}

#[test]
fn directory_preview_counts_match_the_fixture() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"x", 0o644);
    env.write(".config/app/a.conf", &[b'a'; 10], 0o644);
    env.write(".config/app/b.conf", &[b'b'; 20], 0o644);
    env.write(".config/app/private.txt", &[b'p'; 5], 0o600);
    let mut blob = vec![b'z'; 100];
    blob.insert(0, 0);
    blob.truncate(100);
    env.write(".config/app/blob.bin", &blob, 0o644);
    env.write(".config/app/sub/c.conf", &[b'c'; 30], 0o644);
    env.write(".config/app/sub/big.dat", &[b'd'; 300], 0o644);
    env.write(".config/app/sub/editor.swp", b"ignored", 0o644);
    env.symlink(".config/app/dangling", "/nonexistent/target");
    env.symlink(
        ".config/app/to-bashrc",
        env.home().join(".bashrc").as_path(),
    );

    let err = env.ok(&["add", ".config/app"]);
    for expected in [
        "files: 7",
        "total size: 465 bytes",
        "binary files: 1",
        "files at mode 0600: 1",
        "skipped: 1",
        ".config/app/dangling: dangling symlink",
        "300 bytes  .config/app/sub/big.dat",
        "100 bytes  .config/app/blob.bin",
        "10 bytes  .config/app/a.conf",
    ] {
        assert!(err.contains(expected), "missing `{expected}` in:\n{err}");
    }
    assert!(!err.contains("5 bytes  .config/app/private.txt"), "{err}");
    assert!(
        !err.contains("editor.swp"),
        "ignored files stay out:\n{err}"
    );
    assert_eq!(env.repo_config().dirs.paths, [".config/app"]);
    assert_eq!(
        env.porcelain("list").len(),
        7,
        "the seven previewed entries are managed"
    );
}

#[test]
fn absolute_symlink_targets_become_relative_and_a_dangling_link_is_refused() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"x", 0o644);
    env.symlink(".config/inside", env.home().join(".bashrc").as_path());
    env.symlink(".config/outside", "/etc/hostname");
    env.symlink(".config/relative", "../.bashrc");
    env.symlink("dangling", "/nonexistent/target");

    env.ok(&[
        "add",
        ".config/inside",
        ".config/outside",
        ".config/relative",
    ]);
    let err = env.fails(&["add", "dangling"]);
    assert!(err.contains("dangling symlink"), "{err}");

    let target = |key: &str| fs::read_link(env.repo().join(key)).expect("read_link");
    assert_eq!(target(".config/inside"), Path::new("../.bashrc"));
    assert_eq!(target(".config/outside"), Path::new("/etc/hostname"));
    assert_eq!(target(".config/relative"), Path::new("../.bashrc"));
    assert!(fs::symlink_metadata(env.repo().join("dangling")).is_err());
    assert_eq!(
        env.token_of("list", ".config/inside").as_deref(),
        Some("symlink")
    );
    for key in [".config/inside", ".config/outside", ".config/relative"] {
        assert_eq!(
            env.token_of("status", key).as_deref(),
            Some("clean"),
            "{key}"
        );
    }
}

#[test]
fn private_file_and_directory_modes_are_recorded() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".ssh/config", b"Host *\n", 0o600);
    fs::set_permissions(env.home().join(".ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    env.write(".local/bin/run", b"#!/bin/sh\n", 0o755);
    env.write(".profile", b"x", 0o644);

    env.ok(&["add", ".ssh/config", ".local/bin/run", ".profile"]);
    let modes = env.repo_config().modes;
    assert_eq!(modes.get(".ssh/config").map(String::as_str), Some("0600"));
    assert_eq!(modes.get(".ssh/").map(String::as_str), Some("0700"));
    assert!(!modes.contains_key(".local/bin/run"), "{modes:?}");
    assert!(!modes.contains_key(".profile"), "{modes:?}");
    assert_eq!(mode_of(&env.repo().join(".local/bin/run")), 0o755);
    assert_eq!(mode_of(&env.repo().join(".ssh/config")), 0o644);
    for key in [".ssh/config", ".local/bin/run", ".profile"] {
        assert_eq!(
            env.token_of("status", key).as_deref(),
            Some("clean"),
            "{key}"
        );
    }
}

#[test]
fn forget_keeps_the_file_in_home() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"export A=1\n", 0o644);
    env.write(".ssh/config", b"Host *\n", 0o600);
    fs::set_permissions(env.home().join(".ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    env.ok(&["add", ".bashrc", ".ssh/config"]);

    env.ok(&["forget", ".bashrc", ".ssh/config"]);
    assert_eq!(
        fs::read(env.home().join(".bashrc")).unwrap(),
        b"export A=1\n"
    );
    assert_eq!(
        fs::read(env.home().join(".ssh/config")).unwrap(),
        b"Host *\n"
    );
    assert!(!env.repo().join(".bashrc").exists());
    assert!(!env.repo().join(".ssh").exists());
    assert_eq!(env.porcelain("list"), Vec::new());
    assert_eq!(env.porcelain("status"), Vec::new());
    assert!(env.repo_config().modes.is_empty());
    assert!(env.fails(&["forget", ".bashrc"]).contains("not managed"));
}

#[test]
fn forget_directory_removes_its_files_and_entry() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/app/a.conf", b"a", 0o644);
    env.write(".config/app/sub/b.conf", b"b", 0o644);
    env.write(".zshrc", b"z", 0o644);
    env.ok(&["add", ".config/app", ".zshrc"]);
    env.ok(&["forget", ".config/app"]);
    let left: Vec<String> = env.porcelain("list").into_iter().map(|(_, k)| k).collect();
    assert_eq!(left, [".zshrc"]);
    assert_eq!(env.repo_config().dirs.paths, Vec::<String>::new());
    assert!(env.home().join(".config/app/sub/b.conf").is_file());
    assert!(!env.repo().join(".config").exists());
}

/// Another host's variant counts too: forget removes every source of the key.
#[test]
fn forget_saves_each_uncommitted_repo_edit_it_removes_and_a_dry_run_says_where() {
    let env = TestEnv::new();
    env.write_machine_config("[identity]\nhost = \"box\"\n");
    env.ok(&["init"]);
    env.write(".zshrc", b"base\n", 0o644);
    env.write(".bashrc", b"bash\n", 0o644);
    env.ok(&["add", ".zshrc", ".bashrc"]);
    env.ok(&["sync"]);
    let edits = [
        (".zshrc", &b"hand edit in the repo\n"[..]),
        (".zshrc{host=other}", b"edit for the other host\n"),
    ];
    for (source, bytes) in edits {
        env.write_repo(source, bytes);
    }
    let backups = env.state_dir().join("backups");
    let saved = |err: &str, source: &str, verb: &str| {
        let marker = format!("{source}: {verb} ");
        let path = err
            .lines()
            .find_map(|line| line.split_once(marker.as_str()).map(|(_, p)| p.to_owned()));
        assert!(path.is_some(), "{marker}: {err}");
        std::path::PathBuf::from(path.unwrap_or_default())
    };

    let before = env.four_trees();
    let err = env.ok(&["forget", "--dry-run", ".zshrc", ".bashrc"]);
    for (source, _) in edits {
        let would = saved(
            &err,
            source,
            "would save the repo copy's uncommitted edit at",
        );
        assert!(would.starts_with(&backups), "{err}");
        assert!(would.ends_with(format!(".dotman/repo/{source}")), "{err}");
    }
    assert!(!err.contains(".bashrc: would save"), "committed: {err}");
    assert_eq!(env.four_trees(), before);

    let err = env.ok(&["forget", ".zshrc", ".bashrc"]);
    for (source, bytes) in edits {
        let path = saved(&err, source, "the repo copy's uncommitted edit is saved at");
        assert!(path.starts_with(&backups), "{err}");
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(!env.repo().join(source).exists());
    }
    assert!(!err.contains(".bashrc: the repo copy"), "committed: {err}");
    assert_eq!(env.read(".zshrc"), b"base\n");
}

#[test]
fn status_porcelain_reports_rows() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"one\n", 0o644);
    env.write(".zshrc", b"zsh\n", 0o644);
    env.ok(&["add", ".bashrc", ".zshrc"]);
    assert_eq!(env.token_of("status", ".bashrc").as_deref(), Some("clean"));

    env.write(".bashrc", b"two\n", 0o644);
    fs::remove_file(env.home().join(".zshrc")).unwrap();
    assert_eq!(
        env.token_of("status", ".bashrc").as_deref(),
        Some("changed-locally")
    );
    assert_eq!(
        env.token_of("status", ".zshrc").as_deref(),
        Some("deleted-locally")
    );

    let out = env.run(&["status", "--porcelain"]);
    assert!(out.stdout.ends_with(b"\0"));
    let human = env.run(&["status"]);
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(
        text.contains("changed-locally") && text.contains(".bashrc"),
        "{text}"
    );
}

#[test]
fn status_and_list_change_nothing() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"one\n", 0o644);
    env.ok(&["add", ".bashrc"]);
    env.write(".bashrc", b"two\n", 0o644);
    let before = env.four_trees();
    env.ok(&["status"]);
    env.ok(&["list"]);
    assert_eq!(env.four_trees(), before);
}

#[test]
fn own_files_and_non_nfc_names_are_refused() {
    let env = TestEnv::new();
    let inside = |args: &[&str]| {
        let mut cmd = env.dot();
        cmd.env("DOTMAN_REPO_PATH", env.home().join(".dotfiles"))
            .env("XDG_STATE_HOME", env.home().join(".local/state"))
            .args(args);
        cmd.output().expect("run dot")
    };
    assert!(inside(&["init"]).status.success());
    let refused = |path: &str| {
        let out = inside(&["add", path]);
        assert_eq!(out.status.code(), Some(1), "{path}");
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            err.contains("dotman never manages its own files"),
            "{path}: {err}"
        );
    };
    refused(".dotfiles");
    refused(".dotfiles/.dotman.toml");
    refused(".local/state/dotman");

    let nfd = "e\u{301}.txt";
    env.write(nfd, b"x", 0o644);
    let before = tree_checksum(&env.home());
    let out = inside(&["add", nfd]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("not NFC-normalized"), "{err}");
    assert_eq!(tree_checksum(&env.home()), before);
}

#[test]
fn non_nfc_names_inside_a_directory_are_skipped_and_reported() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write("dir/fine.txt", b"x", 0o644);
    env.write("dir/e\u{301}.txt", b"x", 0o644);
    let err = env.ok(&["add", "dir"]);
    assert!(
        err.contains("files: 1") && err.contains("skipped: 1"),
        "{err}"
    );
    assert!(err.contains("not NFC-normalized"), "{err}");
}

#[test]
fn dry_runs_leave_all_four_trees_equal() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"one\n", 0o644);
    env.write(".config/app/a.conf", b"a", 0o600);
    let before = env.four_trees();
    let err = env.ok(&["add", "--dry-run", ".bashrc", ".config/app"]);
    assert!(err.contains("would add"), "{err}");
    assert_eq!(env.four_trees(), before);

    env.ok(&["add", ".bashrc", ".config/app"]);
    let before = env.four_trees();
    let err = env.ok(&["forget", "--dry-run", ".bashrc", ".config/app"]);
    for line in ["would forget .bashrc", "would forget .config/app/"] {
        assert!(err.contains(line), "{err}");
    }
    assert_eq!(env.four_trees(), before);
    assert_eq!(env.porcelain("list").len(), 2);
    let err = env.ok(&["forget", ".bashrc"]);
    assert!(
        err.contains("forget .bashrc") && !err.contains("would"),
        "{err}"
    );
}

#[test]
fn a_symlinked_worktree_parent_is_never_written_through() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let outside = env.root().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, env.repo().join(".config")).unwrap();
    env.write(".config/x", b"secret", 0o644);
    let err = env.fails(&["add", ".config/x"]);
    assert!(err.contains("could not add"), "{err}");
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn commands_need_an_initialised_repo() {
    let env = TestEnv::new();
    env.write(".bashrc", b"x", 0o644);
    for args in [
        &["add", ".bashrc"][..],
        &["list"],
        &["status"],
        &["forget", ".bashrc"],
        &["apply"],
        &["sync"],
    ] {
        assert!(env.fails(args).contains("dotman init"), "{args:?}");
    }
    assert!(!env.repo().exists());
}

#[test]
fn git_before_init_names_init_and_the_printed_init_lets_it_run() {
    let env = TestEnv::new();
    let err = env.fails(&["git", "status"]);
    let expected = format!(
        "no repo at {} yet; run `dotman init` first",
        env.repo().display()
    );
    assert!(err.contains(&expected), "{err}");
    assert!(!env.repo().exists());
    let out = common::run_hint(&env, &err, "init");
    assert!(out.status.success(), "{}", common::text(&out));
    let out = env.run(&["git", "status", "--short"]);
    assert!(out.status.success(), "{}", common::text(&out));
}

#[test]
fn commands_with_nothing_to_do_say_so_in_one_line() {
    let mut env = TestEnv::new();
    env.use_system_root();
    env.ok(&["init"]);
    let said = |args: &[&str], line: &str| {
        let err = env.ok(args);
        assert_eq!(
            err.lines().filter(|l| l.contains(line)).count(),
            1,
            "{args:?}: {err}"
        );
    };
    said(&["init"], "is already set up");
    said(&["list"], "nothing is managed yet");
    said(&["log"], "the repo has no commits yet");
    let out = env.run(&["list", "--porcelain"]);
    assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty());

    env.write(".bashrc", b"one\n", 0o644);
    env.ok(&["add", ".bashrc"]);
    env.ok(&["sync"]);
    for args in [
        &["apply"][..],
        &["apply", "--dry-run"],
        &["apply", "--system"],
        &["apply", "--approve-hooks"],
    ] {
        said(args, "nothing to apply");
    }
    said(&["diff"], "no differences between the repo and $HOME");
    said(&["log", ".zshrc"], "no commit touches .zshrc");
    let url = env.remote_url();
    env.ok(&["init", "--remote", &url]);
    said(
        &["init", "--remote", &url],
        &format!("origin is already {url}"),
    );
    let out = env.run_with(&[("EDITOR", "true")], &["edit", ".bashrc"]);
    assert!(out.status.success());
    assert!(
        common::text(&out).contains(".bashrc is unchanged"),
        "{}",
        common::text(&out)
    );
    assert_eq!(env.ok(&["-q", "apply"]), "");
}

#[test]
fn list_help_explains_the_porcelain_records_and_every_kind() {
    let env = TestEnv::new();
    let out = env.run(&["list", "--help"]);
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("`<kind>\\t<key>\\0`"), "{help}");
    for kind in ["  file ", "  symlink ", "  system "] {
        assert!(help.contains(kind), "{kind}: {help}");
    }
}

/// A new file under `[dirs]` counts as managed, since sync captures it; a held one does not, as in
/// `list`, but both count as to look at.
#[test]
fn the_status_summary_counts_new_and_held_files_and_adds_up() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/app/a.conf", b"a\n", 0o644);
    env.write(".bashrc", b"b\n", 0o644);
    env.ok(&["add", ".config/app", ".bashrc"]);
    env.write(".bashrc", b"changed\n", 0o644);
    env.write(".config/app/new.conf", b"n\n", 0o644);
    env.write(".config/app/.env", b"X=1\n", 0o600);
    let err = env.ok(&["status"]);
    assert!(
        err.contains("3 managed, 1 clean, 3 to look at (1 new, 1 held back)"),
        "{err}"
    );
    assert_eq!(env.porcelain("list").len(), 2);
    env.write(".bashrc", b"b\n", 0o644);
    std::fs::remove_file(env.home().join(".config/app/new.conf")).unwrap();
    std::fs::remove_file(env.home().join(".config/app/.env")).unwrap();
    let err = env.ok(&["status"]);
    assert!(err.contains("2 managed, 2 clean, 0 to look at"), "{err}");
    assert!(!err.contains("held back"), "{err}");
}

#[test]
fn an_interrupted_init_is_named_and_a_repo_without_its_id_can_be_adopted() {
    let env = TestEnv::new();
    let git_dir = env.repo().join(".git");
    fs::create_dir_all(&git_dir).unwrap();
    for args in [&["status"][..], &["init"], &["apply"], &["sync"]] {
        let err = env.fails(args);
        assert!(
            err.contains("is incomplete (an earlier init was interrupted)"),
            "{args:?}: {err}"
        );
        assert!(err.contains("remove it and rerun `dotman init`"), "{err}");
    }

    let other = TestEnv::new();
    other.ok(&["init"]);
    fs::remove_file(other.repo().join(".git/dotman-id")).unwrap();
    fs::write(other.repo().join(".git/config.lock"), b"").unwrap();
    assert!(other.fails(&["status"]).contains("is incomplete"));
    fs::remove_file(other.repo().join(".git/config.lock")).unwrap();

    let err = other.fails(&["status"]);
    assert!(err.contains("has no .git/dotman-id"), "{err}");
    assert!(err.contains("run `dotman init` to adopt it"), "{err}");
    other.ok(&["init"]);
    other.ok(&["status"]);
}

#[test]
fn an_empty_state_file_is_refused_with_the_way_out() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"x\n", 0o644);
    env.ok(&["add", ".bashrc"]);
    let state = env.state_dir().join("state.toml");
    fs::write(&state, b"").unwrap();
    for args in [&["status"][..], &["apply"], &["sync"]] {
        let err = env.fails(args);
        assert!(err.contains("is empty"), "{args:?}: {err}");
        assert!(err.contains(".broken"), "{err}");
    }
    assert_eq!(fs::read(&state).unwrap(), b"");
}

fn real_git() -> Option<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
}

fn unhex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .chunks(2)
        .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// Names under the repo's parent directory, where init stages its work.
fn beside_repo(env: &TestEnv) -> Vec<String> {
    fs::read_dir(env.xdg_data()).map_or_else(
        |_| Vec::new(),
        |dir| {
            dir.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        },
    )
}

#[test]
fn a_clone_whose_checkout_fails_leaves_nothing_at_the_repo_path() {
    let env = TestEnv::new();
    let remote = env.remote_path().to_string_lossy().into_owned();
    let file = env.root().join("object");
    let object = |kind: &str, bytes: &[u8]| {
        fs::write(&file, bytes).unwrap();
        let path = file.to_string_lossy().into_owned();
        env.git(&[
            "--git-dir",
            &remote,
            "hash-object",
            "-t",
            kind,
            "-w",
            "--literally",
            &path,
        ])
        .trim()
        .to_owned()
    };
    let blob = unhex(&object("blob", b"x\n"));
    let mut inner = b"100644 x\0".to_vec();
    inner.extend_from_slice(&blob);
    // A name every git's checkout refuses (core.protectNTFS), so the clone fails half way.
    let mut outer = b"40000 GIT~1\0".to_vec();
    outer.extend_from_slice(&unhex(&object("tree", &inner)));
    outer.extend_from_slice(b"100644 a.conf\0");
    outer.extend_from_slice(&blob);
    let tree = object("tree", &outer);
    let commit = env.git(&["--git-dir", &remote, "commit-tree", &tree, "-m", "dotfiles"]);
    env.git(&[
        "--git-dir",
        &remote,
        "update-ref",
        "refs/heads/main",
        commit.trim(),
    ]);
    env.git(&[
        "--git-dir",
        &remote,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);

    for _ in 0..2 {
        let err = env.fails(&["init", &env.remote_url(), "--take-repo"]);
        assert!(err.contains("GIT~1"), "{err}");
        assert!(!env.repo().exists(), "{err}");
        assert_eq!(beside_repo(&env), Vec::<String>::new());
    }
    assert!(!env.home().join("a.conf").exists());
}

#[test]
fn a_git_init_that_dies_leaves_no_skeleton_and_a_rerun_works() {
    let mut env = TestEnv::new();
    let stubs = env.root().join("stubs");
    fs::create_dir(&stubs).unwrap();
    let stub = stubs.join("git");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\n\"{}\" \"$@\"; rc=$?\ncase \" $* \" in *\" init -q \"*) [ -e \"$STOP\" ] && exit 137;; esac\nexit $rc\n",
            real_git().unwrap().display()
        ),
    )
    .unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    env.prepend_path(&stubs);
    let stop = env.root().join("stop").to_string_lossy().into_owned();
    fs::write(&stop, b"").unwrap();

    let out = env.run_with(&[("STOP", &stop)], &["init", "--no-import"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!env.repo().exists());
    assert_eq!(beside_repo(&env), Vec::<String>::new());

    fs::remove_file(&stop).unwrap();
    env.ok(&["init", "--no-import"]);
    assert!(env.repo().join(".git/dotman-id").is_file());
    assert_eq!(beside_repo(&env), ["dotman"]);

    // A repo path that already holds files keeps them.
    let other = TestEnv::new();
    other.write_repo("notes.txt", b"mine\n");
    other.ok(&["init", "--no-import"]);
    assert_eq!(fs::read(other.repo().join("notes.txt")).unwrap(), b"mine\n");
    other.ok(&["status"]);
}

/// Sets the modification time of `path` to an hour ago, past the sweeps' 10-minute cutoff.
fn age(path: &Path) -> std::io::Result<()> {
    let hour = std::time::Duration::from_secs(3600);
    let then = std::time::SystemTime::now()
        .checked_sub(hour)
        .unwrap_or(std::time::UNIX_EPOCH);
    fs::File::open(path)?.set_modified(then)
}

/// `init <url>` clones into the directory a symlinked repo path points at, keeps the mode of an
/// empty directory already at the repo path, and refuses one that holds files before anything is
/// cloned, in a dry run too. A staging directory a killed run left goes once its PID is dead and
/// it is old.
#[test]
fn init_url_fills_a_symlinked_or_empty_repo_path_and_refuses_a_full_one() {
    let first = TestEnv::new();
    first.ok(&["init", "--remote", &first.remote_url()]);
    first.write(".bashrc", b"x\n", 0o644);
    first.ok(&["add", ".bashrc"]);
    first.ok(&["sync"]);
    let remote = first.remote_path().to_string_lossy().into_owned();
    first.git(&[
        "--git-dir",
        &remote,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    let url = first.remote_url();

    let linked = first.second_device();
    let elsewhere = linked.root().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::create_dir_all(linked.xdg_data()).unwrap();
    std::os::unix::fs::symlink(&elsewhere, linked.repo()).unwrap();
    linked.ok(&["init", &url]);
    assert!(elsewhere.join(".git/dotman-id").is_file());
    assert!(linked.repo().is_symlink());
    assert_eq!(linked.read(".bashrc"), b"x\n");
    let staged = |dir: &Path| {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().starts_with(".dotman-init-"))
    };
    assert!(!staged(linked.root()) && !staged(&linked.xdg_data()));

    let private = first.second_device();
    fs::create_dir_all(private.repo()).unwrap();
    fs::set_permissions(private.repo(), fs::Permissions::from_mode(0o700)).unwrap();
    private.ok(&["init", &url]);
    assert_eq!(mode_of(&private.repo()), 0o700);
    assert_eq!(private.read(".bashrc"), b"x\n");

    let full = first.second_device();
    full.write_repo("notes.txt", b"mine\n");
    for args in [&["init", "--dry-run", &url][..], &["init", &url]] {
        let err = full.fails(args);
        assert!(err.contains("exists and is not empty"), "{args:?}: {err}");
    }
    assert_eq!(fs::read(full.repo().join("notes.txt")).unwrap(), b"mine\n");
    assert!(!full.repo().join(".git").exists());
    assert_eq!(beside_repo(&full), ["dotman"]);

    let killed = first.second_device();
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    let data = killed.xdg_data();
    let stale = data.join(format!(".dotman-init-{dead}-AbCdEf"));
    let young = data.join(format!(".dotman-init-{dead}-GhIjKl"));
    let old_style = data.join(".dotman-init-MnOpQr");
    for dir in [&stale, &young, &old_style] {
        fs::create_dir_all(dir.join("repo")).unwrap();
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    age(&stale).unwrap();
    age(&old_style).unwrap();
    killed.ok(&["init", &url]);
    assert!(!stale.exists());
    assert!(young.exists() && old_style.exists(), "too new, or no PID");
}

#[test]
fn a_corrupt_index_or_head_is_named_with_doctor() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"x\n", 0o644);
    env.ok(&["add", ".bashrc"]);
    env.ok(&["sync"]);
    let index = env.repo().join(".git/index");
    fs::write(&index, b"DIRC garbage").unwrap();
    let err = env.fails(&["sync"]);
    assert!(
        err.contains(&format!("{} is corrupt", index.display())),
        "{err}"
    );
    assert!(err.contains("`dotman doctor` checks the repo"), "{err}");

    // A lost index is rebuilt by the next commit, not read as every file deleted.
    fs::remove_file(&index).unwrap();
    env.ok(&["sync"]);

    let other = TestEnv::new();
    other.ok(&["init"]);
    // The repo's parent looks like a git repo, as a `~/.git` dotfiles repo would be.
    let parent = other.xdg_data().to_string_lossy().into_owned();
    other.git(&["init", "-q", "--bare", &parent]);
    let head = other.repo().join(".git/HEAD");
    fs::remove_file(&head).unwrap();
    for args in [&["status"][..], &["apply"], &["sync"]] {
        let err = other.fails(args);
        assert!(
            err.contains(&format!("{} is missing or corrupt", head.display())),
            "{args:?}: {err}"
        );
        assert!(
            !err.contains("is incomplete"),
            "a repo with its id finished init: {err}"
        );
    }
}

#[test]
fn a_git_that_hangs_on_a_short_probe_is_given_up_on() {
    let mut env = TestEnv::new();
    env.ok(&["init"]);
    let stubs = env.root().join("stubs");
    fs::create_dir(&stubs).unwrap();
    let stub = stubs.join("git");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\ncase \" $* \" in *\" --version \"*) exec sleep 60;; esac\nexec \"{}\" \"$@\"\n",
            real_git().unwrap().display()
        ),
    )
    .unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
    env.prepend_path(&stubs);
    let started = std::time::Instant::now();
    let err = env.fails(&["status"]);
    assert!(
        err.contains("`git --version` did not finish within 5 s"),
        "{err}"
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}
