mod common;

use common::{TestEnv, assert_converged, mode_of};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Output;

/// Set on every run of a test that reads the host from a `sync(<host>)` subject, so the subject
/// does not depend on the machine running the tests.
const HOST: [(&str, &str); 1] = [("DOTMAN_HOST", "testbox")];

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Runs `dotman`, asserts the exit code and returns stderr.
fn exits(env: &TestEnv, code: i32, args: &[&str]) -> String {
    exits_with(env, &[], code, args)
}

fn exits_with(env: &TestEnv, vars: &[(&str, &str)], code: i32, args: &[&str]) -> String {
    let out = env.run_with(vars, args);
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
        "1. dotman git pull --rebase",
        "dotman git add .a",
        "4. dotman git rebase --continue",
        "5. dotman sync",
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
    // git before 2.26 rebases with the apply backend.
    assert!(
        b.repo().join(".git/rebase-merge").exists() || b.repo().join(".git/rebase-apply").exists()
    );
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

    // S still equals B's edit, so only the always-back-up rule keeps those bytes outside history.
    let err = exits(&b, 0, &["sync"]);
    assert!(err.contains("backed up to"), "{err}");
    assert_eq!(b.read(".a"), b"resolved\n");
    assert_eq!(b.backups(".a"), [(b"from b\n".to_vec(), 0o600)]);
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
    assert!(err.contains("dotman git rebase --continue"), "{err}");
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
    env.ok_with(&HOST, &["init", "--no-bootstrap"]);
    let keys: Vec<&str> = files.iter().map(|(key, _, _)| key.as_str()).collect();
    for (key, bytes, mode) in &files {
        env.write(key, bytes, *mode);
    }
    let mut add = vec!["add"];
    add.extend(&keys);
    env.ok_with(&HOST, &add);
    exits_with(&env, &HOST, 0, &["sync"]);
    let repo = repo_path(&env);
    let subject = || env.git(&["-C", &repo, "log", "-1", "--format=%s"]);
    assert_eq!(
        subject().trim(),
        "sync(testbox): .dotman.toml, x/a, x/b (+3 more)"
    );

    for key in &keys {
        env.write(key, b"2\n", 0o644);
    }
    exits_with(&env, &HOST, 0, &["sync"]);
    assert_eq!(subject().trim(), "sync(testbox): x/a, x/b, x/c (+2 more)");

    env.write("x/a", b"3\n", 0o644);
    exits_with(&env, &HOST, 0, &["sync", "-m", "hello"]);
    assert_eq!(subject().trim(), "hello");
    env.write("x/a", b"4\n", 0o644);
    exits_with(&env, &HOST, 0, &["sync"]);
    assert_eq!(subject().trim(), "sync(testbox): x/a");
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
        err.contains("dotman git config --global user.name \"Your Name\""),
        "{err}"
    );
    assert!(
        err.contains("dotman git config --global user.email \"you@example.com\""),
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
    env.ok_with(&HOST, &["init"]);
    env.write(".a", b"a\n", 0o644);
    env.ok_with(&HOST, &["add", ".a"]);
    let err = exits_with(&env, &HOST, 0, &["sync"]);
    assert!(err.contains("dotman init --remote <url>"), "{err}");
    let repo = repo_path(&env);
    let log = env.git(&["-C", &repo, "log", "--format=%s"]);
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(log.starts_with("sync(testbox): "), "{log}");
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

/// What a git hook or a shell exports never redirects dotman's git: with `GIT_NAMESPACE` set, sync
/// still moves `main` and writes no `refs/namespaces/`, and with `GIT_LITERAL_PATHSPECS` set the
/// temp and held exclusions still read as pathspec magic.
#[test]
fn inherited_git_variables_never_redirect_sync() {
    let (a, b) = pair(&[(".a", b"a\n", 0o644)]);
    a.write(".a", b"namespaced\n", 0o644);
    exits_with(&a, &[("GIT_NAMESPACE", "evil")], 0, &["sync"]);
    let refs = a.remote_refs();
    assert!(!refs.contains("refs/namespaces/"), "{refs}");
    assert_eq!(remote_file(&a, ".a"), "namespaced\n");

    a.write(".a", b"literal\n", 0o644);
    exits_with(&a, &[("GIT_LITERAL_PATHSPECS", "1")], 0, &["sync"]);
    assert_eq!(remote_file(&a, ".a"), "literal\n");
    exits(&b, 0, &["sync"]);
    assert_eq!(b.read(".a"), b"literal\n");
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

#[test]
fn a_remote_with_unrelated_history_is_refused_before_the_pull() {
    let (a, _b) = pair(&[(".a", b"a\n", 0o644), (".b", b"b\n", 0o644)]);
    let elsewhere = a.root().join("rewrite");
    let dir = elsewhere.to_string_lossy().into_owned();
    a.git(&["init", "-q", &dir]);
    fs::write(elsewhere.join(".a"), b"rewritten\n").expect("write");
    a.git(&["-C", &dir, "add", "."]);
    a.git(&["-C", &dir, "commit", "-q", "-m", "new root"]);
    let remote = a.remote_path().to_string_lossy().into_owned();
    a.git(&[
        "-C",
        &dir,
        "push",
        "-q",
        "-f",
        &remote,
        "HEAD:refs/heads/main",
    ]);
    let rewritten = a.remote_refs();
    let head = a.git(&["-C", &repo_path(&a), "rev-parse", "HEAD"]);

    a.write(".b", b"edited after the rewrite\n", 0o644);
    let err = a.fails(&["sync"]);
    assert!(err.contains("shares no history"), "{err}");
    assert!(err.contains("dotman import"), "{err}");
    assert!(err.contains("dotman import --overlay"), "{err}");
    assert!(err.contains("--allow-unrelated-histories"), "{err}");
    assert_eq!(a.read(".a"), b"a\n");
    assert_eq!(a.read(".b"), b"edited after the rewrite\n");
    assert_eq!(a.remote_refs(), rewritten);
    assert!(!a.repo().join(".git/rebase-merge").exists());
    let log = a.git(&["-C", &repo_path(&a), "log", "--format=%H"]);
    assert!(log.lines().any(|oid| oid == head.trim()), "{log}");
}

#[test]
fn a_commit_that_deletes_most_tracked_files_needs_allow_mass_delete() {
    let keys: Vec<String> = (0..12)
        .map(|n| format!(".config/app/f{n:02}.conf"))
        .collect();
    let files: Vec<(&str, &[u8], u32)> = keys
        .iter()
        .map(|key| (key.as_str(), b"x\n".as_slice(), 0o644))
        .collect();
    let env = first_device(&files);
    let published = env.remote_refs();
    // A broken checkout: the worktree lost every managed file, HOME still has them.
    for key in &keys {
        fs::remove_file(env.repo().join(key)).expect("delete from the worktree");
    }
    for args in [&["sync", "--dry-run"][..], &["sync"]] {
        let err = env.fails(args);
        assert!(err.contains("delete 12 of its"), "{err}");
        assert!(err.contains("--allow-mass-delete"), "{err}");
        assert!(err.contains("dotman git checkout -- ."), "{err}");
    }
    // The dry run previews everything first, then says the real run would refuse.
    let err = env.fails(&["sync", "--dry-run"]);
    let preview = err.find("would pull origin main").expect("the preview");
    let refusal = err.find("sync would refuse").expect("the refusal");
    assert!(preview < refusal, "{err}");
    assert!(err.contains("would commit: "), "{err}");
    assert_eq!(env.remote_refs(), published);
    let status = env.git(&["-C", &repo_path(&env), "status", "--porcelain"]);
    assert!(
        status.lines().all(|line| line.starts_with(" D")),
        "{status}"
    );

    exits(&env, 0, &["sync", "--allow-mass-delete"]);
    let remote = env.remote_path().to_string_lossy().into_owned();
    let tree = env.git(&["--git-dir", &remote, "ls-tree", "-r", "--name-only", "main"]);
    assert!(!tree.contains(".config/app/"), "{tree}");
    assert_eq!(env.read(".config/app/f00.conf"), b"x\n");
}

/// Without a remote there is no pull to stop, so the held file is only left out of the commit.
#[test]
fn a_repo_file_given_an_age_secret_key_by_edit_is_held_out_of_the_commit() {
    let a = TestEnv::new();
    a.ok(&["init"]);
    a.write(".a", b"a\n", 0o644);
    a.write(".b", b"b\n", 0o644);
    a.ok(&["add", ".a", ".b"]);
    a.ok(&["sync"]);
    let key = "AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ";
    let editor = a.write_script("ed.sh", &format!("#!/bin/sh\necho '{key}' >> \"$1\"\n"));
    let editor = editor.to_string_lossy().into_owned();
    a.ok_with(&[("EDITOR", &editor), ("VISUAL", "")], &["edit", ".a"]);
    a.write(".b", b"harmless\n", 0o644);

    let err = exits(&a, 3, &["sync"]);
    assert!(
        err.contains("\n  .a: line 2 holds an age secret key (AGE-SECRET-KEY-)\n"),
        "{err}"
    );
    assert!(
        err.contains("): .b\n"),
        "the message names only what was committed: {err}"
    );
    let show = |key: &str| a.git(&["-C", &repo_path(&a), "show", &format!("HEAD:{key}")]);
    assert_eq!(show(".b"), "harmless\n");
    assert_eq!(show(".a"), "a\n");
    let local = a.git(&["-C", &repo_path(&a), "log", "--all", "-p", "--format="]);
    assert!(!local.contains("AGE-SECRET-KEY-"), "{local}");

    a.write_repo(".a", b"a, key removed\n");
    exits(&a, 0, &["sync"]);
    assert_eq!(show(".a"), "a, key removed\n");
}

/// A rename staged with `dotman git mv` whose new path holds an age key is held as a whole: the
/// old path is not deleted from the history, in this sync or the next, until the new one can be
/// committed.
#[test]
fn a_held_rename_never_commits_the_deletion_of_its_old_path() {
    let a = TestEnv::new();
    a.ok(&["init"]);
    a.write(".a", b"a\n", 0o644);
    a.ok(&["add", ".a"]);
    a.ok(&["sync"]);
    a.ok(&["git", "mv", ".a", ".b"]);
    a.write_repo(".b", format!("a\n{AGE_KEY}\n").as_bytes());
    let tree = || a.git(&["-C", &repo_path(&a), "ls-tree", "-r", "--name-only", "HEAD"]);
    for _ in 0..2 {
        let err = exits(&a, 3, &["sync"]);
        assert!(
            err.contains("\n  .b: line 2 holds an age secret key"),
            "{err}"
        );
        let names = tree();
        assert!(names.lines().any(|name| name == ".a"), "{names}");
        assert!(!names.lines().any(|name| name == ".b"), "{names}");
    }

    a.write_repo(".b", b"a, renamed\n");
    exits(&a, 0, &["sync"]);
    let names = tree();
    assert!(names.lines().any(|name| name == ".b"), "{names}");
    assert!(!names.lines().any(|name| name == ".a"), "{names}");
}

/// Sets the modification time of `path` to an hour ago, well before the sweep's 10-minute cutoff.
fn age(path: &std::path::Path) -> std::io::Result<()> {
    let hour = std::time::Duration::from_secs(3600);
    let then = std::time::SystemTime::now()
        .checked_sub(hour)
        .unwrap_or(std::time::UNIX_EPOCH);
    fs::File::open(path)?.set_modified(then)
}

#[test]
fn leftover_temp_files_of_dead_runs_are_swept_and_never_committed() {
    let env = first_device(&[(".config/app/a.conf", b"a\n", 0o644)]);
    let mut child = std::process::Command::new("true").spawn().expect("spawn");
    let dead = child.id();
    child.wait().expect("wait");
    let live = std::process::id();
    let repo_temp = env
        .repo()
        .join(format!(".config/app/.dotman-tmp-{dead}-AbCdEf"));
    let home_temp = env
        .home()
        .join(format!(".config/app/.dotman-tmp-{dead}-GhIjKl"));
    let live_temp = env
        .repo()
        .join(format!(".config/app/.dotman-tmp-{live}-MnOpQr"));
    let old_style = env.repo().join(".config/app/.dotman-tmp-StUvWx");
    let state_temp = env.state_dir().join(format!(".dotman-tmp-{dead}-EfGhIj"));
    // The user's own files with names like these, and a run's file still being written.
    let notes = env
        .home()
        .join(format!(".config/app/.dotman-tmp-{dead}-notes.txt"));
    let young = env
        .home()
        .join(format!(".config/app/.dotman-tmp-{dead}-YoUnGx"));
    let user_dir = env
        .home()
        .join(format!(".config/app/.dotman-tmp-{dead}-UsErDr"));
    let stale_dir = env
        .home()
        .join(format!(".config/app/.dotman-tmp-{dead}-StAlEd"));
    for (dir, mode) in [(&user_dir, 0o755), (&stale_dir, 0o700)] {
        fs::create_dir(dir).expect("mkdir");
        fs::write(dir.join("keep"), b"precious\n").expect("write");
        fs::set_permissions(dir, fs::Permissions::from_mode(mode)).expect("chmod");
    }
    for path in [
        &repo_temp,
        &home_temp,
        &live_temp,
        &old_style,
        &state_temp,
        &notes,
        &young,
    ] {
        fs::write(path, b"partial\n").expect("leave a temp file");
    }
    // An earlier version committed one; the next sync untracks it.
    let tracked = format!(".config/.dotman-tmp-{dead}-YzAbCd");
    env.write_repo(&tracked, b"committed by mistake\n");
    let repo = repo_path(&env);
    env.git(&["-C", &repo, "add", "-f", &tracked]);
    env.git(&["-C", &repo, "commit", "-q", "-m", "old"]);
    for path in [
        &repo_temp,
        &home_temp,
        &live_temp,
        &old_style,
        &state_temp,
        &notes,
    ] {
        age(path).expect("age the file");
    }
    for path in [&user_dir, &stale_dir, &env.repo().join(&tracked)] {
        age(path).expect("age the file");
    }

    let err = exits(&env, 0, &["sync"]);
    assert!(err.contains("removed 5 temp files"), "{err}");
    assert!(!repo_temp.exists() && !home_temp.exists() && !state_temp.exists());
    assert!(!stale_dir.exists());
    assert!(!env.repo().join(&tracked).exists());
    assert!(
        live_temp.exists() && old_style.exists(),
        "kept: no dead PID"
    );
    assert!(notes.exists() && young.exists() && user_dir.join("keep").exists());
    let remote = env.remote_path().to_string_lossy().into_owned();
    let tree = env.git(&["--git-dir", &remote, "ls-tree", "-r", "--name-only", "main"]);
    assert!(!tree.contains(".dotman-tmp-"), "{tree}");
}

#[test]
fn an_unreadable_file_is_reported_as_not_captured() {
    let env = first_device(&[(".a", b"a\n", 0o644), (".b", b"b\n", 0o644)]);
    env.write(".b", b"b edited\n", 0o644);
    fs::set_permissions(env.home().join(".a"), fs::Permissions::from_mode(0o000)).expect("chmod");
    let err = exits(&env, 1, &["sync"]);
    fs::set_permissions(env.home().join(".a"), fs::Permissions::from_mode(0o644)).expect("chmod");
    assert!(err.contains(".a: cannot be captured"), "{err}");
    assert!(err.contains("1 path could not be captured"), "{err}");
    assert!(!err.contains("could not be applied"), "{err}");
    assert_eq!(remote_file(&env, ".b"), "b edited\n");
}

#[test]
fn a_managed_directory_replaced_by_a_file_gets_a_clear_message() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/nvim/init.lua", b"init\n", 0o644);
    env.write(".config/nvim/lua/opts.lua", b"opts\n", 0o644);
    env.ok(&["add", ".config/nvim"]);
    env.ok(&["sync"]);
    fs::remove_dir_all(env.home().join(".config/nvim/lua")).expect("remove the dir");
    env.write(".config/nvim/lua", b"not a dir\n", 0o644);

    let err = exits(&env, 3, &["sync"]);
    assert!(
        err.contains(".config/nvim/lua: a file here, but a directory of managed files in the repo"),
        "{err}"
    );
    assert!(err.contains("dotman forget .config/nvim/lua"), "{err}");
    assert!(!err.contains("os error"), "{err}");
    assert_eq!(env.read(".config/nvim/lua"), b"not a dir\n");
    assert_eq!(
        fs::read(env.repo().join(".config/nvim/lua/opts.lua")).expect("repo file"),
        b"opts\n"
    );
}

#[test]
fn a_mode_loosened_on_another_machine_is_applied_and_not_kept_private() {
    let (a, b) = pair(&[(".zshrc", b"z\n", 0o600)]);
    assert_eq!(mode_of(&b.home().join(".zshrc")), 0o600);
    // Back to git's default, so the capture removes the `[modes]` entry.
    fs::set_permissions(b.home().join(".zshrc"), fs::Permissions::from_mode(0o644)).expect("chmod");
    exits(&b, 0, &["sync"]);
    let err = exits(&a, 0, &["sync"]);
    assert!(!err.contains("kept at"), "{err}");
    assert_eq!(mode_of(&a.home().join(".zshrc")), 0o644);
    exits(&a, 0, &["sync"]);
    exits(&b, 0, &["sync"]);
    assert_eq!(mode_of(&b.home().join(".zshrc")), 0o644);
}

/// The full shape: `AGE-SECRET-KEY-1` and 58 bech32 characters.
const AGE_KEY: &str = "AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ";

/// B holds `.zshrc` back for an age key, put in by `dotman edit` or by hand in the repo, while A
/// changed the same line upstream.
fn held_against_upstream(by_edit: bool) -> (TestEnv, TestEnv) {
    let (a, b) = pair(&[(".zshrc", b"line1\nline2\n", 0o644)]);
    a.write(".zshrc", b"line1 from a\nline2\n", 0o644);
    exits(&a, 0, &["sync"]);
    if by_edit {
        let editor = b.write_script(
            "ed.sh",
            &format!("#!/bin/sh\nprintf 'export K={AGE_KEY}\\nline2\\n' > \"$1\"\n"),
        );
        let editor = editor.to_string_lossy().into_owned();
        b.ok_with(&[("EDITOR", &editor), ("VISUAL", "")], &["edit", ".zshrc"]);
    } else {
        b.write_repo(".zshrc", format!("{AGE_KEY}\nline2\n").as_bytes());
    }
    (a, b)
}

#[test]
fn a_held_edit_stops_sync_before_the_pull_by_either_route() {
    for by_edit in [false, true] {
        let (a, b) = held_against_upstream(by_edit);
        let home_before = b.read(".zshrc");
        let head = b.git(&["-C", &repo_path(&b), "rev-parse", "HEAD"]);
        let published = b.remote_refs();
        for args in [&["sync", "--dry-run"][..], &["sync"]] {
            let err = exits(&b, 3, args);
            assert!(err.contains("stopped before the pull"), "{err}");
            assert!(
                err.contains("\n  .zshrc: line 1 holds an age secret key"),
                "{err}"
            );
            for remedy in [
                "dotman add --encrypt",
                "remove the secret",
                "dotman git checkout -- <file>",
            ] {
                assert!(err.contains(remedy), "{remedy}: {err}");
            }
        }
        assert_eq!(b.read(".zshrc"), home_before, "nothing written to $HOME");
        assert!(!String::from_utf8_lossy(&b.read(".zshrc")).contains("<<<<<<<"));
        assert_eq!(b.git(&["-C", &repo_path(&b), "rev-parse", "HEAD"]), head);
        assert_eq!(b.remote_refs(), published);
        let remote = a.remote_path().to_string_lossy().into_owned();
        let history = a.git(&["--git-dir", &remote, "log", "--all", "-p"]);
        assert!(!history.contains("AGE-SECRET-KEY-"), "{history}");

        b.ok(&["git", "checkout", "--", ".zshrc"]);
        exits(&b, 0, &["sync", "--take-repo"]);
        assert_eq!(b.read(".zshrc"), b"line1 from a\nline2\n");
    }
}

#[test]
fn an_edit_that_does_not_go_back_after_the_pull_stops_before_home_is_written() {
    let (a, mut b) = pair(&[(".zshrc", b"line1\nline2\n", 0o644)]);
    a.write(".zshrc", b"line1 from a\nline2\n", 0o644);
    exits(&a, 0, &["sync"]);
    let real = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH");
    // An edit that lands in the repo between sync's commit and its pull, as an editor saving then.
    let stubs = b.root().join("stubs");
    fs::create_dir_all(&stubs).expect("stub dir");
    let wrapper = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do [ \"$arg\" = pull ] && printf 'line1 late edit\\nline2\\n' > '{}'; done\nexec '{}' \"$@\"\n",
        b.repo().join(".zshrc").display(),
        real.display()
    );
    fs::write(stubs.join("git"), wrapper).expect("write the wrapper");
    fs::set_permissions(stubs.join("git"), fs::Permissions::from_mode(0o755)).expect("chmod");
    b.prepend_path(&stubs);

    let published = b.remote_refs();
    let err = exits(&b, 4, &["sync"]);
    assert!(
        err.contains("did not go back on top of it cleanly"),
        "{err}"
    );
    assert!(err.contains("dotman git checkout HEAD -- .zshrc"), "{err}");
    assert!(err.contains("dotman git stash show -p"), "{err}");
    assert_eq!(b.read(".zshrc"), b"line1\nline2\n", "$HOME untouched");
    assert_eq!(b.remote_refs(), published);
}

/// `[ignore]` drops only what git does not track: a managed file that a new pattern matches, and
/// one named to `add` though a default pattern matches it, stay managed on both machines.
#[test]
fn ignore_patterns_never_drop_a_tracked_or_added_file() {
    let (a, b) = pair(&[(".config/app/keep.bak", b"keep\n", 0o644)]);
    a.write(".config/app/merge.orig", b"orig\n", 0o644);
    a.ok(&["add", ".config/app/merge.orig"]);
    let token = |env: &TestEnv, key: &str| env.token_of("status", key);
    assert_eq!(
        token(&a, ".config/app/merge.orig").as_deref(),
        Some("clean")
    );
    let mut config = a.repo_config();
    config.ignore.patterns.push("**/*.bak".to_owned());
    config
        .save(&a.repo().join(".dotman.toml"), false)
        .expect("save .dotman.toml");

    let err = exits(&a, 0, &["sync"]);
    assert!(!err.contains("forgotten"), "{err}");
    assert_eq!(remote_file(&a, ".config/app/merge.orig"), "orig\n");
    let err = exits(&b, 0, &["sync"]);
    assert!(!err.contains("forgotten"), "{err}");
    assert_eq!(b.read(".config/app/merge.orig"), b"orig\n");
    for env in [&a, &b] {
        for key in [".config/app/keep.bak", ".config/app/merge.orig"] {
            assert_eq!(token(env, key).as_deref(), Some("clean"), "{key}");
        }
    }

    a.write(".config/app/keep.bak", b"keep, edited\n", 0o644);
    exits(&a, 0, &["sync"]);
    exits(&b, 0, &["sync"]);
    assert_eq!(b.read(".config/app/keep.bak"), b"keep, edited\n");
}

/// B's own copies are stricter than the repo says. `init --take-repo` keeps their modes, B reads
/// as clean and its sync commits nothing, A keeps the repo's modes, and upstream edits still reach
/// B at its mode. A `[modes]` entry then applies on every machine.
#[test]
fn a_stricter_mode_kept_on_one_machine_stays_on_that_machine() {
    let a = first_device(&[
        (".bashrc", b"v1\n", 0o644),
        ("bin/tool", b"#!/bin/sh\n", 0o755),
    ]);
    let b = a.second_device();
    b.write(".bashrc", b"B's own\n", 0o600);
    b.write("bin/tool", b"#!/bin/sh\necho B\n", 0o700);
    let err = exits(&b, 0, &["init", &b.remote_url(), "--take-repo"]);
    assert!(
        err.contains(".bashrc: kept at 0600 here; the repo says 0644"),
        "{err}"
    );
    let mode = |env: &TestEnv, key: &str| mode_of(&env.home().join(key));
    assert_eq!((mode(&b, ".bashrc"), mode(&b, "bin/tool")), (0o600, 0o700));
    let err = exits(&b, 0, &["status"]);
    assert!(
        err.contains("bin/tool: kept at 0700 here; the repo says 0755"),
        "{err}"
    );
    for key in [".bashrc", "bin/tool"] {
        assert_eq!(b.token_of("status", key).as_deref(), Some("clean"), "{key}");
    }
    let head = |env: &TestEnv| env.git(&["-C", &repo_path(env), "rev-parse", "HEAD"]);
    let before = head(&b);
    exits(&b, 0, &["sync"]);
    assert_eq!(head(&b), before, "B's sync commits nothing");
    assert!(
        b.repo_config().modes.is_empty(),
        "{:?}",
        b.repo_config().modes
    );
    exits(&a, 0, &["sync"]);
    assert_eq!((mode(&a, ".bashrc"), mode(&a, "bin/tool")), (0o644, 0o755));

    a.write(".bashrc", b"v2\n", 0o644);
    exits(&a, 0, &["sync"]);
    exits(&b, 0, &["sync"]);
    assert_eq!(b.read(".bashrc"), b"v2\n");
    assert_eq!(mode(&b, ".bashrc"), 0o600);
    assert_eq!(b.token_of("status", ".bashrc").as_deref(), Some("clean"));

    // An edit on B travels as content only.
    b.write(".bashrc", b"v3 from b\n", 0o600);
    exits(&b, 0, &["sync"]);
    assert!(
        b.repo_config().modes.is_empty(),
        "{:?}",
        b.repo_config().modes
    );
    assert_eq!(b.token_of("status", ".bashrc").as_deref(), Some("clean"));
    exits(&a, 0, &["sync"]);
    assert_eq!(a.read(".bashrc"), b"v3 from b\n");
    assert_eq!(mode(&a, ".bashrc"), 0o644);

    let mut config = a.repo_config();
    config.modes.insert(".bashrc".to_owned(), "0644".to_owned());
    config
        .save(&a.repo().join(".dotman.toml"), false)
        .expect("save .dotman.toml");
    exits(&a, 0, &["sync"]);
    exits(&b, 0, &["sync"]);
    assert_eq!(mode(&b, ".bashrc"), 0o644);
    assert_eq!(b.token_of("status", ".bashrc").as_deref(), Some("clean"));
}

/// Both passes of one sync replace the same file: B's local edit before the pull, then the
/// version the pull brings. Each backup lands in its own directory.
#[test]
fn a_file_backed_up_by_both_passes_of_a_sync_keeps_both_copies() {
    let (a, b) = pair(&[(".bashrc", b"v1\n", 0o644)]);
    a.write(".bashrc", b"v2\n", 0o644);
    exits(&a, 0, &["sync"]);
    // B's repo is ahead of its state, as after a pull by hand.
    b.ok(&["git", "pull", "-q", "--rebase", "origin", "main"]);
    b.write(".bashrc", b"B local edit\n", 0o644);
    a.write(".bashrc", b"v3\n", 0o644);
    exits(&a, 0, &["sync"]);

    exits(&b, 0, &["sync", "--take-repo"]);
    assert_eq!(b.read(".bashrc"), b"v3\n");
    let mut copies: Vec<Vec<u8>> = b
        .backups(".bashrc")
        .into_iter()
        .map(|(bytes, _)| bytes)
        .collect();
    copies.sort();
    assert_eq!(copies, [b"B local edit\n".to_vec(), b"v2\n".to_vec()]);
}

/// Two machines that each ran a fresh `init --remote` share no history. The printed recipe, run
/// as printed, keeps the file only the refused repo has.
#[test]
fn the_unrelated_history_recipe_keeps_the_files_only_this_repo_has() {
    let a = first_device(&[(".bashrc", b"a\n", 0o644), (".onlyA", b"only a\n", 0o644)]);
    let b = a.second_device();
    b.ok(&["init", "--remote", &b.remote_url()]);
    b.write(".bashrc", b"b\n", 0o644);
    b.write(".onlyB", b"only b\n", 0o644);
    b.ok(&["add", ".bashrc", ".onlyB"]);
    let err = b.fails(&["sync"]);
    let step = err
        .lines()
        .find(|line| line.contains("import --overlay"))
        .expect("the overlay step");
    assert!(step.contains("--include-missing"), "{err}");

    let repo = repo_path(&b);
    let old = format!("{repo}.old");
    fs::rename(&repo, &old).expect("move the repo aside");
    b.ok(&["import", &b.remote_url()]);
    let out = b.run(&["import", "--overlay", &old, "--include-missing"]);
    assert!(matches!(out.status.code(), Some(0 | 3)), "{}", stderr(&out));
    assert_eq!(
        fs::read(b.repo().join(".onlyB")).expect("in the repo"),
        b"only b\n"
    );
}

/// A shallow clone cut at the commit that a related force-push then replaced lacks the commit it
/// shares with the remote, so sync deepens it before deciding, and pulls.
#[test]
fn a_shallow_repo_is_deepened_before_it_is_called_unrelated() {
    let a = first_device(&[(".bashrc", b"v1\n", 0o644), (".profile", b"p1\n", 0o644)]);
    a.write(".bashrc", b"v2\n", 0o644);
    exits(&a, 0, &["sync"]);
    let d = a.second_device();
    let repo = repo_path(&d);
    d.git(&["clone", "-q", "--depth", "1", &d.remote_url(), &repo]);
    d.ok(&["init"]);
    d.ok(&["apply", "--take-repo"]);

    a.write_repo(".bashrc", b"v2 amended\n");
    let a_repo = repo_path(&a);
    a.git(&[
        "-C", &a_repo, "commit", "-q", "-a", "--amend", "-m", "amended",
    ]);
    a.git(&["-C", &a_repo, "push", "-q", "-f", "origin", "main"]);
    d.write(".profile", b"p1, edited on d\n", 0o644);
    let err = exits(&d, 0, &["sync"]);
    assert!(!err.contains("shares no history"), "{err}");
    assert_eq!(d.read(".bashrc"), b"v2 amended\n");
    assert_eq!(remote_file(&a, ".profile"), "p1, edited on d\n");
}

/// An editor holds `.bashrc` in a buffer while a sync pulls a new version of it, then writes the
/// buffer back over it. `edit` saves the edit aside, puts the pulled version back and says so, and
/// the next sync reverts nothing.
#[test]
fn an_edit_made_while_a_sync_pulled_the_same_file_reverts_nothing() {
    let (a, b) = pair(&[(".bashrc", b"line1\nline2\n", 0o644)]);
    a.write(".bashrc", b"line1\nline2\nA fix\n", 0o644);
    exits(&a, 0, &["sync"]);
    let started = b.root().join("editor-started");
    let resume = b.root().join("editor-resume");
    let editor = b.write_script(
        "bufed.sh",
        &format!(
            "#!/bin/sh\nbuf=$(cat \"$1\")\n: > '{}'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\nprintf '%s\\nB added\\n' \"$buf\" > \"$1\"\n",
            started.display(),
            resume.display()
        ),
    );
    let child = b
        .dot_process()
        .args(["edit", ".bashrc"])
        .env("EDITOR", &editor)
        .env("VISUAL", "")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start dotman edit");
    let waiting = std::time::Instant::now();
    while !started.exists() {
        assert!(waiting.elapsed() < std::time::Duration::from_secs(60));
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    exits(&b, 0, &["sync"]);
    fs::write(&resume, b"").expect("let the editor finish");
    let out = child.wait_with_output().expect("wait for dotman edit");

    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains(".bashrc changed while the editor was open"),
        "{err}"
    );
    assert!(err.contains("moved HEAD from"), "{err}");
    assert!(err.contains("diff -u"), "{err}");
    assert!(err.contains("dotman edit .bashrc"), "{err}");
    assert!(!err.contains("dotman sync"), "{err}");
    assert_eq!(
        b.backups(".dotman/edit/.bashrc"),
        [(b"line1\nline2\nB added\n".to_vec(), 0o600)]
    );
    assert_eq!(
        fs::read(b.repo().join(".bashrc")).expect("repo copy"),
        b"line1\nline2\nA fix\n"
    );
    exits(&b, 0, &["sync"]);
    assert_eq!(remote_file(&a, ".bashrc"), "line1\nline2\nA fix\n");
}
