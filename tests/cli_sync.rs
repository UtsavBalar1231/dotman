mod common;

use common::{TestEnv, assert_converged, mode_of, short_host};
use std::fs;
use std::process::Output;

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Runs `dot`, asserts the exit code and returns stderr.
fn exits(env: &TestEnv, code: i32, args: &[&str]) -> String {
    let out = env.run(args);
    assert_eq!(
        out.status.code(),
        Some(code),
        "dot {args:?}: {}",
        stderr(&out)
    );
    stderr(&out)
}

/// A device with `files` added, `origin` set and the first sync done.
fn first_device(files: &[(&str, &[u8], u32)]) -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["init", "--remote", &env.remote_url()]);
    let mut args = vec!["add"];
    for (key, bytes, mode) in files {
        env.write(key, bytes, *mode);
        args.push(key);
    }
    env.ok(&args);
    env.ok(&["sync"]);
    // A bare remote's HEAD names `master` until something says otherwise, and a clone follows it.
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

fn clone_to_new_device(first: &TestEnv) -> TestEnv {
    let env = first.second_device();
    env.ok(&["init", &env.remote_url(), "--take-repo"]);
    env
}

fn pair(files: &[(&str, &[u8], u32)]) -> (TestEnv, TestEnv) {
    let a = first_device(files);
    let b = clone_to_new_device(&a);
    (a, b)
}

fn remote_file(env: &TestEnv, key: &str) -> String {
    let remote = env.remote_path().to_string_lossy().into_owned();
    env.git(&["--git-dir", &remote, "show", &format!("main:{key}")])
}

fn repo_path(env: &TestEnv) -> String {
    env.repo().to_string_lossy().into_owned()
}

#[test]
fn a_second_device_gets_what_the_first_synced() {
    let (a, b) = pair(&[(".a", b"a\n", 0o644), (".config/app/b.conf", b"b\n", 0o644)]);
    assert_eq!(b.read(".a"), b"a\n");
    assert_eq!(b.read(".config/app/b.conf"), b"b\n");
    assert_converged(&a, &b);
    exits(&b, 0, &["sync"]);
}

#[test]
fn disjoint_edits_converge_after_syncing_a_then_b_then_a() {
    let (a, b) = pair(&[(".a", b"a\n", 0o644), (".b", b"b\n", 0o644)]);
    a.write(".a", b"a edited on a\n", 0o644);
    b.write(".b", b"b edited on b\n", 0o644);
    exits(&a, 0, &["sync"]);
    exits(&b, 0, &["sync"]);
    exits(&a, 0, &["sync"]);
    assert_eq!(a.read(".b"), b"b edited on b\n");
    assert_eq!(b.read(".a"), b"a edited on a\n");
    assert_converged(&a, &b);
    assert_eq!(a.token_of("status", ".a").as_deref(), Some("clean"));
    assert_eq!(b.token_of("status", ".b").as_deref(), Some("clean"));
}

#[test]
fn the_same_file_edited_on_both_exits_4_and_the_printed_steps_recover() {
    let (a, b) = pair(&[(".a", b"base\n", 0o644), (".b", b"b\n", 0o644)]);
    a.write(".a", b"from a\n", 0o644);
    exits(&a, 0, &["sync"]);
    let published = a.remote_refs();

    b.write(".a", b"from b\n", 0o644);
    let err = exits(&b, 4, &["sync"]);
    for step in [
        "1. dot git pull --rebase",
        "dot git add .a",
        "4. dot git rebase --continue",
        "5. dot sync",
    ] {
        assert!(err.contains(step), "{step}: {err}");
    }
    assert!(
        err.contains(&format!(
            "2. Edit the conflicting files under {}",
            repo_path(&b)
        )),
        "{err}"
    );
    assert!(err.contains("conflicting paths:\n  .a"), "{err}");

    assert!(!b.repo().join(".git/rebase-merge").exists());
    assert!(!b.repo().join(".git/rebase-apply").exists());
    assert_eq!(b.read(".a"), b"from b\n");
    assert_eq!(a.read(".a"), b"from a\n");
    assert_eq!(
        fs::read(b.repo().join(".a")).expect("repo file"),
        b"from b\n"
    );
    assert_eq!(a.remote_refs(), published);

    let repo = repo_path(&b);
    let pull = b.git_raw(&["-C", &repo, "pull", "--rebase"]);
    assert!(
        !pull.status.success(),
        "the pull should stop on the conflict"
    );
    assert!(b.repo().join(".git/rebase-merge").exists());
    exits(&b, 4, &["status"]);
    b.write_repo(".a", b"resolved\n");
    b.git(&["-C", &repo, "add", ".a"]);
    b.git(&[
        "-C",
        &repo,
        "-c",
        "core.editor=true",
        "rebase",
        "--continue",
    ]);

    exits(&b, 0, &["sync"]);
    assert_eq!(b.read(".a"), b"resolved\n");
    exits(&a, 0, &["sync"]);
    assert_eq!(a.read(".a"), b"resolved\n");
    assert_converged(&a, &b);
}

#[test]
fn a_rebase_conflict_wins_over_a_skipped_path() {
    let (a, b) = pair(&[(".a", b"base\n", 0o644), (".d", b"d\n", 0o644)]);
    a.write(".a", b"from a\n", 0o644);
    exits(&a, 0, &["sync"]);

    b.write(".a", b"from b\n", 0o644);
    fs::remove_file(b.home().join(".d")).expect("delete .d");
    let err = exits(&b, 4, &["sync"]);
    assert!(err.contains("local deletions left undecided (1)"), "{err}");
    assert!(err.contains("dot git rebase --continue"), "{err}");
    assert!(!b.home().join(".d").exists());
}

#[test]
fn a_forget_reaches_the_other_device_without_deleting_its_file() {
    let (a, b) = pair(&[(".a", b"a\n", 0o644), (".c", b"c\n", 0o644)]);
    a.ok(&["forget", ".c"]);
    exits(&a, 0, &["sync"]);
    let err = exits(&b, 0, &["sync"]);
    assert!(err.contains(".c: forgotten upstream"), "{err}");
    assert_eq!(b.read(".c"), b"c\n");
    assert!(b.porcelain("list").iter().all(|(_, key)| key != ".c"));
    assert_eq!(b.token_of("status", ".c"), None);
}

#[test]
fn modes_and_symlinks_round_trip() {
    let a = TestEnv::new();
    a.ok(&["init"]);
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write(".secret", b"s\n", 0o600);
    a.write(".a", b"a\n", 0o644);
    a.symlink(".link", ".a");
    a.ok(&["add", ".secret", ".a", ".link"]);
    exits(&a, 0, &["sync"]);
    let remote = a.remote_path().to_string_lossy().into_owned();
    a.git(&[
        "--git-dir",
        &remote,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);

    let b = clone_to_new_device(&a);
    assert_eq!(mode_of(&b.home().join(".secret")), 0o600);
    assert_eq!(
        fs::read_link(b.home().join(".link")).expect("link"),
        std::path::Path::new(".a")
    );
    assert_converged(&a, &b);

    a.write(".secret", b"s2\n", 0o600);
    a.write(".a", b"a\n", 0o755);
    exits(&a, 0, &["sync"]);
    exits(&b, 0, &["sync"]);
    assert_eq!(b.read(".secret"), b"s2\n");
    assert_eq!(mode_of(&b.home().join(".secret")), 0o600);
    assert_eq!(mode_of(&b.home().join(".a")), 0o755);
    assert_converged(&a, &b);
}

#[test]
fn commit_messages_name_three_paths_and_count_the_rest() {
    let files: Vec<(String, &[u8], u32)> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|name| (format!("x/{name}"), b"1\n".as_slice(), 0o644))
        .collect();
    let env = TestEnv::new();
    env.ok(&["init"]);
    let keys: Vec<&str> = files.iter().map(|(key, _, _)| key.as_str()).collect();
    for (key, bytes, mode) in &files {
        env.write(key, bytes, *mode);
    }
    let mut add = vec!["add"];
    add.extend(&keys);
    env.ok(&add);
    exits(&env, 0, &["sync"]);
    let repo = repo_path(&env);
    let subject = || env.git(&["-C", &repo, "log", "-1", "--format=%s"]);
    let host = short_host();
    assert_eq!(
        subject().trim(),
        format!("sync({host}): .dotman.toml, x/a, x/b (+3 more)")
    );

    for key in &keys {
        env.write(key, b"2\n", 0o644);
    }
    exits(&env, 0, &["sync"]);
    assert_eq!(
        subject().trim(),
        format!("sync({host}): x/a, x/b, x/c (+2 more)")
    );

    env.write("x/a", b"3\n", 0o644);
    exits(&env, 0, &["sync", "-m", "hello"]);
    assert_eq!(subject().trim(), "hello");
    env.write("x/a", b"4\n", 0o644);
    exits(&env, 0, &["sync"]);
    assert_eq!(subject().trim(), format!("sync({host}): x/a"));
}

#[test]
fn a_missing_git_identity_exits_1_with_the_fix_and_commits_nothing() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", b"a\n", 0o644);
    env.ok(&["add", ".a"]);
    fs::remove_file(env.home().join(".gitconfig")).expect("remove the identity");
    let err = env.fails(&["sync"]);
    assert!(
        err.contains("dot git config --global user.name \"Your Name\""),
        "{err}"
    );
    assert!(
        err.contains("dot git config --global user.email \"you@example.com\""),
        "{err}"
    );
    assert!(!env.repo().join(".git/refs/heads/main").exists());
}

#[test]
fn a_git_identity_from_the_environment_is_enough_to_commit() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", b"a\n", 0o644);
    env.ok(&["add", ".a"]);
    fs::remove_file(env.home().join(".gitconfig")).expect("remove the identity");
    let identity = [
        ("GIT_AUTHOR_NAME", "Ci"),
        ("GIT_AUTHOR_EMAIL", "ci@example.invalid"),
        ("GIT_COMMITTER_NAME", "Ci"),
        ("GIT_COMMITTER_EMAIL", "ci@example.invalid"),
    ];
    let out = env.run_with(&identity, &["sync"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = env.git(&["-C", &repo_path(&env), "log", "--format=%an <%ae>"]);
    assert_eq!(log.trim(), "Ci <ci@example.invalid>");
}

#[test]
fn without_a_remote_sync_commits_and_prints_the_hint() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", b"a\n", 0o644);
    env.ok(&["add", ".a"]);
    let err = exits(&env, 0, &["sync"]);
    assert!(err.contains("dot init --remote <url>"), "{err}");
    let repo = repo_path(&env);
    let log = env.git(&["-C", &repo, "log", "--format=%s"]);
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(
        log.starts_with(&format!("sync({}): ", short_host())),
        "{log}"
    );
    assert_eq!(env.remote_refs(), "");
}

#[test]
fn a_dry_run_changes_nothing_anywhere() {
    let (a, _b) = pair(&[(".a", b"a\n", 0o644), (".b", b"b\n", 0o644)]);
    a.write(".a", b"edited\n", 0o644);
    a.write(".c", b"c\n", 0o600);
    a.ok(&["add", ".c"]);
    a.write(".c", b"c edited\n", 0o600);
    let before = (a.four_trees(), a.remote_refs());

    let err = exits(&a, 0, &["sync", "--dry-run"]);
    assert!(err.contains("would capture .a"), "{err}");
    assert!(err.contains("would capture .c"), "{err}");
    assert!(err.contains("would commit: sync("), "{err}");
    assert!(err.contains("would pull origin main"), "{err}");
    assert_eq!((a.four_trees(), a.remote_refs()), before);

    exits(&a, 0, &["sync"]);
    assert_ne!(a.remote_refs(), before.1);
}

#[test]
fn a_managed_file_that_the_global_excludes_ignore_still_reaches_the_remote() {
    let key = ".claude/settings.local.json";
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["init", "--remote", &env.remote_url()]);
    let ignore = env.xdg_config().join("git/ignore");
    fs::create_dir_all(ignore.parent().expect("parent")).expect("create git config dir");
    fs::write(&ignore, "**/.claude/settings.local.json\n").expect("write the excludes");
    env.write(key, b"{}\n", 0o644);
    env.ok(&["add", key]);

    let repo = repo_path(&env);
    env.git(&["-C", &repo, "check-ignore", "-q", key]);
    exits(&env, 0, &["sync"]);
    let remote = env.remote_path().to_string_lossy().into_owned();
    let tree = env.git(&["--git-dir", &remote, "ls-tree", "-r", "--name-only", "main"]);
    assert!(tree.lines().any(|line| line == key), "{tree}");
}

#[test]
fn a_skipped_path_exits_3_and_everything_else_is_still_pushed() {
    let (a, _b) = pair(&[(".a", b"a\n", 0o644), (".d", b"d\n", 0o644)]);
    a.write(".a", b"edited\n", 0o644);
    fs::remove_file(a.home().join(".d")).expect("delete .d");
    let err = exits(&a, 3, &["sync"]);
    assert!(err.contains("local deletions left undecided (1)"), "{err}");
    assert!(
        err.contains("--forget-deleted or --restore-deleted"),
        "{err}"
    );
    assert_eq!(remote_file(&a, ".a"), "edited\n");
    assert_eq!(remote_file(&a, ".d"), "d\n");
}

#[test]
fn forget_deleted_removes_a_locally_deleted_path_from_the_remote() {
    let (a, b) = pair(&[(".a", b"a\n", 0o644), (".d", b"d\n", 0o644)]);
    fs::remove_file(a.home().join(".d")).expect("delete .d");
    exits(&a, 0, &["sync", "--forget-deleted"]);
    let remote = a.remote_path().to_string_lossy().into_owned();
    let tree = a.git(&["--git-dir", &remote, "ls-tree", "-r", "--name-only", "main"]);
    assert!(!tree.lines().any(|line| line == ".d"), "{tree}");
    exits(&b, 0, &["sync"]);
    assert_eq!(b.read(".d"), b"d\n");
}

#[test]
fn keep_local_captures_a_path_changed_on_both_sides_and_take_repo_backs_it_up() {
    let (_a, b) = pair(&[(".a", b"base\n", 0o644)]);
    b.write(".a", b"mine\n", 0o644);
    b.write_repo(".a", b"theirs\n");
    assert_eq!(b.token_of("status", ".a").as_deref(), Some("both-changed"));

    let err = exits(&b, 3, &["sync"]);
    assert!(err.contains("conflicts left undecided (1)"), "{err}");
    assert_eq!(b.read(".a"), b"mine\n");

    exits(&b, 0, &["sync", "--keep-local"]);
    assert_eq!(b.read(".a"), b"mine\n");
    assert_eq!(remote_file(&b, ".a"), "mine\n");
    assert_eq!(b.token_of("status", ".a").as_deref(), Some("clean"));

    b.write(".a", b"mine again\n", 0o644);
    b.write_repo(".a", b"theirs again\n");
    exits(&b, 0, &["sync", "--take-repo"]);
    assert_eq!(b.read(".a"), b"theirs again\n");
    assert_eq!(b.backups(".a"), [(b"mine again\n".to_vec(), 0o600)]);
}

const BROKEN: &str = ".config/app/t{tmpl}.conf";
const RENDERED: &str = ".config/app/t.conf";

#[test]
fn a_path_that_fails_to_render_still_lets_sync_commit_and_push() {
    let env = first_device(&[(".a", b"base\n", 0o644)]);
    env.write_repo(BROKEN, b"x {{ vars.missing }}\n");
    env.write(".a", b"changed\n", 0o644);
    let before = env.remote_refs();

    let err = exits(&env, 1, &["sync"]);
    assert!(err.contains("t{tmpl}.conf"), "{err}");
    assert!(err.contains("1 path could not be applied"), "{err}");
    assert_ne!(env.remote_refs(), before, "{err}");
    assert_eq!(remote_file(&env, ".a"), "changed\n");
    assert!(!env.home().join(RENDERED).exists());
}

#[test]
fn a_template_that_cannot_render_here_does_not_block_the_upstream_fix() {
    let (a, b) = pair(&[(".a", b"base\n", 0o644)]);
    a.write_machine_config("[vars]\nnewvar = \"1\"\n");
    a.write_repo(BROKEN, b"v {{ vars.newvar }}\n");
    exits(&a, 0, &["sync"]);
    assert_eq!(a.read(RENDERED), b"v 1\n");

    let err = exits(&b, 1, &["sync"]);
    assert!(err.contains("vars.newvar"), "{err}");
    assert!(!b.home().join(RENDERED).exists());

    a.write_repo(BROKEN, b"v fixed\n");
    exits(&a, 0, &["sync"]);
    exits(&b, 0, &["sync"]);
    assert_eq!(b.read(RENDERED), b"v fixed\n");
}

#[test]
fn failed_paths_do_not_stop_a_sync_without_a_remote_or_a_dry_run() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", b"base\n", 0o644);
    env.ok(&["add", ".a"]);
    env.write_repo(BROKEN, b"x {{ vars.missing }}\n");
    env.write(".a", b"changed\n", 0o644);

    let err = exits(&env, 1, &["sync", "--dry-run"]);
    assert!(err.contains("would commit"), "{err}");
    assert!(err.contains("1 path could not be applied"), "{err}");

    let err = exits(&env, 1, &["sync"]);
    assert!(err.contains("committed"), "{err}");
    assert!(err.contains("1 path could not be applied"), "{err}");
    let log = env.git(&["-C", &repo_path(&env), "show", "HEAD:.a"]);
    assert_eq!(log, "changed\n");
}
