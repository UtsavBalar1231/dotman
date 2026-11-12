mod common;

use common::{TestEnv, assert_converged, mode_of, tree_checksum};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
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

/// The commands a message gives in backticks, in order.
fn backticked(err: &str) -> Vec<String> {
    err.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

/// The numbered steps of a printed recipe with the number taken off. The lines that stand below a
/// step are joined to it with newlines, their indentation trimmed.
fn steps(err: &str) -> Vec<String> {
    let mut steps: Vec<String> = Vec::new();
    for line in err.lines() {
        let numbered = line
            .strip_prefix("  ")
            .and_then(|rest| rest.split_once(". "))
            .filter(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if let Some((_, text)) = numbered {
            steps.push(text.to_owned());
        } else if let (Some(more), Some(last)) = (line.strip_prefix("     "), steps.last_mut()) {
            last.push('\n');
            last.push_str(more.trim_start());
        }
    }
    steps
}

/// Runs `line` as it runs pasted into a shell, with this build's `dotman` first on PATH. A trailing
/// comment or a parenthesised note does not survive a paste into every shell, and an editor that
/// fails stands for one that cannot open here.
fn paste(env: &TestEnv, line: &str) -> Output {
    paste_with(env, &[], line)
}

/// Like [`paste`], with extra environment variables.
// reason: a shared helper, and clippy.toml exempts `expect` only inside `#[test]` functions.
#[allow(clippy::expect_used)]
fn paste_with(env: &TestEnv, vars: &[(&str, &str)], line: &str) -> Output {
    assert!(
        !line.contains(" #") && !line.contains(" ("),
        "not pasteable: {line}"
    );
    let bin = Path::new(env!("CARGO_BIN_EXE_dotman"))
        .parent()
        .expect("the binary's directory");
    env.process_for(Path::new("sh"))
        .env("EDITOR", "false")
        .env("VISUAL", "false")
        .envs(vars.iter().copied())
        .arg("-c")
        .arg(format!(
            "PATH={}:\"$PATH\"; {line}",
            shell_words::quote(&bin.to_string_lossy())
        ))
        .output()
        .expect("run sh")
}

/// Puts a `git` in front of PATH that runs the shell snippet `before`, with the arguments in `$*`,
/// and then the real git.
// reason: a shared helper, and clippy.toml exempts `expect` only inside `#[test]` functions.
#[allow(clippy::expect_used)]
fn wrap_git(env: &mut TestEnv, before: &str) {
    let real = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH");
    let stubs = env.root().join("git-wrapper");
    fs::create_dir_all(&stubs).expect("stub dir");
    let wrapper = format!("#!/bin/sh\n{before}\nexec '{}' \"$@\"\n", real.display());
    fs::write(stubs.join("git"), wrapper).expect("write the wrapper");
    fs::set_permissions(stubs.join("git"), fs::Permissions::from_mode(0o755)).expect("chmod");
    env.prepend_path(&stubs);
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

    // A repo whose first sync never pushed has no upstream, and the steps must not need one.
    b.ok(&["git", "branch", "--unset-upstream"]);
    b.write(".a", b"from b\n", 0o644);
    let err = exits(&b, 4, &["sync"]);
    let [pull, edit, add, resume, sync]: [String; 5] = steps(&err).try_into().expect("five steps");
    assert_eq!(pull, "dotman git pull --rebase origin main");
    assert_eq!(
        edit,
        format!("Edit the conflicting files under {}.", repo_path(&b))
    );
    assert_eq!(add, "dotman git add -- .a");
    assert_eq!(resume, "dotman git -c core.editor=true rebase --continue");
    assert_eq!(sync, "dotman sync");
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

    let out = paste(&b, &pull);
    assert!(!out.status.success(), "the pull stops on the conflict");
    // git before 2.26 rebases with the apply backend.
    assert!(
        b.repo().join(".git/rebase-merge").exists() || b.repo().join(".git/rebase-apply").exists(),
        "{}",
        stderr(&out)
    );
    exits(&b, 4, &["status"]);
    b.write_repo(".a", b"resolved\n");
    for step in [&add, &resume] {
        let out = paste(&b, step);
        assert!(out.status.success(), "{step}: {}", stderr(&out));
    }

    // S still equals B's edit, so only the always-back-up rule keeps those bytes outside history.
    let out = paste(&b, &sync);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("backed up to"), "{err}");
    assert_eq!(b.read(".a"), b"resolved\n");
    assert_eq!(b.backups(".a"), [(b"from b\n".to_vec(), 0o600)]);
    exits(&a, 0, &["sync"]);
    assert_eq!(a.read(".a"), b"resolved\n");
    assert_converged(&a, &b);
}

/// git reads a path that starts with a dash as an option unless `--` comes first.
#[test]
fn a_conflict_on_a_key_that_starts_with_a_dash_resolves_with_the_printed_steps() {
    let a = first_device(&[(".a", b"a\n", 0o644)]);
    a.write("-x", b"base\n", 0o644);
    a.ok(&["add", "./-x"]);
    a.ok(&["sync"]);
    let b = clone_to_new_device(&a);
    a.write("-x", b"from a\n", 0o644);
    exits(&a, 0, &["sync"]);
    b.write("-x", b"from b\n", 0o644);
    let err = exits(&b, 4, &["sync"]);
    let [pull, _, add, resume, sync]: [String; 5] = steps(&err).try_into().expect("five steps");
    assert_eq!(add, "dotman git add -- -x");

    assert!(!paste(&b, &pull).status.success(), "the pull stops");
    b.write_repo("-x", b"resolved\n");
    for step in [&add, &resume, &sync] {
        let out = paste(&b, step);
        assert!(out.status.success(), "{step}: {}", stderr(&out));
    }
    exits(&a, 0, &["sync"]);
    assert_eq!(a.read("-x"), b"resolved\n");
    assert_eq!(b.read("-x"), b"resolved\n");
}

/// A conflict in `.dotman.toml` leaves conflict markers in it until the rebase is aborted, and
/// untracked junk in the worktree must not make sync read it then: the rebase is still aborted.
#[test]
fn a_conflict_in_dotman_toml_is_aborted_with_junk_in_the_worktree() {
    let (a, b) = pair(&[(".a", b"a\n", 0o644)]);
    for (env, pattern) in [(&a, "**/*.from-a"), (&b, "**/*.from-b")] {
        let mut config = env.repo_config();
        config.ignore.patterns.push(pattern.to_owned());
        config
            .save(&env.repo().join(".dotman.toml"), false)
            .expect("save .dotman.toml");
    }
    exits(&a, 0, &["sync"]);
    b.write_repo(".bashrc.orig", b"junk\n");

    let err = exits(&b, 4, &["sync"]);
    assert!(err.contains("conflicting paths:\n  .dotman.toml"), "{err}");
    assert!(!b.repo().join(".git/rebase-merge").exists());
    assert!(!b.repo().join(".git/rebase-apply").exists());
    exits(&b, 0, &["status"]);
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
    assert!(
        err.contains("dotman git -c core.editor=true rebase --continue"),
        "{err}"
    );
    assert!(!b.home().join(".d").exists());
}

const SEALED: &str = ".config/app/s.conf";

/// Two devices that share a key, with [`SEALED`] synced encrypted.
fn sealed_pair() -> (TestEnv, TestEnv) {
    let a = first_device(&[(".plain", b"p\n", 0o644)]);
    a.ok(&["key", "init"]);
    a.write(SEALED, b"base\n", 0o600);
    a.ok(&["add", "--encrypt", SEALED]);
    exits(&a, 0, &["sync"]);
    let b = a.second_device();
    b.ok(&["init", &b.remote_url()]);
    let identity = a.xdg_config().join("dotman/age-identity.txt");
    b.ok(&["key", "import", &identity.to_string_lossy()]);
    exits(&b, 0, &["apply"]);
    assert_eq!(b.read(SEALED), b"base\n");
    (a, b)
}

/// Both devices edit an encrypted file. The steps say which side `--ours` and `--theirs` stand for
/// and keep one version whole; run as printed, either pick reaches both devices, and the plaintext
/// it replaces is backed up. Git before 2.26, whose apply backend `rebase.backend=apply` stands in
/// for, stops on the commit that taking the upstream version empties, and the printed skip goes on.
#[test]
fn an_encrypted_conflict_keeps_the_version_the_printed_step_picks() {
    if !common::age_installed() {
        return;
    }
    let apply_backend = format!("{} 'rebase.backend=apply'", common::GIT_PARAMETERS);
    let older = [("GIT_CONFIG_PARAMETERS", apply_backend.as_str())];
    for (side, kept, git) in [
        ("--ours", "from a\n", &[][..]),
        ("--theirs", "from b\n", &[][..]),
        ("--ours", "from a\n", &older[..]),
    ] {
        let (a, b) = sealed_pair();
        a.write(SEALED, b"from a\n", 0o600);
        exits(&a, 0, &["sync"]);
        b.write(SEALED, b"from b\n", 0o600);
        let err = exits(&b, 4, &["sync"]);
        assert!(!err.contains("Edit the conflicting"), "{err}");
        let [pull, pick, add, resume, sync]: [String; 5] =
            steps(&err).try_into().expect("five steps");
        assert!(
            pick.starts_with("Encrypted files cannot be merged, so keep one version of each whole. In this rebase, --theirs is this machine's version and --ours is the one from origin/main."),
            "{pick}"
        );
        let file = "'.config/app/s{age}.conf'";
        assert!(
            pick.contains(&format!(
                "To keep this machine's:\ndotman git checkout --theirs -- {file}\nor to take the one from origin/main:\ndotman git checkout --ours -- {file}"
            )),
            "{pick}"
        );
        assert_eq!(add, format!("dotman git add -- {file}"));
        assert!(
            err.contains(&format!(
                "To merge the two versions by hand afterwards, edit {}, where dotman keeps the decrypted file",
                b.home().join(SEALED).display()
            )),
            "{err}"
        );

        let mut resume = resume.lines();
        let (resume, skip) = (resume.next().unwrap_or_default(), resume.next_back());
        assert_eq!(resume, "dotman git -c core.editor=true rebase --continue");
        assert_eq!(skip, Some("dotman git rebase --skip"));

        assert!(
            !paste_with(&b, git, &pull).status.success(),
            "the pull stops"
        );
        let chosen = format!("dotman git checkout {side} -- {file}");
        assert!(pick.lines().any(|line| line == chosen), "{pick}");
        for step in [chosen.as_str(), &add] {
            let out = paste_with(&b, git, step);
            assert!(out.status.success(), "{side} {step}: {}", stderr(&out));
        }
        let out = paste_with(&b, git, resume);
        if git.is_empty() {
            assert!(out.status.success(), "{side}: {}", stderr(&out));
        } else {
            // git am prints it on stdout.
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(said.contains("No changes"), "{said}{}", stderr(&out));
            let out = paste_with(&b, git, skip.unwrap_or_default());
            assert!(out.status.success(), "skip: {}", stderr(&out));
        }
        let out = paste(&b, &sync);
        assert!(out.status.success(), "{side} sync: {}", stderr(&out));
        exits(&a, 0, &["sync"]);
        assert_eq!(b.read(SEALED), kept.as_bytes(), "{side}");
        assert_eq!(a.read(SEALED), kept.as_bytes(), "{side}");
        let (replaced, lost) = if side == "--ours" {
            (&b, "from b\n")
        } else {
            (&a, "from a\n")
        };
        assert!(
            replaced
                .backups(SEALED)
                .iter()
                .any(|(bytes, _)| bytes == lost.as_bytes()),
            "{side}"
        );
        assert_converged(&a, &b);
    }
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

/// The push line counts the commits this sync sent, not those the pull brought, and a sync that
/// sent none says the remote is up to date.
#[test]
fn the_push_line_counts_the_commits_sent() {
    let a = TestEnv::new();
    a.ok(&["init"]);
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write(".a", b"a\n", 0o644);
    a.ok(&["add", ".a"]);
    let err = exits(&a, 0, &["sync"]);
    assert!(err.contains("pushed 1 commit to origin/main\n"), "{err}");
    let err = exits(&a, 0, &["sync"]);
    assert!(err.contains("origin/main is up to date"), "{err}");
    assert!(!err.contains("pushed"), "{err}");

    let remote = a.remote_path().to_string_lossy().into_owned();
    a.git(&[
        "--git-dir",
        &remote,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    let b = clone_to_new_device(&a);
    a.write(".a", b"a2\n", 0o644);
    exits(&a, 0, &["sync"]);
    // Not empty: git before 2.26 drops a commit that starts empty when it rebases.
    b.write_repo(".c", b"c\n");
    b.ok(&["git", "add", "--", ".c"]);
    b.ok(&["git", "commit", "-q", "-m", "by hand"]);
    b.write(".b", b"b\n", 0o644);
    b.ok(&["add", ".b"]);
    let err = exits(&b, 0, &["sync"]);
    assert!(err.contains("pushed 2 commits to origin/main"), "{err}");
    assert_eq!(remote_file(&a, ".b"), "b\n");
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
    assert_eq!(subject().trim(), "sync(testbox): x/a, x/b, x/c (+2 more)");

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

/// The settings and the bootstrap script stay out of a subject that has files to name, so the dry
/// run and the real run list them on lines of their own; alone, the subject names them in words.
#[test]
fn dotmans_own_files_are_listed_and_named_in_words_when_alone() {
    let env = TestEnv::new();
    env.ok_with(&HOST, &["init"]);
    env.ok_with(&HOST, &["init", "--remote", &env.remote_url()]);
    env.write(".a", b"a\n", 0o644);
    env.ok_with(&HOST, &["add", ".a"]);
    let own = [".dotman.toml (settings)", "install.sh (bootstrap script)"];

    let has = |err: &str, line: String| err.lines().any(|l| l == line);
    let err = exits_with(&env, &HOST, 0, &["sync", "--dry-run"]);
    for line in own {
        assert!(has(&err, format!("would commit {line}")), "{err}");
    }
    assert!(err.contains("would commit: sync(testbox): .a\n"), "{err}");
    let err = exits_with(&env, &HOST, 0, &["sync"]);
    for line in own {
        assert!(has(&err, format!("commit {line}")), "{err}");
    }
    assert!(err.contains("committed: sync(testbox): .a\n"), "{err}");

    let mut config = env.repo_config();
    config.ignore.patterns.push("**/*.swp".to_owned());
    config
        .save(&env.repo().join(".dotman.toml"), false)
        .expect("save .dotman.toml");
    let script = env.repo().join("install.sh");
    let mut bytes = fs::read(&script).expect("read install.sh");
    bytes.extend_from_slice(b"# edited\n");
    fs::write(&script, bytes).expect("edit install.sh");
    let err = exits_with(&env, &HOST, 0, &["sync"]);
    assert!(
        err.contains("committed: sync(testbox): settings, bootstrap script\n"),
        "{err}"
    );

    env.write_repo(".dotman/age-recipients", b"age1recipient\n");
    env.write(".a", b"a2\n", 0o644);
    let err = exits_with(&env, &HOST, 0, &["sync"]);
    let line = "commit .dotman/age-recipients (encryption recipients)";
    assert!(has(&err, line.to_owned()), "{err}");
    assert!(err.contains("committed: sync(testbox): .a\n"), "{err}");
}

/// Without a git identity, init, sync and doctor give the same steps. Sync commits nothing, and
/// the global steps, or the repo-only ones they describe, run as printed let it commit.
#[test]
fn a_missing_git_identity_exits_1_with_steps_that_work() {
    let steps = "set them with `git config --global user.name \"Your Name\"` and `git config --global user.email \"you@example.com\"`; to set them for the dotman repo only, run `dotman git config` in place of `git config --global`";
    for global in [true, false] {
        let env = TestEnv::new();
        fs::remove_file(env.home().join(".gitconfig")).expect("remove the identity");
        let err = env.ok(&["init"]);
        assert!(err.contains(steps), "{err}");
        env.write(".a", b"a\n", 0o644);
        env.ok(&["add", ".a"]);
        let err = env.fails(&["sync"]);
        let line = format!("git has no user.name and user.email to commit with; {steps}");
        assert!(err.lines().any(|l| l == line), "{err}");
        assert!(!env.repo().join(".git/refs/heads/main").exists());
        let doctor = env.run(&["doctor"]);
        let report = String::from_utf8_lossy(&doctor.stdout);
        assert!(
            report.contains(&format!(
                "git identity: user.name or user.email is not set; {steps}"
            )),
            "{report}"
        );

        let [name, email, repo_only, in_place]: [String; 4] =
            backticked(steps).try_into().expect("four commands");
        for command in [name, email] {
            let command = if global {
                command
            } else {
                command.replacen(&in_place, &repo_only, 1)
            };
            let out = paste(&env, &command);
            assert!(out.status.success(), "{command}: {}", stderr(&out));
        }
        exits(&env, 0, &["sync"]);
        let repo = repo_path(&env);
        let log = env.git(&["-C", &repo, "log", "--format=%an <%ae>"]);
        assert_eq!(log.trim(), "Your Name <you@example.com>");
        assert_eq!(env.home().join(".gitconfig").exists(), global);
    }
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
        err.contains(
            "answer with `dotman sync --forget-deleted` or `dotman sync --restore-deleted`"
        ),
        "{err}"
    );
    assert_eq!(remote_file(&a, ".a"), "edited\n");
    assert_eq!(remote_file(&a, ".d"), "d\n");

    let out = common::run_hint(&a, &err, "sync --restore-deleted");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(a.read(".d"), b"d\n");
    assert_eq!(a.token_of("status", ".d").as_deref(), Some("clean"));
}

#[test]
fn status_counts_the_commits_to_push_and_pull_and_the_printed_sync_settles_them() {
    let (a, b) = pair(&[(".a", b"a\n", 0o644)]);
    let position = |env: &TestEnv| {
        exits(env, 0, &["status"])
            .lines()
            .filter(|line| line.contains("to push") || line.contains("to pull"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(position(&b), Vec::<String>::new());
    a.write(".a", b"a2\n", 0o644);
    exits(&a, 0, &["sync"]);
    assert_eq!(position(&b), Vec::<String>::new(), "status never fetches");

    exits(&b, 0, &["git", "fetch", "-q", "origin"]);
    assert_eq!(position(&b), ["1 commit to pull: run `dotman sync`"]);
    for key in [".b", ".c"] {
        b.write_repo(key, b"local\n");
        exits(&b, 0, &["git", "add", key]);
        exits(&b, 0, &["git", "commit", "-q", "-m", key]);
    }
    let err = exits(&b, 0, &["status"]);
    assert_eq!(
        position(&b),
        ["2 commits to push, 1 to pull: run `dotman sync`"]
    );

    let out = common::run_hint(&b, &err, "sync");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(position(&b), Vec::<String>::new());
    assert_eq!(b.read(".a"), b"a2\n");
    exits(&a, 0, &["git", "fetch", "-q", "origin"]);
    assert_eq!(position(&a), ["2 commits to pull: run `dotman sync`"]);
}

/// With no `origin` ref at all, as after every push failed, nothing was fetched, so status cannot
/// count what origin lacks and says so; the printed sync pushes the commits.
#[test]
fn status_says_origin_was_never_fetched_and_the_printed_sync_pushes() {
    let env = TestEnv::new();
    let later = env.root().join("later.git");
    env.ok(&["init"]);
    env.ok(&["init", "--remote", &format!("file://{}", later.display())]);
    env.write(".a", b"a\n", 0o644);
    env.ok(&["add", ".a"]);
    let err = env.fails(&["sync"]);
    assert!(err.contains("could not reach origin"), "{err}");

    let err = exits(&env, 0, &["status"]);
    let repo = repo_path(&env);
    let line = "origin has not been fetched yet; `dotman sync` fetches and pushes";
    assert!(err.lines().any(|l| l == line), "{err}");
    assert!(!err.contains("not pushed"), "{err}");

    env.git(&["init", "--bare", "-q", &later.to_string_lossy()]);
    let out = common::run_hint(&env, &err, "sync");
    assert!(out.status.success(), "{}", stderr(&out));
    let err = exits(&env, 0, &["status"]);
    assert!(!err.contains("not pushed"), "{err}");
    let pushed = env.git(&["--git-dir", &later.to_string_lossy(), "rev-parse", "main"]);
    assert_eq!(pushed, env.git(&["-C", &repo, "rev-parse", "HEAD"]));
}

/// Before the first fetch, as after an import whose history origin already holds, status does not
/// count the commits origin has as not pushed: it says origin was never fetched, and the next
/// sync pushes only the new one.
#[test]
fn status_never_counts_what_origin_has_before_the_first_fetch() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".a", b"a\n", 0o644);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    env.write(".a", b"a2\n", 0o644);
    env.ok(&["sync"]);
    let repo = repo_path(&env);
    env.git(&["-C", &repo, "update-ref", "-d", "refs/remotes/origin/main"]);
    // Not empty: git before 2.26 drops a commit that starts empty when it rebases.
    env.write_repo(".c", b"c\n");
    env.git(&["-C", &repo, "add", "--", ".c"]);
    env.git(&["-C", &repo, "commit", "-q", "-m", "local"]);

    let err = exits(&env, 0, &["status"]);
    let line = "origin has not been fetched yet; `dotman sync` fetches and pushes";
    assert!(err.lines().any(|l| l == line), "{err}");
    assert!(!err.contains("not pushed"), "{err}");
    let err = exits(&env, 0, &["sync"]);
    assert!(err.contains("pushed 1 commit to origin/main"), "{err}");
    let err = exits(&env, 0, &["status"]);
    assert!(!err.contains("fetched") && !err.contains("push"), "{err}");
}

/// Once origin was fetched and it lacks the branch, status counts what origin lacks, against
/// `origin` alone: a second remote that holds every commit does not hide that `origin` has none
/// of them.
#[test]
fn status_counts_what_origin_lacks_even_when_another_remote_has_it() {
    let env = TestEnv::new();
    let later = env.root().join("later.git");
    let backup = env.root().join("backup.git");
    env.ok(&["init"]);
    env.write(".a", b"a\n", 0o644);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    let repo = repo_path(&env);
    env.git(&["init", "--bare", "-q", &backup.to_string_lossy()]);
    env.git(&[
        "-C",
        &repo,
        "remote",
        "add",
        "backup",
        &backup.to_string_lossy(),
    ]);
    env.git(&["-C", &repo, "push", "-q", "backup", "main"]);
    env.git(&["init", "--bare", "-q", &later.to_string_lossy()]);
    let other = env.root().join("other");
    let other = other.to_string_lossy().into_owned();
    env.git(&["init", "-q", &other]);
    env.git(&["-C", &other, "commit", "-q", "--allow-empty", "-m", "other"]);
    let later_path = later.to_string_lossy().into_owned();
    env.git(&[
        "-C",
        &other,
        "push",
        "-q",
        &later_path,
        "HEAD:refs/heads/other",
    ]);
    env.ok(&["init", "--remote", &format!("file://{}", later.display())]);
    exits(&env, 0, &["git", "fetch", "-q", "origin"]);

    let err = exits(&env, 0, &["status"]);
    let count = env.git(&["-C", &repo, "rev-list", "--count", "HEAD"]);
    assert_ne!(count.trim(), "0");
    let commits = if count.trim() == "1" {
        "commit"
    } else {
        "commits"
    };
    let line = format!(
        "{} {commits} not pushed yet: run `dotman sync`",
        count.trim()
    );
    assert!(err.lines().any(|l| l == line), "{err}");
}

#[test]
fn the_printed_answer_to_a_conflict_in_sync_does_what_it_says() {
    for (answer, home) in [
        ("sync --keep-local", "mine\n"),
        ("sync --take-repo", "theirs\n"),
    ] {
        let (a, b) = pair(&[(".a", b"base\n", 0o644)]);
        a.write(".a", b"theirs\n", 0o644);
        exits(&a, 0, &["sync"]);
        b.write(".a", b"mine\n", 0o644);
        exits(&b, 0, &["git", "fetch", "-q", "origin"]);
        exits(&b, 0, &["git", "merge", "-q", "--ff-only", "origin/main"]);
        let err = exits(&b, 3, &["sync"]);
        assert!(err.contains("conflicts left undecided (1)"), "{err}");
        let out = common::run_hint(&b, &err, answer);
        assert!(out.status.success(), "{answer}: {}", stderr(&out));
        assert_eq!(b.read(".a"), home.as_bytes(), "{answer}");
        assert_eq!(remote_file(&b, ".a"), home, "{answer}");
        assert_eq!(b.token_of("status", ".a").as_deref(), Some("clean"));
    }
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

/// Where the message in `err` says the repo copy's uncommitted edit was saved.
fn saved_at(err: &str) -> std::path::PathBuf {
    let marker = "the repo copy's uncommitted edit is saved at ";
    let path = err
        .lines()
        .find_map(|line| line.split_once(marker).map(|(_, path)| path.to_owned()));
    assert!(path.is_some(), "{err}");
    path.unwrap_or_default().into()
}

#[test]
fn keep_local_saves_an_uncommitted_hand_edit_in_the_repo_before_capturing_over_it() {
    let env = first_device(&[(".a", b"base\n", 0o644)]);
    env.write_repo(".a", b"hand edit in the repo\n");
    env.write(".a", b"home edit\n", 0o644);
    assert_eq!(
        env.token_of("status", ".a").as_deref(),
        Some("both-changed")
    );

    let err = exits(&env, 0, &["sync", "--keep-local", "--dry-run"]);
    assert!(
        err.contains(".a: would save the repo copy's uncommitted edit at "),
        "{err}"
    );
    assert!(!env.state_dir().join("backups").exists(), "{err}");

    let err = exits(&env, 0, &["sync", "--keep-local"]);
    let saved = saved_at(&err);
    assert!(saved.starts_with(env.state_dir().join("backups")), "{err}");
    assert_eq!(
        fs::read(&saved).expect("the saved edit"),
        b"hand edit in the repo\n"
    );
    assert_eq!(env.read(".a"), b"home edit\n");
    assert_eq!(remote_file(&env, ".a"), "home edit\n");
    assert!(
        env.backups(".a").is_empty(),
        "never where a home backup goes"
    );
}

/// Machine A, then machine B named `box`, whose `.a` and `.b` conflict in its capture pass: A's
/// edit reached B's repo before B's own edit was captured.
fn kept_pair() -> (TestEnv, TestEnv) {
    let a = first_device(&[(".a", b"base\n", 0o644), (".b", b"base\n", 0o644)]);
    let b = a.second_device();
    b.write_machine_config("[identity]\nhost = \"box\"\n");
    b.ok(&["init", &b.remote_url(), "--take-repo"]);
    for key in [".a", ".b"] {
        a.write(key, b"from a\n", 0o644);
        b.write(key, b"mine\n", 0o644);
    }
    exits(&a, 0, &["sync"]);
    exits(&b, 0, &["git", "pull", "-q", "--rebase", "origin", "main"]);
    for key in [".a", ".b"] {
        assert_eq!(b.token_of("status", key).as_deref(), Some("both-changed"));
    }
    (a, b)
}

/// The capture is pushed by the time sync ends, so no step printed after it can keep the file to
/// this machine.
#[test]
fn keep_local_in_sync_replaces_the_repo_version_for_every_machine_and_says_so() {
    let (a, b) = kept_pair();
    let err = exits(&b, 0, &["sync", "--keep-local"]);
    for key in [".a", ".b"] {
        let line = format!("capture {key} (replaces the repo version for every machine)");
        assert!(err.lines().any(|l| l == line), "{err}");
        assert_eq!(remote_file(&b, key), "mine\n");
    }
    assert!(!err.contains("--for host="), "{err}");

    exits(&a, 0, &["sync"]);
    for key in [".a", ".b"] {
        assert_eq!(a.read(key), b"mine\n");
        assert_eq!(a.backups(key), [(b"from a\n".to_vec(), 0o600)]);
    }
}

/// Offered while the paths are still undecided, the printed `add --for` keeps B's version to B
/// and leaves the shared file and machine A as they were.
#[test]
fn the_way_to_keep_a_conflict_to_this_machine_is_offered_before_the_capture_and_works() {
    let (a, b) = kept_pair();
    let err = exits(&b, 3, &["sync"]);
    assert!(err.contains("conflicts left undecided (2)"), "{err}");
    assert!(
        err.contains("--keep-local captures this machine's version into the repo for every machine; `dotman add --for host=box "),
        "{err}"
    );
    assert_eq!(
        common::hint(&err, "add --for host=box").len(),
        5,
        "both paths in one command: {err}"
    );
    assert_eq!(remote_file(&b, ".a"), "from a\n", "{err}");

    let out = common::run_hint(&b, &err, "add --for host=box");
    assert!(out.status.success(), "{}", stderr(&out));
    exits(&b, 0, &["sync"]);
    exits(&a, 0, &["sync"]);
    for key in [".a", ".b"] {
        assert_eq!(remote_file(&b, key), "from a\n");
        assert_eq!(remote_file(&b, &format!("{key}{{host=box}}")), "mine\n");
        assert_eq!(a.read(key), b"from a\n");
        assert_eq!(a.backups(key), Vec::new());
        assert_eq!(b.read(key), b"mine\n");
        assert_eq!(b.token_of("status", key).as_deref(), Some("clean"));
        assert_eq!(a.token_of("status", key).as_deref(), Some("clean"));
    }

    let Some(seen) = common::in_a_terminal(&kept_pair().1, &[], &["sync"], b"s\n") else {
        return;
    };
    assert!(
        seen.contains("[k] captures this machine's versions into the repo for every machine; to keep them to this machine only, answer [s] and run `dotman add --for host=box "),
        "{seen}"
    );
}

const OWN: &str = ".a{host=box+profile=work}";

/// Machine A, then machine `box` with the profile `work`, which applies its own `OWN` for `.a` and
/// shares `.b`. A edits `OWN` in the repo and `.b` in `$HOME`, and box edits both in `$HOME`, so
/// both conflict in box's capture pass.
fn own_variant_pair() -> (TestEnv, TestEnv) {
    let a = first_device(&[(".a", b"base\n", 0o644), (".b", b"base\n", 0o644)]);
    let b = a.second_device();
    b.write_machine_config("profiles = [\"work\"]\n[identity]\nhost = \"box\"\n");
    b.ok(&["init", &b.remote_url(), "--take-repo"]);
    b.write(".a", b"box's own\n", 0o644);
    b.ok(&["add", "--for", "host=box+profile=work", ".a"]);
    b.ok(&["sync"]);
    exits(&a, 0, &["sync"]);
    a.write_repo(OWN, b"from a\n");
    a.write(".b", b"from a\n", 0o644);
    exits(&a, 0, &["sync"]);
    for key in [".a", ".b"] {
        b.write(key, b"mine\n", 0o644);
    }
    exits(&b, 0, &["git", "pull", "-q", "--rebase", "origin", "main"]);
    for key in [".a", ".b"] {
        assert_eq!(b.token_of("status", key).as_deref(), Some("both-changed"));
    }
    (a, b)
}

/// Keeping box's `.a` captures it into `OWN`, which no other machine applies, so no `add --for`
/// is offered for it; `.b` goes into the repo for every machine, and the printed step for it alone
/// keeps it to box.
#[test]
fn keeping_a_file_whose_winner_names_this_host_says_where_it_goes_and_offers_no_step() {
    let (a, b) = own_variant_pair();
    let err = exits(&b, 3, &["sync"]);
    let own = format!("--keep-local captures .a into {OWN}, which only this machine applies");
    assert!(err.lines().any(|line| line == own), "{err}");
    assert!(
        err.contains("--keep-local captures this machine's version of .b into the repo for every machine; `dotman add --for host=box "),
        "{err}"
    );
    let step = common::hint(&err, "add --for host=box");
    assert_eq!(step.len(), 4, "only .b: {err}");
    assert!(
        step.last().is_some_and(|path| path.ends_with("/.b")),
        "{err}"
    );
    let out = common::run_hint(&b, &err, "add --for host=box");
    assert!(out.status.success(), "{}", stderr(&out));

    let err = exits(&b, 0, &["sync", "--keep-local"]);
    let line = format!("capture .a (replaces {OWN}, which only this machine applies)");
    assert!(err.lines().any(|l| l == line), "{err}");
    assert!(!err.contains("for every machine"), "{err}");
    assert_eq!(remote_file(&b, OWN), "mine\n");
    assert_eq!(remote_file(&b, ".a"), "base\n");
    assert_eq!(remote_file(&b, ".b{host=box}"), "mine\n");
    assert_eq!(remote_file(&b, ".b"), "from a\n");
    exits(&a, 0, &["sync"]);
    assert_eq!(a.read(".a"), b"base\n");
    assert_eq!(a.read(".b"), b"from a\n");
    for key in [".a", ".b"] {
        assert_eq!(b.read(key), b"mine\n");
        assert_eq!(b.token_of("status", key).as_deref(), Some("clean"));
    }
}

#[test]
fn apply_keep_local_of_a_file_whose_winner_names_this_host_says_where_the_next_sync_puts_it() {
    let (_, b) = own_variant_pair();
    let err = exits(&b, 0, &["apply", "--keep-local"]);
    let own = format!(
        "kept; the next `dotman sync` captures .a into {OWN}, which only this machine applies"
    );
    assert!(err.lines().any(|line| line == own), "{err}");
    assert!(
        err.contains("kept; the next `dotman sync` captures .b into the repo for every machine; `dotman add --for host=box "),
        "{err}"
    );
    assert_eq!(common::hint(&err, "add --for host=box").len(), 4, "{err}");

    let Some(seen) = common::in_a_terminal(&own_variant_pair().1, &[], &["sync"], b"s\n") else {
        return;
    };
    assert!(
        seen.contains(&format!(
            "[k] captures .a into {OWN}, which only this machine applies"
        )),
        "{seen}"
    );
    assert!(
        seen.contains("[k] captures this machine's version of .b into the repo for every machine; to keep it to this machine only, answer [s] and run `dotman add --for host=box "),
        "{seen}"
    );
    assert!(
        seen.contains("[k] keep all local (capture them into the repo)  [t]"),
        "{seen}"
    );

    let Some(seen) = common::in_a_terminal(&own_variant_pair().1, &[], &["sync"], b"p\ns\ns\n")
    else {
        return;
    };
    assert!(
        seen.contains(".a [k] keep local (capture it into this machine's own variant)  [t]"),
        "{seen}"
    );
    assert!(
        seen.contains(".b [k] keep local (capture it into the repo for every machine)  [t]"),
        "{seen}"
    );
}

#[test]
fn apply_keep_local_saves_an_edit_made_with_dotman_edit_before_sync_replaces_it() {
    let env = first_device(&[(".a", b"base\n", 0o644)]);
    env.write(".a", b"home edit\n", 0o644);
    let editor = env.write_script(
        "editor.sh",
        "#!/bin/sh\nfor f; do :; done\nprintf 'made with edit\\n' > \"$f\"\n",
    );
    let editor = editor.to_string_lossy().into_owned();
    let out = env.run_with(&[("EDITOR", &editor)], &["edit", ".a"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));

    let err = exits(&env, 0, &["apply", "--keep-local"]);
    let saved = saved_at(&err);
    assert_eq!(
        fs::read(&saved).expect("the saved edit"),
        b"made with edit\n"
    );
    assert!(
        err.contains(".a: the repo copy's uncommitted edit is saved at "),
        "{err}"
    );
    assert!(
        err.contains("kept; the next `dotman sync` captures it into the repo for every machine"),
        "{err}"
    );

    let err = exits(&env, 0, &["sync"]);
    assert!(!err.contains("uncommitted edit"), "saved once: {err}");
    assert_eq!(remote_file(&env, ".a"), "home edit\n");
    assert_eq!(
        fs::read(&saved).expect("the saved edit"),
        b"made with edit\n"
    );
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

/// A bare repo whose `HEAD` is `main`, and an origin for it: `~/dots.git`, which git expands
/// itself, or a URL with credentials.
fn odd_origin(env: &TestEnv, tilde: bool) -> (String, std::path::PathBuf) {
    let bare = if tilde {
        let bare = env.home().join("dots.git");
        env.git(&["init", "-q", "--bare", &bare.to_string_lossy()]);
        bare
    } else {
        env.remote_path()
    };
    let dir = bare.to_string_lossy().into_owned();
    env.git(&["--git-dir", &dir, "symbolic-ref", "HEAD", "refs/heads/main"]);
    let origin = if tilde {
        "~/dots.git".to_owned()
    } else {
        format!("file://user:s3cr3t@localhost{dir}")
    };
    (origin, bare)
}

/// The start-over steps run as printed, from `$HOME`, for an origin starting with `~/`, which
/// neither `git clone` nor a quoted path expands, and for one with credentials, which is shown as
/// `<url>` and never printed.
#[test]
fn the_start_over_steps_run_as_printed_for_a_tilde_or_credential_origin() {
    for tilde in [true, false] {
        let env = TestEnv::new();
        let (origin, bare) = odd_origin(&env, tilde);
        env.ok(&["init", "--no-import", "--remote", &origin]);
        env.write(".a", b"a\n", 0o644);
        env.ok(&["add", ".a"]);
        env.ok(&["sync"]);
        let other = env.root().join("other");
        let dir = other.to_string_lossy().into_owned();
        env.git(&["init", "-q", &dir]);
        fs::write(other.join(".z"), b"only on the remote\n").expect("write");
        env.git(&["-C", &dir, "add", "."]);
        env.git(&["-C", &dir, "commit", "-q", "-m", "other"]);
        let bare = bare.to_string_lossy().into_owned();
        env.git(&[
            "-C",
            &dir,
            "push",
            "-q",
            "-f",
            &bare,
            "HEAD:refs/heads/main",
        ]);
        env.write(".a", b"edited\n", 0o644);

        let err = env.fails(&["sync"]);
        assert!(!err.contains("s3cr3t"), "{err}");
        let block = err
            .split_once("To start over from the remote")
            .and_then(|(_, rest)| rest.split_once("To merge the two histories"))
            .map(|(block, _)| block)
            .unwrap_or_default();
        let steps: Vec<&str> = block
            .lines()
            .filter_map(|line| line.strip_prefix("  "))
            .collect();
        assert_eq!(steps.len(), 3, "{err}");
        assert_eq!(
            block.contains("<url> as the URL with its credentials"),
            !tilde,
            "{err}"
        );
        for step in steps {
            let step = step.replace("<url>", &shell_words::quote(&origin));
            let out = common::paste(&env, &step);
            assert!(out.status.success(), "{step}: {}", stderr(&out));
            assert!(!stderr(&out).contains("s3cr3t"), "{}", stderr(&out));
        }
        let tracked = env.git(&["-C", &repo_path(&env), "ls-files"]);
        assert!(tracked.lines().any(|key| key == ".z"), "{tracked}");
        assert!(tracked.lines().any(|key| key == ".a"), "{tracked}");
    }
}

/// A remote whose `HEAD` names a commit no branch has gives a clone with a detached `HEAD`. sync
/// and doctor refuse it with a way out that runs as printed and moves no branch, so the next sync
/// pushes: a new `main`, a branch already at `HEAD`'s commit, or a new branch when `main` and
/// `detached` are elsewhere.
#[test]
fn a_detached_head_is_refused_with_a_way_out_that_runs_as_printed() {
    let env = TestEnv::new();
    let remote = env.remote_path().to_string_lossy().into_owned();
    let work = env.root().join("work");
    let dir = work.to_string_lossy().into_owned();
    env.git(&["init", "-q", &dir]);
    fs::write(work.join(".a"), b"one\n").expect("write");
    env.git(&["-C", &dir, "add", "."]);
    env.git(&["-C", &dir, "commit", "-q", "-m", "one"]);
    env.git(&["-C", &dir, "push", "-q", &remote, "HEAD:refs/heads/main"]);
    fs::write(work.join(".a"), b"two\n").expect("write");
    env.git(&["-C", &dir, "commit", "-q", "-a", "-m", "two"]);
    env.git(&["-C", &dir, "push", "-q", &remote, "HEAD:refs/heads/tmp"]);
    let tip = env.git(&["-C", &dir, "rev-parse", "HEAD"]);
    fs::write(env.remote_path().join("HEAD"), &tip).expect("detach the remote's HEAD");
    env.git(&["--git-dir", &remote, "update-ref", "-d", "refs/heads/tmp"]);
    env.ok(&["init", &env.remote_url()]);

    let way_out = |fix: &str| {
        let command = format!("`dotman git {fix}`");
        let err = env.fails(&["sync"]);
        assert!(err.contains(&command), "{err}");
        let doctor = env.run(&["doctor"]);
        assert_eq!(doctor.status.code(), Some(1));
        let out = String::from_utf8_lossy(&doctor.stdout).into_owned();
        assert!(
            out.lines()
                .any(|line| line.starts_with("fail  branch: ") && line.contains(&command)),
            "{out}"
        );
        let ran = common::run_hint(&env, &err, "git checkout");
        assert!(ran.status.success(), "{}", stderr(&ran));
        env.ok(&["sync"]);
    };
    way_out("checkout -b main");
    let main = || env.git(&["--git-dir", &remote, "rev-parse", "refs/heads/main"]);
    assert_eq!(main(), tip);

    env.ok(&["git", "checkout", "-q", "--detach"]);
    way_out("checkout main");

    env.ok(&["git", "checkout", "-q", "--detach", "HEAD~1"]);
    way_out("checkout -b detached");
    assert_eq!(main(), tip);
    assert_eq!(env.git(&["-C", &repo_path(&env), "rev-parse", "main"]), tip);

    env.ok(&["git", "checkout", "-q", "--detach"]);
    way_out("checkout detached");

    env.ok(&["git", "checkout", "-q", "--detach"]);
    env.ok(&["git", "commit", "-q", "--allow-empty", "-m", "on no branch"]);
    way_out("checkout -b detached-2");
    assert_eq!(main(), tip);
}

/// A branch `main/x` or `detached/x` holds that name as a directory, so git cannot create the
/// branch; the way out passes over it.
#[test]
fn a_detached_head_way_out_passes_over_a_name_a_branch_holds_as_a_directory() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".a", b"one\n", 0o644);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    let repo = repo_path(&env);
    let branches = || env.git(&["-C", &repo, "for-each-ref", "refs/heads/"]);
    env.ok(&["git", "branch", "-q", "-m", "main", "main/x"]);
    env.ok(&["git", "checkout", "-q", "--detach"]);
    env.ok(&["git", "branch", "-q", "detached/x"]);

    let before = branches();
    let err = env.fails(&["sync"]);
    assert!(err.contains("`dotman git checkout -b detached-2`"), "{err}");
    let doctor = env.run(&["doctor"]);
    let out = String::from_utf8_lossy(&doctor.stdout).into_owned();
    assert!(
        out.lines().any(|line| line.starts_with("fail  branch: ")
            && line.contains("`dotman git checkout -b detached-2`")),
        "{out}"
    );
    let ran = common::run_hint(&env, &err, "git checkout");
    assert!(ran.status.success(), "{}", stderr(&ran));
    assert!(
        before.lines().all(|line| branches().contains(line)),
        "no branch moved: {before}"
    );
    env.ok(&["sync"]);
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

    // Each way out, run as printed.
    let err = env.fails(&["sync"]);
    let fix = err
        .lines()
        .find(|line| line.contains("To bring the files back"))
        .expect("the fix");
    let [restore, allow]: [String; 2] = backticked(fix).try_into().expect("two steps");
    let out = paste(&env, &restore);
    assert!(out.status.success(), "{restore}: {}", stderr(&out));
    exits(&env, 0, &["sync"]);
    assert_eq!(env.remote_refs(), published);
    for key in &keys {
        fs::remove_file(env.repo().join(key)).expect("delete from the worktree");
    }
    let out = paste(&env, &allow);
    assert!(out.status.success(), "{allow}: {}", stderr(&out));
    let remote = env.remote_path().to_string_lossy().into_owned();
    let tree = env.git(&["--git-dir", &remote, "ls-tree", "-r", "--name-only", "main"]);
    assert!(!tree.contains(".config/app/"), "{tree}");
    assert_eq!(env.read(".config/app/f00.conf"), b"x\n");
}

/// `$HOME` still has the files under a `[dirs]` entry, which the next sync would capture again,
/// so the way out that keeps the deletion forgets the entry.
#[test]
fn a_mass_delete_under_dirs_points_to_forget_and_the_printed_route_sticks() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["init", "--remote", &env.remote_url()]);
    for n in 0..12 {
        env.write(format!(".config/app/f{n:02}.conf"), b"x\n", 0o644);
    }
    env.ok(&["add", ".config/app"]);
    env.ok(&["sync"]);
    assert_eq!(env.repo_config().dirs.paths, [".config/app"]);
    fs::remove_dir_all(env.repo().join(".config/app")).expect("delete from the worktree");

    let err = env.fails(&["sync"]);
    let fix = err
        .lines()
        .find(|line| line.contains("To bring the files back"))
        .expect("the fix");
    let [_, forget, allow, _]: [String; 4] = backticked(fix).try_into().expect("four steps");
    assert!(forget.starts_with("dotman forget "), "{fix}");
    assert!(forget.ends_with("/home/.config/app"), "{fix}");
    assert_eq!(allow, "dotman sync --allow-mass-delete");
    for step in [&forget, &allow] {
        let out = paste(&env, step);
        assert!(out.status.success(), "{step}: {}", stderr(&out));
    }

    let remote = env.remote_path().to_string_lossy().into_owned();
    for _ in 0..2 {
        exits(&env, 0, &["sync"]);
        let tree = env.git(&["--git-dir", &remote, "ls-tree", "-r", "--name-only", "main"]);
        assert!(!tree.contains(".config/app/"), "{tree}");
    }
    assert_eq!(env.repo_config().dirs.paths, Vec::<String>::new());
    assert_eq!(env.read(".config/app/f00.conf"), b"x\n");
    assert_eq!(env.porcelain("status"), Vec::new());
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
/// committed. A user's gitconfig that turns rename detection off changes nothing.
#[test]
fn a_held_rename_never_commits_the_deletion_of_its_old_path() {
    for gitconfig in [
        "",
        "[status]\n\trenames = false\n",
        "[diff]\n\trenames = false\n",
    ] {
        let a = TestEnv::new();
        let global = a.home().join(".gitconfig");
        let mut text = fs::read_to_string(&global).expect("read .gitconfig");
        text.push_str(gitconfig);
        fs::write(&global, text).expect("write .gitconfig");
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
                "{gitconfig:?}: {err}"
            );
            let names = tree();
            assert!(
                names.lines().any(|name| name == ".a"),
                "{gitconfig:?}: {names}"
            );
            assert!(!names.lines().any(|name| name == ".b"), "{names}");
        }

        a.write_repo(".b", b"a, renamed\n");
        exits(&a, 0, &["sync"]);
        let names = tree();
        assert!(names.lines().any(|name| name == ".b"), "{names}");
        assert!(!names.lines().any(|name| name == ".a"), "{names}");
    }
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

/// Untracked files in the repo worktree that `[ignore]` matches, and a temp directory a run is
/// still writing, are never committed, so no machine receives them; sync neither names nor holds
/// them, even one with a credential in it. A file that `[ignore]` matches but was added by name
/// still syncs, as git keeps staging a tracked file its ignore rules match.
#[test]
fn ignored_junk_in_the_repo_worktree_never_reaches_a_commit() {
    let (a, b) = pair(&[
        (".bashrc", b"b\n", 0o644),
        (".config/app/a.conf", b"a\n", 0o644),
    ]);
    let mut child = std::process::Command::new("true").spawn().expect("spawn");
    let dead = child.id();
    child.wait().expect("wait");
    let token = format!("gh{}_{}", "p", "Zq7".repeat(12));
    let junk = [
        ".bashrc.orig".to_owned(),
        ".config/app/.a.conf.swp".to_owned(),
        ".config/app/node_modules/x/i.js".to_owned(),
        format!(".config/app/.dotman-tmp-{dead}-DiRrEp/inner.conf"),
    ];
    for key in &junk {
        a.write_repo(key, b"junk\n");
    }
    a.write_repo(
        ".config/app/.b.conf.swp",
        format!("tok={token}\n").as_bytes(),
    );
    a.write(".config/vim/.vimrc.swp", b"swap\n", 0o644);
    a.ok(&["add", ".config/vim/.vimrc.swp"]);

    let err = exits(&a, 0, &["sync", "--dry-run"]);
    assert!(
        !err.contains(".orig") && !err.contains(".a.conf.swp"),
        "{err}"
    );
    let err = exits(&a, 0, &["sync"]);
    assert!(
        !err.contains("node_modules") && !err.contains(".b.conf.swp"),
        "{err}"
    );
    let tree = a.git(&["-C", &repo_path(&a), "ls-tree", "-r", "--name-only", "HEAD"]);
    let committed: Vec<&str> = tree.lines().collect();
    assert_eq!(
        committed,
        [
            ".bashrc",
            ".config/app/a.conf",
            ".config/vim/.vimrc.swp",
            ".dotman.toml",
            "install.sh"
        ]
    );
    for key in &junk {
        assert!(!a.home().join(key).exists(), "{key}");
    }

    exits(&b, 0, &["sync"]);
    assert_eq!(b.read(".config/vim/.vimrc.swp"), b"swap\n");
    for key in junk.iter().chain([&".config/app/.b.conf.swp".to_owned()]) {
        assert!(!b.home().join(key).exists(), "{key}");
    }

    a.write(".config/vim/.vimrc.swp", b"swap edited\n", 0o644);
    exits(&a, 0, &["sync"]);
    exits(&b, 0, &["sync"]);
    assert_eq!(b.read(".config/vim/.vimrc.swp"), b"swap edited\n");
}

/// A repo worktree name that is not UTF-8 is never a key. `[ignore]` matches it, or a directory
/// above it, in its lossy form; one that no pattern matches is never committed either, and status
/// names it until one does. A reserved root name, such as a README, is repo content and is
/// committed.
#[test]
fn a_repo_name_that_is_not_utf8_is_never_committed_and_status_names_it() {
    let env = first_device(&[(".a", b"a\n", 0o644)]);
    let repo = env.repo();
    let ignored = repo.join(OsStr::from_bytes(b"\xff\xfe.log"));
    let in_ignored_dir = repo.join(OsStr::from_bytes(b"\xffcache/blob"));
    let loose = repo.join(OsStr::from_bytes(b"notes\xff.txt"));
    let readme = repo.join(OsStr::from_bytes(b"README\xff.md"));
    for path in [&ignored, &in_ignored_dir, &loose, &readme] {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).expect("create the dir");
        }
        fs::write(path, b"junk\n").expect("write the file");
    }
    let mut config = env.repo_config();
    config
        .ignore
        .patterns
        .extend(["*.log", "*cache"].map(str::to_owned));
    config
        .save(&repo.join(".dotman.toml"), false)
        .expect("save .dotman.toml");
    env.write(".a", b"a2\n", 0o644);

    let err = exits(&env, 0, &["status"]);
    let named: Vec<&str> = err
        .lines()
        .filter(|line| line.contains("never committed"))
        .collect();
    assert_eq!(named.len(), 1, "{err}");
    assert!(
        named
            .iter()
            .all(|line| line.starts_with("notes\u{fffd}.txt: in the repo")),
        "{err}"
    );
    let err = exits(&env, 0, &["sync"]);
    assert!(err.contains("pushed 1 commit to origin/main"), "{err}");
    assert_eq!(remote_file(&env, ".a"), "a2\n");
    let remote = env.remote_path().to_string_lossy().into_owned();
    let tree = env.git(&["--git-dir", &remote, "ls-tree", "-r", "--name-only", "main"]);
    assert!(
        !tree.contains(".log") && !tree.contains("cache") && !tree.contains("notes"),
        "{tree}"
    );
    assert!(tree.contains("README"), "{tree}");
    assert!(loose.exists() && ignored.exists() && in_ignored_dir.exists());

    config.ignore.patterns.push("notes*".to_owned());
    config
        .save(&repo.join(".dotman.toml"), false)
        .expect("save .dotman.toml");
    let err = exits(&env, 0, &["status"]);
    assert!(!err.contains("never committed"), "{err}");
}

/// Many repo names that are not UTF-8 give one warning with their count, the first three names
/// and one remedy, not a line each.
#[test]
fn many_repo_names_that_are_not_utf8_are_counted_in_one_warning() {
    let env = first_device(&[(".a", b"a\n", 0o644)]);
    for n in 0..7_u8 {
        let name = [b'n', b'0'.saturating_add(n), 0xff];
        fs::write(env.repo().join(OsStr::from_bytes(&name)), b"junk\n").expect("write the file");
    }
    let err = exits(&env, 0, &["status"]);
    let lines: Vec<&str> = err.lines().collect();
    let at = lines
        .iter()
        .position(|line| line.starts_with("7 files in the repo are never committed"))
        .expect("one warning counts them");
    assert!(
        lines
            .get(at)
            .is_some_and(|line| line.contains("[ignore] pattern")),
        "{err}"
    );
    assert_eq!(
        lines.get(at.saturating_add(1)..at.saturating_add(5)),
        Some(
            &[
                "  n0\u{fffd}",
                "  n1\u{fffd}",
                "  n2\u{fffd}",
                "  and 4 more"
            ][..]
        ),
        "{err}"
    );
    assert_eq!(err.matches("not valid UTF-8").count(), 1, "{err}");
    assert!(!err.contains("n3"), "{err}");
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

/// The message names `forget` and `add` for the path, and run as printed, on a path with a space,
/// they hand the file over to the repo.
#[test]
fn a_managed_directory_replaced_by_a_file_gets_a_clear_message() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/my nvim/init.lua", b"init\n", 0o644);
    env.write(".config/my nvim/lua/opts.lua", b"opts\n", 0o644);
    env.ok(&["add", ".config/my nvim"]);
    env.ok(&["sync"]);
    fs::remove_dir_all(env.home().join(".config/my nvim/lua")).expect("remove the dir");
    env.write(".config/my nvim/lua", b"not a dir\n", 0o644);

    let err = exits(&env, 3, &["sync"]);
    assert!(
        err.contains(
            ".config/my nvim/lua: a file here, but a directory of managed files in the repo"
        ),
        "{err}"
    );
    let path = env.home().join(".config/my nvim/lua");
    let path = shell_words::quote(&path.to_string_lossy()).into_owned();
    assert!(
        err.contains(&format!("`dotman forget {path}`, then `dotman add {path}`")),
        "{err}"
    );
    assert!(!err.contains("os error"), "{err}");
    assert_eq!(env.read(".config/my nvim/lua"), b"not a dir\n");
    assert_eq!(
        fs::read(env.repo().join(".config/my nvim/lua/opts.lua")).expect("repo file"),
        b"opts\n"
    );

    for step in ["forget", "add"] {
        let out = common::run_hint(&env, &err, step);
        assert!(out.status.success(), "{step}: {}", stderr(&out));
    }
    assert_eq!(env.read(".config/my nvim/lua"), b"not a dir\n");
    assert_eq!(
        fs::read(env.repo().join(".config/my nvim/lua")).expect("repo file"),
        b"not a dir\n"
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
            assert!(err.contains("dotman git checkout -- .zshrc"), "{err}");
            assert!(
                !err.contains("--allow-plain"),
                "never for an age key: {err}"
            );
            // A hand edit is only in the repo, so `add`, which copies $HOME in, is no remedy.
            let route = if by_edit {
                [
                    "has the same edit: `dotman add --encrypt",
                    "remove the secret there",
                ]
            } else {
                [
                    "the edit is only in the repo copy, not in $HOME",
                    "dotman git diff -- .zshrc",
                ]
            };
            for remedy in route {
                assert!(err.contains(remedy), "{remedy}: {err}");
            }
            assert_eq!(err.contains("add --encrypt"), by_edit, "{err}");
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

/// A GitHub token shape, built here so no token-shaped literal sits in the source.
fn github_token() -> String {
    format!("ghp_{}", "Zq7".repeat(12))
}

/// The reviewer's case: a hand edit in the repo holds work found nowhere else plus a token. The
/// remedy sync prints keeps the work, and `add`, plain or `--encrypt`, refuses to replace the only
/// copy instead of destroying it.
#[test]
fn add_never_destroys_a_held_hand_edit_in_the_repo() {
    let env = first_device(&[(".bashrc", b"alias ll=ls\n", 0o644)]);
    let edit = format!(
        "alias ll=ls\n# PRECIOUS hand-written function\nexport T={}\n",
        github_token()
    );
    env.write_repo(".bashrc", edit.as_bytes());
    let err = exits(&env, 3, &["sync"]);
    assert!(
        err.contains("\n  .bashrc: line 3 holds a github-token\n"),
        "{err}"
    );
    for remedy in [
        "the edit is only in the repo copy, not in $HOME",
        "dotman git diff -- .bashrc",
        "dotman git checkout -- .bashrc",
    ] {
        assert!(err.contains(remedy), "{remedy}: {err}");
    }
    assert!(!err.contains("dotman add"), "{err}");
    let status = env.ok(&["status"]);
    assert!(status.contains("held in the repo"), "{status}");

    let repo_copy = env.repo().join(".bashrc").to_string_lossy().into_owned();
    for args in [
        &["add", "--allow-plain", ".bashrc"][..],
        &["add", ".bashrc"],
        &["add", "--dry-run", ".bashrc"],
    ] {
        let err = exits(&env, 1, args);
        assert!(
            err.contains(".bashrc in the repo has an edit that is neither committed nor in $HOME")
                && err.contains("dotman git diff -- .bashrc")
                && err.contains(&repo_copy),
            "{args:?}: {err}"
        );
        assert_eq!(
            fs::read(env.repo().join(".bashrc")).expect("repo copy"),
            edit.as_bytes(),
            "{args:?}"
        );
    }
    if common::age_installed() {
        env.ok(&["key", "init"]);
        let err = exits(&env, 1, &["add", "--encrypt", ".bashrc"]);
        assert!(err.contains("add would remove it"), "{err}");
        assert_eq!(
            fs::read(env.repo().join(".bashrc")).expect("repo copy"),
            edit.as_bytes()
        );
        assert!(!env.repo().join(".bashrc{age}").exists());
    }
}

/// An `[ignore]` pattern leaves a managed file managed, so a held edit to one is offered `forget`.
/// Run as printed, it ends the hold for a plain key and for one under `[dirs]`, which the next
/// sync does not capture again; the file stays in `$HOME` and the token never reaches the remote.
#[test]
fn a_held_edit_to_a_managed_file_is_offered_forget_which_ends_the_hold() {
    let env = first_device(&[(".apprc", b"a=1\n", 0o644)]);
    env.write(".config/app/one.conf", b"x=1\n", 0o644);
    env.write(".config/app/two.conf", b"y=1\n", 0o644);
    exits(&env, 0, &["add", ".config/app"]);
    exits(&env, 0, &["sync"]);
    let token = github_token();
    for key in [".apprc", ".config/app/one.conf"] {
        env.write(key, format!("a=1\ntoken = {token}\n").as_bytes(), 0o644);
    }

    let err = exits(&env, 3, &["sync"]);
    for key in [".apprc", ".config/app/one.conf"] {
        let line = err
            .lines()
            .find(|line| line.starts_with(&format!("{key}: held, ")))
            .unwrap_or_default();
        assert!(!line.contains("[ignore]"), "{err}");
        let out = common::run_hint(&env, line, "forget");
        assert_eq!(out.status.code(), Some(0), "{line}");
    }
    exits(&env, 0, &["sync"]);
    let err = exits(&env, 0, &["sync"]);
    assert!(!err.contains("capture"), "{err}");
    let status = exits(&env, 0, &["status"]);
    assert!(!status.contains("one.conf"), "{status}");
    for key in [".apprc", ".config/app/one.conf"] {
        assert!(String::from_utf8_lossy(&env.read(key)).contains(&token));
    }
    let remote = env.remote_path().to_string_lossy().into_owned();
    let tree = env.git(&["--git-dir", &remote, "ls-tree", "-r", "--name-only", "main"]);
    assert!(
        tree.contains(".config/app/two.conf") && !tree.contains("one.conf"),
        "{tree}"
    );
    let history = env.git(&["--git-dir", &remote, "log", "--all", "-p"]);
    assert!(!history.contains(&token), "{history}");
}

/// `forget` takes a key, never one variant, so for a held edit to a key with variants the hint says
/// it removes every repo file of the key. Run as printed, it removes those it names; the other
/// machine keeps its file, and the token never reaches the remote.
#[test]
fn the_forget_hint_for_a_key_with_variants_names_every_repo_file_it_removes() {
    let a = first_device(&[(".prof", b"shared=1\n", 0o644)]);
    a.write_machine_config("[identity]\nhost = \"alpha\"\n");
    a.write(".prof", b"alpha=1\n", 0o644);
    exits(&a, 0, &["add", "--for", "host=alpha", ".prof"]);
    exits(&a, 0, &["sync"]);
    let b = a.second_device();
    b.write_machine_config("[identity]\nhost = \"beta\"\n");
    b.ok(&["init", &b.remote_url()]);
    b.write(".prof", b"beta=1\n", 0o644);
    b.ok(&["add", "--for", "host=beta", ".prof"]);
    b.ok(&["sync"]);
    exits(&a, 0, &["sync"]);
    let token = github_token();
    a.write(
        ".prof",
        format!("alpha=1\ntoken = {token}\n").as_bytes(),
        0o644,
    );

    let err = exits(&a, 3, &["sync"]);
    let line = err
        .lines()
        .find(|line| line.starts_with(".prof: held, "))
        .unwrap_or_default();
    let sources = [".prof", ".prof{host=alpha}", ".prof{host=beta}"];
    assert!(
        line.contains(&format!(
            "stops managing it on every machine and removes all 3 of its repo files ({})",
            sources.join(", ")
        )),
        "{err}"
    );
    let out = common::run_hint(&a, line, "forget");
    assert_eq!(out.status.code(), Some(0), "{line}");
    exits(&a, 0, &["sync"]);
    let remote = a.remote_path().to_string_lossy().into_owned();
    let tree = a.git(&["--git-dir", &remote, "ls-tree", "-r", "--name-only", "main"]);
    for source in sources {
        assert!(!tree.lines().any(|path| path == source), "{tree}");
    }
    let history = a.git(&["--git-dir", &remote, "log", "--all", "-p"]);
    assert!(!history.contains(&token), "{history}");

    b.ok(&["sync"]);
    assert_eq!(b.read(".prof"), b"beta=1\n");
    assert_eq!(b.porcelain("list"), []);
}

/// What `add` replaces is the work of `$HOME`'s file: the copy an earlier `add` or `dotman edit`
/// left in the repo, uncommitted, is replaced as before.
#[test]
fn add_still_replaces_a_repo_copy_that_came_from_home() {
    let env = first_device(&[(".a", b"one\n", 0o644)]);
    env.write(".a", b"two\n", 0o644);
    exits(&env, 0, &["add", ".a"]);
    env.write(".a", b"three\n", 0o644);
    exits(&env, 0, &["add", ".a"]);
    assert_eq!(
        fs::read(env.repo().join(".a")).expect("repo copy"),
        b"three\n"
    );

    let editor = env.write_script("ed.sh", "#!/bin/sh\necho edited >> \"$1\"\n");
    let editor = editor.to_string_lossy().into_owned();
    env.ok_with(&[("EDITOR", &editor), ("VISUAL", "")], &["edit", ".a"]);
    env.write(".a", b"four\n", 0o644);
    exits(&env, 0, &["add", ".a"]);
    assert_eq!(
        fs::read(env.repo().join(".a")).expect("repo copy"),
        b"four\n"
    );
    exits(&env, 0, &["sync"]);
}

/// `dotman edit` leaves the key clean while its uncommitted repo copy holds a credential, which
/// stops sync before the pull: status says so, in both forms, instead of saying clean. The remedy
/// offers `--encrypt`, never `--allow-plain`; typed by hand, `--allow-plain` still lets it through.
#[test]
fn status_reports_a_held_repo_edit_that_stops_sync() {
    let env = first_device(&[(".a", b"a\n", 0o644)]);
    let editor = env.write_script(
        "ed.sh",
        &format!("#!/bin/sh\necho 'GH={}' >> \"$1\"\n", github_token()),
    );
    let editor = editor.to_string_lossy().into_owned();
    env.ok_with(&[("EDITOR", &editor), ("VISUAL", "")], &["edit", ".a"]);
    let home = env.home().join(".a").to_string_lossy().into_owned();

    let out = env.run(&["status"]);
    let (stdout, err) = (String::from_utf8_lossy(&out.stdout), stderr(&out));
    assert!(stdout.contains("held in the repo"), "{stdout}");
    assert!(
        stdout.contains("\n  .a: line 2 holds a github-token\n"),
        "{stdout}"
    );
    assert!(err.contains("1 held in the repo"), "{err}");
    let porcelain = env.porcelain("status");
    assert!(
        porcelain.contains(&("held-in-repo".to_owned(), ".a".to_owned())),
        "{porcelain:?}"
    );

    let err = exits(&env, 3, &["sync"]);
    assert!(err.contains("stopped before the pull"), "{err}");
    let encrypt =
        format!("`dotman add --encrypt {home}` stores it encrypted; or remove the secret");
    assert!(err.contains(&encrypt), "{err}");
    assert!(!err.contains("--allow-plain"), "{err}");
    exits(&env, 0, &["add", "--allow-plain", ".a"]);
    exits(&env, 0, &["sync"]);
    assert!(!env.ok(&["status"]).contains("held in the repo"));
}

/// A dotman temp file in the repo worktree is never staged, so it never holds sync back either.
#[test]
fn a_temp_file_in_the_repo_holds_nothing_back() {
    let env = first_device(&[(".a", b"a\n", 0o644)]);
    env.write_repo(
        ".dotman-tmp-1-x",
        format!("GH={}\n", github_token()).as_bytes(),
    );
    let err = exits(&env, 0, &["sync", "--dry-run"]);
    assert!(!err.contains("stopped before the pull"), "{err}");
    assert!(!env.ok(&["status"]).contains("held in the repo"));
}

#[test]
fn an_edit_that_does_not_go_back_after_the_pull_stops_before_home_is_written() {
    let (a, mut b) = pair(&[(".zshrc", b"line1\nline2\n", 0o644)]);
    a.write(".zshrc", b"line1 from a\nline2\n", 0o644);
    exits(&a, 0, &["sync"]);
    // An edit that lands in the repo between sync's commit and its pull, as an editor saving then.
    let armed = b.root().join("late-edit");
    fs::write(&armed, b"").expect("arm the edit");
    let edit = format!(
        "for arg in \"$@\"; do [ \"$arg\" = pull ] && [ -e '{armed}' ] && rm '{armed}' && printf 'line1 late edit\\nline2\\n' > '{}'; done",
        b.repo().join(".zshrc").display(),
        armed = armed.display()
    );
    wrap_git(&mut b, &edit);

    let published = b.remote_refs();
    let err = exits(&b, 4, &["sync"]);
    assert!(
        err.contains("did not go back on top of it cleanly"),
        "{err}"
    );
    let [put_back, show, drop, sync]: [String; 4] = steps(&err).try_into().expect("four steps");
    assert_eq!(
        put_back,
        "dotman git checkout HEAD -- .zshrc\nputs back the pulled version"
    );
    assert!(
        show.starts_with("dotman git stash show -p\nshows the edits"),
        "{show}"
    );
    assert_eq!(b.read(".zshrc"), b"line1\nline2\n", "$HOME untouched");
    assert_eq!(b.remote_refs(), published);

    for step in [&put_back, &show, &drop, &sync] {
        let command = step.lines().next().unwrap_or_default();
        let out = paste(&b, command);
        assert!(out.status.success(), "{command}: {}", stderr(&out));
    }
    assert_eq!(b.read(".zshrc"), b"line1 from a\nline2\n");
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

/// Backups are never pruned: while syncs and applies replace one file again and again, every
/// backup run directory and every copy in it stays as it was written, and each replaced version
/// is there at the end.
#[test]
fn backups_are_never_pruned_by_later_syncs_and_applies() {
    let (a, b) = pair(&[(".bashrc", b"v0\n", 0o644)]);
    let backups = b.state_dir().join("backups");
    let mut seen = std::collections::BTreeMap::new();
    for round in 0..3_u32 {
        a.write(".bashrc", format!("s{round}\n").as_bytes(), 0o644);
        exits(&a, 0, &["sync"]);
        exits(&b, 0, &["sync"]);
        a.write(".bashrc", format!("p{round}\n").as_bytes(), 0o644);
        exits(&a, 0, &["sync"]);
        b.ok(&["git", "pull", "-q", "--rebase", "origin", "main"]);
        exits(&b, 0, &["apply"]);
        assert_eq!(b.read(".bashrc"), format!("p{round}\n").into_bytes());

        let now = tree_checksum(&backups);
        for (path, entry) in &seen {
            assert_eq!(now.get(path), Some(entry), "{path} in round {round}");
        }
        assert!(now.len() > seen.len(), "round {round} backed nothing up");
        seen = now;
    }
    let runs = seen.keys().filter(|path| !path.contains('/')).count();
    assert_eq!(runs, 6, "{seen:?}");
    let mut copies: Vec<Vec<u8>> = b
        .backups(".bashrc")
        .into_iter()
        .map(|(bytes, _)| bytes)
        .collect();
    copies.sort();
    assert_eq!(
        copies,
        ["p0\n", "p1\n", "s0\n", "s1\n", "s2\n", "v0\n"].map(|text| text.as_bytes().to_vec())
    );
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
    let recipe: Vec<&str> = err
        .lines()
        .skip_while(|line| !line.starts_with("  mv "))
        .take_while(|line| line.starts_with("  "))
        .map(str::trim_start)
        .collect();
    let repo = repo_path(&b);
    assert_eq!(
        recipe,
        [
            format!("mv {repo} {repo}.old"),
            format!("dotman import {}", b.remote_url()),
            format!("dotman import --overlay {repo}.old --include-missing"),
        ],
        "{err}"
    );

    let [mv, import, overlay]: [&str; 3] = recipe.try_into().expect("three steps");
    for step in [mv, import] {
        let out = paste(&b, step);
        assert!(out.status.success(), "{step}: {}", stderr(&out));
    }
    let out = paste(&b, overlay);
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

/// An editor holds `.my rc` in a buffer while a sync pulls a new version of it, then writes the
/// buffer back over it. `edit` saves the edit aside, puts the pulled version back and says so, and
/// the next sync reverts nothing.
#[test]
fn an_edit_made_while_a_sync_pulled_the_same_file_reverts_nothing() {
    let (a, b) = pair(&[(".my rc", b"line1\nline2\n", 0o644)]);
    a.write(".my rc", b"line1\nline2\nA fix\n", 0o644);
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
        .args(["edit", ".my rc"])
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
        err.contains(".my rc changed while the editor was open"),
        "{err}"
    );
    assert!(err.contains("moved HEAD from"), "{err}");
    assert!(!err.contains("dotman sync"), "{err}");
    let printed = backticked(&err);
    let diff = printed.iter().find(|c| c.starts_with("diff -u"));
    assert!(diff.is_some(), "no diff in: {err}");
    let diff = diff.map_or("", String::as_str);
    let out = paste(&b, diff);
    assert_eq!(out.status.code(), Some(1), "{diff}: {}", stderr(&out));
    assert!(stderr(&out).is_empty(), "{diff}: {}", stderr(&out));
    let home = shell_words::quote(&b.home().join(".my rc").to_string_lossy()).into_owned();
    let again = format!("dotman edit {home}");
    assert!(printed.contains(&again), "{err}");
    let out = paste_with(&b, &[("VISUAL", "true")], &again);
    assert!(out.status.success(), "{again}: {}", stderr(&out));
    assert_eq!(
        b.backups(".dotman/edit/.my rc"),
        [(b"line1\nline2\nB added\n".to_vec(), 0o600)]
    );
    assert_eq!(
        fs::read(b.repo().join(".my rc")).expect("repo copy"),
        b"line1\nline2\nA fix\n"
    );
    exits(&b, 0, &["sync"]);
    assert_eq!(remote_file(&a, ".my rc"), "line1\nline2\nA fix\n");
}

/// Whether the line after `headline` in `err`, where git's own message goes, starts with `start`.
fn below(err: &str, headline: &str, start: &str) -> bool {
    err.lines()
        .skip_while(|line| *line != headline)
        .nth(1)
        .is_some_and(|line| line.starts_with(start))
}

/// A remote that cannot be reached is named on one line that says what this run committed, with
/// git's message below it, and the next sync sends the commit.
#[test]
fn an_unreachable_remote_says_what_the_run_committed() {
    let (a, b) = pair(&[(".a", b"a\n", 0o644)]);
    let away = a.root().join("remote-away.git");
    fs::rename(a.remote_path(), &away).expect("take the remote away");

    let err = exits(&b, 1, &["sync"]);
    let headline = "could not reach origin, so nothing was pushed; the next sync tries again";
    assert!(below(&err, headline, "git ls-remote"), "{err}");

    b.write(".a", b"edited offline\n", 0o644);
    let err = exits(&b, 1, &["sync"]);
    let headline =
        "could not reach origin; your changes are committed locally and go out on the next sync";
    assert!(below(&err, headline, "git ls-remote"), "{err}");
    assert!(
        err.contains("does not appear to be a git repository"),
        "{err}"
    );

    fs::rename(&away, a.remote_path()).expect("bring the remote back");
    let err = exits(&b, 0, &["sync"]);
    assert!(err.contains("pushed 1 commit to origin/main"), "{err}");
    assert_eq!(remote_file(&a, ".a"), "edited offline\n");
}

/// Another machine pushes between this sync's pull and its push. The refusal says the remote moved
/// and to sync again, and that sync, run as printed, sends both machines' edits.
#[test]
fn a_push_refused_because_the_remote_moved_says_to_sync_again() {
    let (a, mut b) = pair(&[(".a", b"a\n", 0o644), (".b", b"b\n", 0o644)]);
    let armed = b.root().join("race");
    fs::write(&armed, b"").expect("arm the race");
    wrap_git(
        &mut b,
        &format!(
            "case \" $* \" in *\" push \"*) if [ -e '{armed}' ]; then rm '{armed}'; cd '{repo}' && printf 'from a\\n' > .a && git commit -qam race && git push -q origin HEAD:main; cd - >/dev/null; fi;; esac",
            armed = armed.display(),
            repo = a.repo().display()
        ),
    );
    b.write(".b", b"from b\n", 0o644);
    let err = exits(&b, 1, &["sync"]);
    let headline = "origin moved while this sync ran, so the push was refused; your commits are kept here, and `dotman sync` again pulls what arrived and pushes them";
    assert!(below(&err, headline, "git push"), "{err}");
    assert!(err.contains("[rejected]"), "{err}");

    let steps = backticked(headline);
    assert_eq!(steps, ["dotman sync"]);
    for step in steps {
        let out = paste(&b, &step);
        assert!(out.status.success(), "{step}: {}", stderr(&out));
    }
    assert_eq!(remote_file(&a, ".a"), "from a\n");
    assert_eq!(remote_file(&a, ".b"), "from b\n");
    assert_eq!(b.read(".a"), b"from a\n");
}

/// A lock file that a killed git left in the repo stops sync with a removal step, whether sync
/// finds it at the start or git meets it mid-run, and the step run as printed lets sync through.
#[test]
fn a_stale_git_lock_gets_a_removal_step_that_works() {
    let (a, mut b) = pair(&[(".a", b"a\n", 0o644)]);
    let lock = b.repo().join(".git/index.lock");
    fs::write(&lock, b"").expect("leave a lock");
    b.write(".a", b"edited\n", 0o644);
    let err = exits(&b, 1, &["sync"]);
    let run = |env: &TestEnv, err: &str| {
        let rm = backticked(err)
            .into_iter()
            .find(|step| step.starts_with("rm "))
            .expect("an rm step");
        let out = paste(env, &rm);
        assert!(out.status.success(), "{rm}: {}", stderr(&out));
    };
    run(&b, &err);
    assert!(!lock.exists());

    let armed = b.root().join("lock-on-add");
    fs::write(&armed, b"").expect("arm the lock");
    wrap_git(
        &mut b,
        &format!(
            "case \" $* \" in *\" add \"*) if [ -e '{armed}' ]; then rm '{armed}'; : > '{lock}'; fi;; esac",
            armed = armed.display(),
            lock = lock.display()
        ),
    );
    let err = exits(&b, 1, &["sync"]);
    let repo = repo_path(&b);
    let headline = format!(
        "git's lock file {} is in the repo. Another git is running in {repo}, or one was killed and left it behind; if no git is running, remove it with `rm {}` and run this again",
        lock.display(),
        lock.display()
    );
    assert!(below(&err, &headline, "git add"), "{err}");
    assert!(err.contains("Unable to create"), "{err}");
    run(&b, &err);
    let err = exits(&b, 0, &["sync"]);
    assert!(err.contains("pushed 1 commit to origin/main"), "{err}");
    assert_eq!(remote_file(&a, ".a"), "edited\n");
}

/// Any lock file at the top of `.git`, as a killed rebase leaves `MERGE_MSG.lock` or
/// `AUTO_MERGE.lock`, stops sync with an `rm` step that names it, whether sync finds it at the
/// start or the pull's rebase stops on it, which no longer reads as a conflict with no paths. The
/// step run as printed lets sync through.
#[test]
fn a_lock_file_a_killed_rebase_left_gets_a_removal_step_that_works() {
    let (a, mut b) = pair(&[(".a", b"a\n", 0o644), (".b", b"b\n", 0o644)]);
    a.write(".a", b"from a\n", 0o644);
    exits(&a, 0, &["sync"]);
    b.write(".b", b"from b\n", 0o644);
    let git_dir = b.repo().join(".git");
    let remove = |env: &TestEnv, err: &str, names: &[&str]| {
        let rm = backticked(err)
            .into_iter()
            .find(|step| step.starts_with("rm "))
            .expect("an rm step");
        for name in names {
            assert!(rm.contains(&*git_dir.join(name).to_string_lossy()), "{err}");
        }
        let out = paste(env, &rm);
        assert!(out.status.success(), "{rm}: {}", stderr(&out));
    };
    let names = ["AUTO_MERGE.lock", "MERGE_MSG.lock", "ORIG_HEAD.lock"];
    for name in names {
        fs::write(git_dir.join(name), b"").expect("leave a lock");
    }
    let err = exits(&b, 1, &["sync"]);
    remove(&b, &err, &names);
    assert!(names.iter().all(|name| !git_dir.join(name).exists()));

    // MERGE_MSG.lock stops the merge backend of rebase, REBASE_HEAD.lock the apply backend of
    // git before 2.26.
    let mid_run = ["MERGE_MSG.lock", "REBASE_HEAD.lock"];
    let armed = b.root().join("lock-on-pull");
    fs::write(&armed, b"").expect("arm the locks");
    let leave = mid_run
        .map(|name| format!(": > '{}'", git_dir.join(name).display()))
        .join("; ");
    wrap_git(
        &mut b,
        &format!(
            "case \" $* \" in *\" pull \"*) if [ -e '{armed}' ]; then rm '{armed}'; {leave}; fi;; esac",
            armed = armed.display()
        ),
    );
    let err = exits(&b, 1, &["sync"]);
    assert!(!err.contains("the pull conflicts"), "{err}");
    remove(&b, &err, &mid_run);
    let err = exits(&b, 0, &["sync"]);
    assert!(err.contains("pushed 1 commit to origin/main"), "{err}");
    assert_eq!(remote_file(&a, ".b"), "from b\n");
}

/// The list of paths `git add` stages from goes to a file in `.git`, so a TMPDIR that is missing
/// or read-only never fails a sync after it captured an edit.
#[test]
fn sync_needs_no_usable_tmpdir() {
    let env = first_device(&[(".a", b"one\n", 0o644)]);
    let missing = env.root().join("no-such-tmp");
    env.write(".a", b"two\n", 0o644);
    let err = exits_with(
        &env,
        &[("TMPDIR", &missing.to_string_lossy())],
        0,
        &["sync"],
    );
    assert!(err.contains("pushed 1 commit to origin/main"), "{err}");
    assert_eq!(remote_file(&env, ".a"), "two\n");

    let read_only = env.root().join("read-only-tmp");
    fs::create_dir_all(&read_only).expect("create the temp dir");
    fs::set_permissions(&read_only, fs::Permissions::from_mode(0o555)).expect("chmod");
    env.write(".a", b"three\n", 0o644);
    let err = exits_with(
        &env,
        &[("TMPDIR", &read_only.to_string_lossy())],
        0,
        &["sync"],
    );
    assert!(err.contains("pushed 1 commit to origin/main"), "{err}");
    assert_eq!(remote_file(&env, ".a"), "three\n");
}
