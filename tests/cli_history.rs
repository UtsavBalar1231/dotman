mod common;

use common::TestEnv;
use std::os::unix::fs::PermissionsExt;

/// A repo with `.a` committed as "one\n" on 2020-01-01 and "two\n" by `sync` today.
fn two_commits() -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", b"one\n", 0o644);
    env.ok(&["add", ".a"]);
    commit_at(&env, "2020-01-01T12:00:00Z", "first");
    env.write(".a", b"two\n", 0o644);
    env.ok(&["sync"]);
    env
}

// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
/// Commits the repo worktree with the given author and committer date.
fn commit_at(env: &TestEnv, date: &str, message: &str) {
    let repo = env.repo().to_string_lossy().into_owned();
    let dated = |args: &[&str]| {
        let mut cmd = std::process::Command::new("git");
        cmd.args(["-C", &repo]).args(args);
        cmd.env("HOME", env.home())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date);
        let out = cmd.output().expect("run git");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    dated(&["add", "-A", "-f"]);
    dated(&["commit", "-q", "-m", message]);
}

fn token(env: &TestEnv, key: &str) -> Option<String> {
    env.token_of("status", key)
}

#[test]
fn restore_from_an_earlier_commit_backs_up_and_leaves_the_repo_alone() {
    let env = two_commits();
    env.write(".a", b"three\n", 0o644);
    env.ok(&["sync"]);
    let head = env.git(&["-C", &env.repo().to_string_lossy(), "rev-parse", "HEAD"]);
    assert_eq!(token(&env, ".a").as_deref(), Some("clean"));
    env.ok(&["restore", ".a", "--from", "HEAD~1"]);
    assert_eq!(env.read(".a"), b"two\n");
    assert!(
        env.backups(".a")
            .iter()
            .any(|(bytes, _)| bytes == b"three\n")
    );
    assert_eq!(token(&env, ".a").as_deref(), Some("changed-locally"));
    env.ok(&["sync"]);
    let repo = env.repo().to_string_lossy().into_owned();
    assert_eq!(env.git(&["-C", &repo, "show", "HEAD:.a"]), "two\n");
    assert_ne!(env.git(&["-C", &repo, "rev-parse", "HEAD"]), head);
}

#[test]
fn restore_from_a_date_between_commits_picks_the_earlier_one() {
    let env = two_commits();
    env.ok(&["restore", ".a", "--from", "2020-02-01"]);
    assert_eq!(env.read(".a"), b"one\n");
}

#[test]
fn restore_refuses_a_mistyped_revision_instead_of_reading_it_as_a_date() {
    let env = two_commits();
    let before = env.four_trees();
    for typo in ["heaad~2", "mian", "yesterdy"] {
        let err = env.fails(&["restore", ".a", "--from", typo]);
        assert!(
            err.contains(&format!("`{typo}` is neither a commit nor a date")),
            "{err}"
        );
    }
    assert_eq!(env.four_trees(), before);
    env.ok(&["restore", ".a", "--from", "2020-02-01 08:00"]);
    assert_eq!(env.read(".a"), b"one\n");
    let err = env.fails(&["restore", ".a", "--from", "2019-12-31"]);
    assert!(err.contains("every commit is newer than"), "{err}");
}

#[test]
fn a_calendar_date_is_read_in_the_local_time_zone() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", b"one\n", 0o644);
    env.ok(&["add", ".a"]);
    commit_at(&env, "2020-06-15T12:00:00Z", "one");
    env.write_repo(".a", b"two\n");
    commit_at(&env, "2020-06-15T13:00:00Z", "two");
    // POSIX offsets need no zone database: XXX-5:30 is five and a half hours east of UTC.
    for (zone, expected) in [("XXX-5:30", b"one\n"), ("UTC0", b"two\n")] {
        env.write(".a", b"local\n", 0o644);
        let out = env.run_with(
            &[("TZ", zone)],
            &["restore", ".a", "--from", "2020-06-15 18:00"],
        );
        assert!(
            out.status.success(),
            "{zone}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(env.read(".a"), expected, "{zone}");
    }
}

#[test]
fn restore_reads_dates_itself_and_names_a_revision_past_the_history() {
    let env = two_commits();
    env.write(".a", b"three\n", 0o644);
    for future in ["2100-01-01", "3000-01-01"] {
        env.ok(&["restore", ".a", "--from", future]);
        assert_eq!(env.read(".a"), b"two\n", "{future} is after every commit");
        env.write(".a", b"three\n", 0o644);
    }
    let err = env.fails(&["restore", ".a", "--from", "2025-09-31"]);
    assert!(err.contains("neither a commit nor a date"), "{err}");
    let err = env.fails(&["restore", ".a", "--from", "99999.years.ago"]);
    assert!(err.contains("every commit is newer than"), "{err}");
    let err = env.fails(&["restore", ".a", "--from", "HEAD~9"]);
    assert!(
        err.contains("the revision `HEAD~9` does not exist"),
        "{err}"
    );
    assert_eq!(env.read(".a"), b"three\n");
}

/// A truncated or deleted pack leaves the refs readable but not the commits they name, so
/// `restore` says the repo is damaged and names `doctor`, not that HEAD is no revision.
#[test]
fn restore_on_a_damaged_pack_says_the_repo_is_damaged() {
    for damage in ["truncate", "delete"] {
        let env = two_commits();
        let repo = env.repo().to_string_lossy().into_owned();
        env.git(&["-C", &repo, "gc", "-q"]);
        let packs = env.repo().join(".git/objects/pack");
        let pack = std::fs::read_dir(&packs)
            .expect("read the pack dir")
            .map(|entry| entry.expect("entry").path())
            .find(|path| path.extension().is_some_and(|ext| ext == "pack"))
            .expect("a pack after gc");
        if damage == "delete" {
            std::fs::remove_file(&pack).expect("delete the pack");
        } else {
            let bytes = std::fs::read(&pack).expect("read the pack");
            let half = bytes.len().checked_div(2).unwrap_or_default();
            std::fs::set_permissions(&pack, std::fs::Permissions::from_mode(0o644)).expect("chmod");
            std::fs::write(&pack, bytes.get(..half).expect("half")).expect("truncate");
        }
        for from in ["HEAD", "HEAD~1"] {
            let err = env.fails(&["restore", ".a", "--from", from]);
            assert!(
                err.contains("the repo is damaged") && err.contains("doctor` checks the repo"),
                "{damage} {from}: {err}"
            );
            assert!(!err.contains("neither a commit nor a date"), "{err}");
            if damage == "truncate" {
                assert!(err.contains("does not match index"), "git's line: {err}");
            }
        }
        assert_eq!(env.read(".a"), b"two\n");
    }
}

#[test]
fn a_dry_run_restore_changes_nothing() {
    let env = two_commits();
    let before = env.four_trees();
    env.ok(&["restore", ".a", "--from", "HEAD~1", "--dry-run"]);
    assert_eq!(env.four_trees(), before);
}

#[test]
fn restore_refuses_an_unmanaged_path() {
    let env = two_commits();
    env.write(".b", b"x", 0o644);
    assert!(env.fails(&["restore", ".b"]).contains("not managed"));
}

#[test]
fn log_filters_by_directory() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", b"a\n", 0o644);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    env.write(".config/x/c", b"c\n", 0o644);
    env.ok(&["add", ".config/x/c"]);
    env.ok(&["sync"]);
    let out = env.run(&["log", ".config/"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("x/c"), "{text}");
    assert!(text.contains("ago"), "{text}");
    let all = env.run(&["log"]);
    assert_eq!(String::from_utf8_lossy(&all.stdout).lines().count(), 2);
    let one = env.run(&["log", "-n", "1"]);
    assert_eq!(String::from_utf8_lossy(&one.stdout).lines().count(), 1);
}

#[test]
fn log_notes_a_shallow_history() {
    let env = two_commits();
    let source = env.root().join("source");
    let url = format!("file://{}", env.repo().display());
    env.git(&[
        "clone",
        "-q",
        "--depth",
        "1",
        &url,
        &source.to_string_lossy(),
    ]);
    let other = env.second_device();
    other.ok(&["init", &source.to_string_lossy()]);
    let out = other.run(&["log"]);
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("history is shallow; run `dotman git fetch --unshallow`"),
        "{err}"
    );
}

#[test]
fn git_passes_arguments_and_the_exit_code_through() {
    let env = two_commits();
    assert_eq!(
        env.run(&["git", "rev-parse", "--verify", "nosuch"])
            .status
            .code(),
        Some(128)
    );
    let out = env.run(&["git", "log", "--oneline", "-n", "1"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains(".a"));
}
