mod common;

use common::{TestEnv, mode_of, tree_checksum};
use dotman::lock::Lock;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Output;

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Runs `dotman`, asserts exit 3 and returns stderr.
fn attention(env: &TestEnv, args: &[&str]) -> String {
    let out = env.run(args);
    assert_eq!(out.status.code(), Some(3), "dot {args:?}: {}", stderr(&out));
    stderr(&out)
}

fn token(env: &TestEnv, key: &str) -> Option<String> {
    env.token_of("status", key)
}

/// A repo with `files` added, so R, H and S agree for each.
fn added(files: &[(&str, &[u8], u32)]) -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let mut args = vec!["add"];
    for (key, bytes, mode) in files {
        env.write(key, bytes, *mode);
        args.push(key);
    }
    env.ok(&args);
    env
}

#[test]
fn row_1_gone_everywhere_drops_the_state() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    fs::remove_file(env.repo().join(".bashrc")).unwrap();
    fs::remove_file(env.home().join(".bashrc")).unwrap();
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("gone"));
    env.ok(&["apply"]);
    assert_eq!(token(&env, ".bashrc"), None);
    assert!(!env.home().join(".bashrc").exists());
}

#[test]
fn row_2_forgotten_upstream_keeps_the_file_and_drops_the_state() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    fs::remove_file(env.repo().join(".bashrc")).unwrap();
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("forgotten"));
    let err = env.ok(&["apply"]);
    assert!(err.contains(".bashrc: forgotten upstream"), "{err}");
    assert_eq!(env.read(".bashrc"), b"one\n");
    assert_eq!(token(&env, ".bashrc"), None);
}

#[test]
fn row_3_forgotten_with_local_edits_is_reported_and_kept() {
    for answer in ["add", "forget"] {
        let env = added(&[(".bashrc", b"one\n", 0o644)]);
        fs::remove_file(env.repo().join(".bashrc")).unwrap();
        env.write(".bashrc", b"edited\n", 0o644);
        assert_eq!(token(&env, ".bashrc").as_deref(), Some("forgotten-edited"));
        let err = attention(&env, &["apply", "--take-repo"]);
        assert!(err.contains("forgotten upstream but edited here"), "{err}");
        assert_eq!(env.read(".bashrc"), b"edited\n");
        assert_eq!(token(&env, ".bashrc").as_deref(), Some("forgotten-edited"));

        let out = common::run_hint(&env, &err, answer);
        assert!(out.status.success(), "{answer}: {}", stderr(&out));
        assert_eq!(env.read(".bashrc"), b"edited\n");
        if answer == "add" {
            assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
            assert_eq!(fs::read(env.repo().join(".bashrc")).unwrap(), b"edited\n");
        } else {
            assert_eq!(token(&env, ".bashrc"), None);
        }
        env.ok(&["apply"]);
    }
}

#[test]
fn row_4_clean_records_a_missing_state() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
    fs::remove_file(env.state_dir().join("state.toml")).unwrap();
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
    let err = env.ok(&["status"]);
    assert!(
        err.contains("run `dotman apply` to record 1 path that already matches the repo"),
        "{err}"
    );
    env.ok(&["apply"]);
    assert!(!env.ok(&["status"]).contains("to record"));
    env.write(".bashrc", b"edited\n", 0o644);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("changed-locally"));
}

#[test]
fn row_5_not_applied_is_written() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".config/app/a.conf", b"a\n");
    assert_eq!(
        token(&env, ".config/app/a.conf").as_deref(),
        Some("not-applied")
    );
    env.ok(&["apply"]);
    assert_eq!(env.read(".config/app/a.conf"), b"a\n");
    assert_eq!(token(&env, ".config/app/a.conf").as_deref(), Some("clean"));
    assert!(!env.state_dir().join("backups").exists());
}

#[test]
fn row_6_deleted_locally_waits_for_restore_or_forget() {
    let env = added(&[(".bashrc", b"one\n", 0o644), (".zshrc", b"z\n", 0o600)]);
    assert!(env.repo_config().modes.contains_key(".zshrc"));
    fs::remove_file(env.home().join(".bashrc")).unwrap();
    fs::remove_file(env.home().join(".zshrc")).unwrap();
    for key in [".bashrc", ".zshrc"] {
        assert_eq!(
            token(&env, key).as_deref(),
            Some("deleted-locally"),
            "{key}"
        );
    }

    let err = attention(&env, &["apply"]);
    assert!(err.contains("local deletions left undecided (2)"), "{err}");
    assert!(
        err.contains(
            "answer with `dotman apply --forget-deleted` or `dotman apply --restore-deleted`"
        ),
        "{err}"
    );
    assert!(!env.home().join(".bashrc").exists());

    env.ok(&["apply", "--restore-deleted", ".bashrc"]);
    assert_eq!(env.read(".bashrc"), b"one\n");
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
    assert_eq!(token(&env, ".zshrc").as_deref(), Some("deleted-locally"));

    env.ok(&["apply", "--forget-deleted"]);
    assert!(!env.repo().join(".zshrc").exists());
    assert!(!env.home().join(".zshrc").exists());
    assert_eq!(token(&env, ".zshrc"), None);
    assert!(!env.repo_config().modes.contains_key(".zshrc"));
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
}

#[test]
fn row_7_deleted_here_and_changed_upstream_is_a_local_deletion() {
    let env = added(&[(".bashrc", b"one\n", 0o644), (".zshrc", b"z\n", 0o644)]);
    for (key, upstream) in [(".bashrc", b"two\n".as_slice()), (".zshrc", b"z 2\n")] {
        fs::remove_file(env.home().join(key)).unwrap();
        env.write_repo(key, upstream);
        assert_eq!(
            token(&env, key).as_deref(),
            Some("deleted-changed-upstream")
        );
    }
    // --take-repo answers conflicts only, so the hint never names it for a deletion.
    let err = attention(&env, &["apply", "--take-repo"]);
    assert!(err.contains("local deletions left undecided (2)"), "{err}");
    assert!(
        err.contains(
            "answer with `dotman apply --forget-deleted` or `dotman apply --restore-deleted`"
        ),
        "{err}"
    );
    assert!(!err.contains("--take-repo`"), "{err}");

    env.ok(&["apply", "--restore-deleted", ".bashrc"]);
    assert_eq!(env.read(".bashrc"), b"two\n");
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));

    env.ok(&["apply", "--forget-deleted", ".zshrc"]);
    assert!(!env.home().join(".zshrc").exists());
    assert!(!env.repo().join(".zshrc").exists());
    assert_eq!(token(&env, ".zshrc"), None);
}

#[test]
fn apply_never_says_nothing_to_apply_after_reporting_a_path_it_left_alone() {
    let env = added(&[(".a", b"a\n", 0o644)]);
    env.write(".a", b"changed here\n", 0o644);
    let err = env.ok(&["apply"]);
    assert!(err.contains(".a: changed locally"), "{err}");
    assert!(!err.contains("nothing to apply"), "{err}");
    assert!(
        err.contains("nothing written; the path above stays as it is"),
        "{err}"
    );

    env.write_machine_config("[age]\ncommand = \"/nonexistent/age\"\n");
    env.write_repo(
        ".s{age}",
        b"-----BEGIN AGE ENCRYPTED FILE-----\nx\n-----END AGE ENCRYPTED FILE-----\n",
    );
    let err = env.ok(&["apply"]);
    assert!(err.contains("locked"), "{err}");
    assert!(!err.contains("nothing to apply"), "{err}");
    assert!(
        err.contains("nothing written; the 2 paths above stay as they are"),
        "{err}"
    );
}

/// The path after `marker` on the line of `err` that has it.
fn path_after(err: &str, marker: &str) -> std::path::PathBuf {
    let path = err
        .lines()
        .find_map(|line| line.split_once(marker).map(|(_, path)| path.to_owned()));
    assert!(path.is_some(), "no `{marker}` in: {err}");
    path.unwrap_or_default().into()
}

#[test]
fn forget_deleted_saves_an_uncommitted_repo_edit_before_removing_it() {
    let env = added(&[(".zshrc", b"base\n", 0o644)]);
    env.ok(&["sync"]);
    env.write_repo(".zshrc", b"hand edit in the repo\n");
    fs::remove_file(env.home().join(".zshrc")).unwrap();
    assert_eq!(
        token(&env, ".zshrc").as_deref(),
        Some("deleted-changed-upstream")
    );
    let backups = env.state_dir().join("backups");

    let err = env.ok(&["apply", "--forget-deleted", "--dry-run"]);
    let would = path_after(
        &err,
        ".zshrc: would save the repo copy's uncommitted edit at ",
    );
    assert!(would.starts_with(&backups), "{err}");
    assert!(would.ends_with(".dotman/repo/.zshrc"), "{err}");
    assert!(!backups.exists(), "{err}");
    assert!(env.repo().join(".zshrc").exists());

    let err = env.ok(&["apply", "--forget-deleted"]);
    let saved = path_after(
        &err,
        ".zshrc: the repo copy's uncommitted edit is saved at ",
    );
    assert!(saved.starts_with(&backups), "{err}");
    assert_eq!(fs::read(&saved).unwrap(), b"hand edit in the repo\n");
    assert!(!env.repo().join(".zshrc").exists());
    assert_eq!(token(&env, ".zshrc"), None);
}

#[test]
fn the_printed_answer_to_each_deletion_does_what_it_says() {
    for (answer, kept) in [
        ("apply --restore-deleted", true),
        ("apply --forget-deleted", false),
    ] {
        let env = added(&[(".bashrc", b"one\n", 0o644)]);
        fs::remove_file(env.home().join(".bashrc")).unwrap();
        env.write_repo(".bashrc", b"two\n");
        let err = attention(&env, &["apply"]);
        let out = common::run_hint(&env, &err, answer);
        assert!(out.status.success(), "{answer}: {}", stderr(&out));
        assert_eq!(env.home().join(".bashrc").exists(), kept, "{answer}");
        assert_eq!(env.repo().join(".bashrc").exists(), kept, "{answer}");
        if kept {
            assert_eq!(env.read(".bashrc"), b"two\n");
        }
        env.ok(&["apply"]);
    }
}

#[test]
fn row_8_existing_file_is_backed_up_before_the_repo_version() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".bashrc", b"repo\n");
    // The repo's 0644 would give others read access the file lacks, so it stays 0640 (see below).
    env.write(".bashrc", b"mine\n", 0o640);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("existing"));

    let err = attention(&env, &["apply"]);
    assert!(err.contains("existing files left undecided (1)"), "{err}");
    assert_eq!(env.read(".bashrc"), b"mine\n");

    let err = env.ok(&["apply", "--take-repo"]);
    assert!(err.contains("backed up to"), "{err}");
    assert_eq!(env.read(".bashrc"), b"repo\n");
    assert_eq!(mode_of(&env.home().join(".bashrc")), 0o640);
    assert_eq!(env.backups(".bashrc"), [(b"mine\n".to_vec(), 0o600)]);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
}

#[test]
fn row_9_changed_locally_is_reported_and_never_overwritten() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    env.write(".bashrc", b"edited\n", 0o644);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("changed-locally"));
    let err = env.ok(&["apply", "--take-repo"]);
    assert!(err.contains(".bashrc: changed locally"), "{err}");
    assert_eq!(env.read(".bashrc"), b"edited\n");
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("changed-locally"));
}

#[test]
fn row_10_changed_in_repo_is_backed_up_before_it_is_written() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    env.write_repo(".bashrc", b"two\n");
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("changed-in-repo"));
    let err = env.ok(&["apply"]);
    assert!(err.contains("backed up to"), "{err}");
    assert_eq!(env.read(".bashrc"), b"two\n");
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
    assert_eq!(env.backups(".bashrc"), [(b"one\n".to_vec(), 0o600)]);
    assert_eq!(
        env.backups(".dotman/modes.txt"),
        [(b".bashrc\t0644\n".to_vec(), 0o600)]
    );
}

/// A write that changes only the mode says so, with both modes, instead of claiming a backup that
/// holds no copy; a dry run says the same in the conditional and records nothing.
#[test]
fn a_mode_only_write_names_the_modes_and_the_record() {
    let env = added(&[(".ssh/config", b"Host x\n", 0o644)]);
    let mut config = env.repo_config();
    config
        .modes
        .insert(".ssh/config".to_owned(), "0600".to_owned());
    config
        .save(&env.repo().join(".dotman.toml"), false)
        .unwrap();

    let err = env.ok(&["apply", "--dry-run"]);
    assert!(
        err.lines()
            .any(|line| line == "would apply .ssh/config (mode 0644 to 0600)"),
        "{err}"
    );
    assert!(
        err.contains("\n  would record the old mode in ") && err.contains("/.dotman/modes.txt\n"),
        "{err}"
    );
    assert!(!err.contains("backed up"), "{err}");
    assert_eq!(mode_of(&env.home().join(".ssh/config")), 0o644);
    assert_eq!(env.backups(".dotman/modes.txt"), []);

    let err = env.ok(&["apply"]);
    assert!(
        err.lines()
            .any(|line| line == "apply .ssh/config (mode 0644 to 0600)"),
        "{err}"
    );
    let record = err
        .lines()
        .find_map(|line| line.strip_prefix("  the old mode is recorded in "))
        .expect("a record line");
    assert_eq!(
        fs::read(record).unwrap(),
        b".ssh/config\t0644\n",
        "the printed path holds the old mode"
    );
}

/// The bytes are the same, so no copy is made, but the backup's `modes.txt` keeps the mode the
/// file had. `--in-place` copies it as before.
#[test]
fn a_mode_only_change_records_the_mode_it_replaces() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    let mut config = env.repo_config();
    config.modes.insert(".bashrc".to_owned(), "0600".to_owned());
    config
        .save(&env.repo().join(".dotman.toml"), false)
        .unwrap();
    let err = env.ok(&["apply"]);
    assert!(err.contains("the old mode is recorded in "), "{err}");
    assert_eq!(mode_of(&env.home().join(".bashrc")), 0o600);
    assert_eq!(env.backups(".bashrc"), []);
    assert_eq!(
        env.backups(".dotman/modes.txt"),
        [(b".bashrc\t0644\n".to_vec(), 0o600)]
    );

    config.modes.insert(".bashrc".to_owned(), "0640".to_owned());
    config
        .save(&env.repo().join(".dotman.toml"), false)
        .unwrap();
    env.ok(&["apply", "--in-place"]);
    assert_eq!(mode_of(&env.home().join(".bashrc")), 0o640);
    assert_eq!(env.backups(".bashrc"), [(b"one\n".to_vec(), 0o600)]);
    let mut records: Vec<Vec<u8>> = env
        .backups(".dotman/modes.txt")
        .into_iter()
        .map(|(bytes, _)| bytes)
        .collect();
    records.sort();
    assert_eq!(records, [b".bashrc\t0600\n", b".bashrc\t0644\n"]);
}

#[test]
fn an_absolute_link_into_home_is_replaced_when_the_repo_changes_it() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"x\n", 0o644);
    env.symlink(".config/link", env.home().join(".bashrc"));
    env.ok(&["add", ".config/link"]);
    let repo_link = env.repo().join(".config/link");
    assert_eq!(fs::read_link(&repo_link).unwrap(), Path::new("../.bashrc"));
    fs::remove_file(&repo_link).unwrap();
    symlink("../.profile", &repo_link).unwrap();
    assert_eq!(
        token(&env, ".config/link").as_deref(),
        Some("changed-in-repo")
    );
    env.ok(&["apply"]);
    assert_eq!(
        fs::read_link(env.home().join(".config/link")).unwrap(),
        Path::new("../.profile")
    );
    assert_eq!(token(&env, ".config/link").as_deref(), Some("clean"));
}

#[test]
fn an_unmodified_managed_link_is_replaced_when_upstream_makes_it_a_file() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write("dots/real.conf", b"real\n", 0o644);
    env.symlink("cfg.link", "dots/real.conf");
    env.symlink("other.link", "dots/real.conf");
    env.ok(&["add", "cfg.link", "other.link"]);
    for key in ["cfg.link", "other.link"] {
        fs::remove_file(env.repo().join(key)).unwrap();
        env.write_repo(key, b"now a file\n");
    }
    // Re-pointed here, so it is no longer the link dotman applied.
    fs::remove_file(env.home().join("other.link")).unwrap();
    env.symlink("other.link", "dots/elsewhere");
    assert_eq!(token(&env, "cfg.link").as_deref(), Some("changed-in-repo"));
    assert_eq!(
        token(&env, "other.link").as_deref(),
        Some("symlink-at-target")
    );

    let err = attention(&env, &["apply", "--take-repo"]);
    assert!(err.contains("other.link: is a symlink"), "{err}");
    assert_eq!(env.read("cfg.link"), b"now a file\n");
    assert!(
        !fs::symlink_metadata(env.home().join("cfg.link"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(env.read("dots/real.conf"), b"real\n");
    assert_eq!(
        env.backups(".dotman/links.txt"),
        [(b"cfg.link\tdots/real.conf\n".to_vec(), 0o600)]
    );
    assert_eq!(token(&env, "cfg.link").as_deref(), Some("clean"));
    assert_eq!(
        fs::read_link(env.home().join("other.link")).unwrap(),
        Path::new("dots/elsewhere")
    );
}

#[test]
fn row_11_both_changed_is_a_conflict() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    env.write_repo(".bashrc", b"repo\n");
    env.write(".bashrc", b"mine\n", 0o644);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("both-changed"));

    let err = attention(&env, &["apply"]);
    assert!(err.contains("conflicts left undecided (1)"), "{err}");
    assert!(
        err.contains("answer with `dotman apply --keep-local` or `dotman apply --take-repo`"),
        "{err}"
    );
    assert_eq!(env.read(".bashrc"), b"mine\n");

    let out = common::run_hint(&env, &err, "apply --take-repo");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(env.read(".bashrc"), b"repo\n");
    assert_eq!(env.backups(".bashrc"), [(b"mine\n".to_vec(), 0o600)]);
    assert_eq!(token(&env, ".bashrc").as_deref(), Some("clean"));
}

#[test]
fn the_conflicts_are_listed_before_the_question_even_with_quiet() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    env.write_repo(".bashrc", b"repo\n");
    env.write(".bashrc", b"mine\n", 0o644);
    let plain = [("NO_COLOR", "1"), ("TERM", "dumb")];
    let Some(seen) = common::in_a_terminal(&env, &plain, &["-q", "apply"], b"s\n") else {
        eprintln!("skipped: no script(1) to give dotman a terminal");
        return;
    };
    assert!(seen.contains("\n1 conflict:"), "{seen}");
    assert!(
        seen.contains("  both-changed              .bashrc"),
        "{seen}"
    );
    assert!(
        seen.contains("[k] keep all local  [t] take all from repo  [d] show diffs"),
        "{seen}"
    );
    assert_eq!(env.read(".bashrc"), b"mine\n");
}

#[test]
fn keep_local_says_the_next_sync_shares_it_and_the_printed_variant_keeps_it_here() {
    let env = TestEnv::new();
    env.write_machine_config("[identity]\nhost = \"box\"\n");
    env.ok(&["init"]);
    env.write(".bashrc", b"one\n", 0o644);
    env.ok(&["add", ".bashrc"]);
    env.write_repo(".bashrc", b"repo\n");
    env.write(".bashrc", b"mine\n", 0o644);
    let err = attention(&env, &["apply"]);
    let out = common::run_hint(&env, &err, "apply --keep-local");
    assert!(out.status.success(), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(
        err.contains("kept; the next `dotman sync` captures it into the repo for every machine"),
        "{err}"
    );
    assert_eq!(env.read(".bashrc"), b"mine\n");

    assert_eq!(
        common::hint(&err, "add --for").get(2).map(String::as_str),
        Some("host=box"),
        "{err}"
    );
    let out = common::run_hint(&env, &err, "add --for");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        fs::read(env.repo().join(".bashrc{host=box}")).unwrap(),
        b"mine\n"
    );
    assert_eq!(fs::read(env.repo().join(".bashrc")).unwrap(), b"repo\n");
    assert_eq!(
        env.porcelain("status"),
        [("clean".to_owned(), ".bashrc".to_owned())]
    );
}

#[test]
fn blocked_directory_is_reported_and_left_alone() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".config/app.conf", b"x\n");
    fs::create_dir_all(env.home().join(".config/app.conf/inner")).unwrap();
    assert_eq!(token(&env, ".config/app.conf").as_deref(), Some("blocked"));
    let before = tree_checksum(&env.home());
    let err = attention(&env, &["apply", "--take-repo"]);
    assert!(err.contains("a directory is in the way"), "{err}");
    assert_eq!(tree_checksum(&env.home()), before);
    assert_eq!(token(&env, ".config/app.conf").as_deref(), Some("blocked"));

    let out = common::run_hint(&env, &err, "forget");
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(tree_checksum(&env.home()), before);
    assert_eq!(token(&env, ".config/app.conf"), None);
    env.ok(&["apply"]);
}

#[test]
fn every_skip_the_user_must_clear_names_forget_as_a_way_out() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".link-here", b"x\n");
    env.write_repo(".config/app/a.conf", b"a\n");
    env.write_repo(".fifo", b"f\n");
    env.symlink(".link-here", "elsewhere");
    fs::create_dir_all(env.root().join("outside")).unwrap();
    env.symlink(".config", env.root().join("outside"));
    let fifo = env.home().join(".fifo");
    let made = std::process::Command::new("mkfifo").arg(&fifo).status();
    assert!(made.is_ok_and(|status| status.success()));
    // A checksum would block reading the FIFO, so the obstacles are checked one by one.
    let obstacles = || {
        let kind = |key: &str| {
            fs::symlink_metadata(env.home().join(key))
                .unwrap()
                .file_type()
        };
        (
            fs::read_link(env.home().join(".link-here")).unwrap(),
            kind(".config").is_symlink(),
            std::os::unix::fs::FileTypeExt::is_fifo(&kind(".fifo")),
            fs::read_dir(env.root().join("outside")).unwrap().count(),
        )
    };
    let before = obstacles();

    let err = attention(&env, &["apply"]);
    for (key, why) in [
        (".link-here", "is a symlink, which is never written through"),
        (
            ".config/app/a.conf",
            "`.config` is a symlink or not a directory",
        ),
        (".fifo", "a socket, FIFO or device is in the way"),
    ] {
        let line = err
            .lines()
            .find(|line| line.starts_with(&format!("{key}: ")));
        let line = line.unwrap_or_default();
        assert!(line.contains(why), "{line}");
        let out = common::run_hint(&env, line, "forget");
        assert!(out.status.success(), "{}", stderr(&out));
        assert_eq!(token(&env, key), None, "{key}");
    }
    assert_eq!(obstacles(), before);
    assert!(before.1 && before.2);
    env.ok(&["apply"]);
}

#[test]
fn fresh_machine_non_interactive_init_skips_existing_files() {
    let a = added(&[(".bashrc", b"from A\n", 0o644), (".vimrc", b"vim\n", 0o644)]);
    a.push_repo();
    let b = a.second_device();
    b.write(".bashrc", b"B's own\n", 0o644);

    let err = attention(&b, &["init", &a.remote_url()]);
    assert!(err.contains("existing files left undecided (1)"), "{err}");
    assert!(err.contains("  .bashrc"), "{err}");
    assert_eq!(b.read(".bashrc"), b"B's own\n");
    assert_eq!(b.read(".vimrc"), b"vim\n");
    assert_eq!(token(&b, ".bashrc").as_deref(), Some("existing"));
    assert!(!b.state_dir().join("backups").exists());
    // A rerun finishes setting up the machine it cloned to, and still leaves the file alone.
    let err = attention(&b, &["init", &a.remote_url()]);
    assert!(err.contains("existing files left undecided (1)"), "{err}");
    assert_eq!(b.read(".bashrc"), b"B's own\n");
}

#[test]
fn fresh_machine_take_repo_backs_up_the_original() {
    let a = added(&[(".bashrc", b"from A\n", 0o644)]);
    a.push_repo();
    let b = a.second_device();
    b.write(".bashrc", b"B's own\n", 0o644);

    let err = b.ok(&["init", &a.remote_url(), "--take-repo"]);
    assert!(
        err.contains("1 existing"),
        "the status summary comes first: {err}"
    );
    assert_eq!(b.read(".bashrc"), b"from A\n");
    assert_eq!(b.backups(".bashrc"), [(b"B's own\n".to_vec(), 0o600)]);
    assert_eq!(token(&b, ".bashrc").as_deref(), Some("clean"));
}

#[test]
fn fresh_machine_keep_local_leaves_the_file_changed_locally() {
    let a = added(&[(".bashrc", b"from A\n", 0o644)]);
    a.push_repo();
    let b = a.second_device();
    b.write(".bashrc", b"B's own\n", 0o644);

    b.ok(&["init", &a.remote_url(), "--keep-local"]);
    assert_eq!(b.read(".bashrc"), b"B's own\n");
    assert_eq!(token(&b, ".bashrc").as_deref(), Some("changed-locally"));
    assert!(!b.state_dir().join("backups").exists());
}

#[test]
fn hostile_symlinks_never_carry_a_write_outside_home() {
    let a = TestEnv::new();
    let outside = a.root().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("kept"), b"kept\n").unwrap();
    fs::write(a.root().join("outside-file"), b"outside\n").unwrap();
    a.ok(&["init"]);
    symlink(&outside, a.repo().join(".config")).unwrap();
    a.write_repo(".bashrc", b"repo\n");
    a.push_repo();
    let outside_before = tree_checksum(&outside);

    let b = a.second_device();
    b.symlink(".bashrc", a.root().join("outside-file"));
    let err = attention(&b, &["init", &a.remote_url(), "--take-repo"]);
    assert!(err.contains(".bashrc: is a symlink"), "{err}");
    assert_eq!(token(&b, ".bashrc").as_deref(), Some("symlink-at-target"));
    assert_eq!(
        fs::read(a.root().join("outside-file")).unwrap(),
        b"outside\n"
    );
    assert_eq!(fs::read_link(b.home().join(".config")).unwrap(), outside);

    // Upstream turns `.config` into a directory with a file in it; this HOME still has the link.
    fs::remove_file(b.repo().join(".config")).unwrap();
    b.write_repo(".config/x", b"pwned\n");
    let err = attention(&b, &["apply", "--take-repo"]);
    assert!(
        err.contains("`.config` is a symlink or not a directory"),
        "{err}"
    );
    assert_eq!(token(&b, ".config/x").as_deref(), Some("blocked"));
    assert_eq!(tree_checksum(&outside), outside_before);
    assert!(!outside.join("x").exists());
}

#[test]
fn recorded_modes_are_applied_without_special_bits() {
    let a = TestEnv::new();
    a.ok(&["init"]);
    a.write(".local/bin/tool", b"#!/bin/sh\n", 0o755);
    a.write(".ssh/config", b"Host *\n", 0o600);
    fs::set_permissions(a.home().join(".ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    a.ok(&["add", ".local/bin/tool", ".ssh/config"]);
    let mut config = a.repo_config();
    config
        .modes
        .insert(".local/bin/tool".to_owned(), "4755".to_owned());
    config.save(&a.repo().join(".dotman.toml"), false).unwrap();
    a.push_repo();

    let b = a.second_device();
    b.ok(&["init", &a.remote_url()]);
    assert_eq!(mode_of(&b.home().join(".local/bin/tool")), 0o755);
    assert_eq!(mode_of(&b.home().join(".ssh/config")), 0o600);
    assert_eq!(mode_of(&b.home().join(".ssh")), 0o700);
    assert_eq!(b.read(".ssh/config"), b"Host *\n");
    for key in [".local/bin/tool", ".ssh/config"] {
        assert_eq!(token(&b, key).as_deref(), Some("clean"), "{key}");
    }
}

#[test]
fn a_private_file_is_never_widened_to_git_default_mode() {
    // A git-tree repo carries only git's 100644; `[modes]` says nothing about these keys.
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".ssh/config", b"Host repo\n");
    env.write_repo(".ssh/known", b"same\n");
    env.write_repo(".netrc", b"repo\n");
    env.write(".ssh/config", b"Host mine\n", 0o600);
    env.write(".ssh/known", b"same\n", 0o600);
    env.write(".netrc", b"mine\n", 0o600);
    let mut config = env.repo_config();
    config.modes.insert(".netrc".to_owned(), "0644".to_owned());
    config
        .save(&env.repo().join(".dotman.toml"), false)
        .unwrap();

    let err = env.ok(&["apply", "--take-repo"]);
    assert!(
        err.contains(".ssh/config: kept at 0600 here; the repo says 0644"),
        "{err}"
    );
    assert_eq!(env.read(".ssh/config"), b"Host repo\n");
    assert_eq!(mode_of(&env.home().join(".ssh/config")), 0o600);
    assert_eq!(mode_of(&env.home().join(".ssh/known")), 0o600);
    assert_eq!(
        env.backups(".ssh/config"),
        [(b"Host mine\n".to_vec(), 0o600)]
    );
    // An explicit `[modes]` entry wins.
    assert_eq!(mode_of(&env.home().join(".netrc")), 0o644);

    // The kept mode is this machine's: the files read as clean, and a sync records nothing.
    for key in [".ssh/config", ".ssh/known", ".netrc"] {
        assert_eq!(token(&env, key).as_deref(), Some("clean"), "{key}");
    }
    let err = env.ok(&["status"]);
    assert!(
        err.contains(".ssh/config: kept at 0600 here; the repo says 0644"),
        "{err}"
    );
    env.ok(&["sync"]);
    let modes = env.repo_config().modes;
    assert_eq!(modes.get(".ssh/config"), None);
    assert_eq!(modes.get(".ssh/known"), None);
    assert_eq!(mode_of(&env.home().join(".ssh/config")), 0o600);
}

/// Without a `[modes]` entry the repo never gives group or other a read or write bit the file in
/// `$HOME` lacks, whatever the content, while execute bits follow the repo.
#[test]
fn group_and_other_never_gain_access_and_execute_follows_the_repo() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".config/app/shared.conf", b"repo\n");
    env.write_repo(".netrc", b"machine repo\n");
    env.write_repo("bin/run", b"#!/bin/sh\n");
    fs::set_permissions(
        env.repo().join("bin/run"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    env.write(".config/app/shared.conf", b"mine\n", 0o640);
    env.write(".netrc", b"machine mine\n", 0o600);
    env.write("bin/run", b"#!/bin/sh\n# mine\n", 0o644);

    env.ok(&["apply", "--take-repo"]);
    assert_eq!(env.read(".netrc"), b"machine repo\n");
    for (key, mode) in [
        (".config/app/shared.conf", 0o640),
        (".netrc", 0o600),
        ("bin/run", 0o755),
    ] {
        assert_eq!(mode_of(&env.home().join(key)), mode, "{key}");
        assert_eq!(token(&env, key).as_deref(), Some("clean"), "{key}");
    }
}

#[test]
fn ignored_names_in_the_worktree_are_not_keys() {
    let env = added(&[(".config/app/a.conf", b"a\n", 0o644)]);
    for leftover in [
        ".config/app/a.conf.orig",
        ".config/app/a.conf.rej",
        ".config/app/.a.conf.swp",
    ] {
        env.write_repo(leftover, b"merge leftover\n");
    }
    env.write_repo(".config/app/node_modules/x.js", b"x\n");
    let rows = env.porcelain("status");
    assert_eq!(
        rows,
        [("clean".to_owned(), ".config/app/a.conf".to_owned())]
    );
    env.ok(&["apply"]);
    for leftover in [".config/app/a.conf.orig", ".config/app/node_modules/x.js"] {
        assert!(!env.home().join(leftover).exists(), "{leftover}");
    }
}

#[test]
fn a_malformed_mode_fails_a_dry_run() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    let config = env.repo().join(".dotman.toml");
    let text = fs::read_to_string(&config).unwrap();
    assert!(text.contains("\n[modes]\n"), "{text}");
    fs::write(
        &config,
        text.replace("\n[modes]\n", "\n[modes]\n\".bashrc\" = \"rwx\"\n"),
    )
    .unwrap();
    let before = env.four_trees();
    let err = env.fails(&["apply", "--dry-run"]);
    assert_eq!(env.four_trees(), before);
    assert!(err.contains("[modes] entry `.bashrc`"), "{err}");
}

#[test]
fn a_moved_repo_keeps_its_state_and_another_repo_starts_empty() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    env.write(".bashrc", b"edited\n", 0o644);
    let at = |repo: &Path, args: &[&str]| {
        env.dot()
            .env("DOTMAN_REPO_PATH", repo)
            .args(args)
            .output()
            .expect("run dot")
    };
    let status_at = |repo: &Path| {
        String::from_utf8_lossy(&at(repo, &["status", "--porcelain"]).stdout).into_owned()
    };

    let moved = env.root().join("moved");
    fs::rename(env.repo(), &moved).unwrap();
    assert_eq!(status_at(&moved), "changed-locally\t.bashrc\0");

    let other = env.root().join("other");
    assert!(at(&other, &["init"]).status.success());
    fs::write(other.join(".bashrc"), b"one\n").unwrap();
    assert_eq!(status_at(&other), "existing\t.bashrc\0");
}

#[test]
fn dry_run_apply_changes_none_of_the_four_trees() {
    let env = added(&[
        (".bashrc", b"one\n", 0o644),
        (".zshrc", b"z\n", 0o644),
        (".profile", b"p\n", 0o644),
    ]);
    env.write_repo(".bashrc", b"repo\n");
    env.write(".bashrc", b"mine\n", 0o644);
    fs::remove_file(env.home().join(".zshrc")).unwrap();
    env.write_repo(".profile", b"p 2\n");
    env.write_repo(".config/new/app.conf", b"new\n");

    let before = env.four_trees();
    let args = ["apply", "--dry-run", "--take-repo", "--forget-deleted"];
    let err = env.ok(&args);
    for expected in ["would apply", "would back up to", "would forget"] {
        assert!(err.contains(expected), "missing `{expected}` in:\n{err}");
    }
    assert_eq!(env.four_trees(), before);

    env.ok(&["apply", "--take-repo", "--forget-deleted"]);
    assert_ne!(env.four_trees(), before);
}

#[test]
fn dry_run_init_from_a_url_changes_none_of_the_four_trees() {
    let a = added(&[(".bashrc", b"from A\n", 0o644), (".vimrc", b"vim\n", 0o644)]);
    a.push_repo();
    let b = a.second_device();
    b.write(".bashrc", b"B's own\n", 0o644);
    let tmp = b.root().join("tmp");
    fs::create_dir(&tmp).unwrap();

    let before = b.four_trees();
    let out = b
        .dot()
        .env("TMPDIR", &tmp)
        .args(["init", &a.remote_url(), "--dry-run", "--take-repo"])
        .output()
        .unwrap();
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    for expected in ["would clone", "would apply", "would back up to"] {
        assert!(err.contains(expected), "missing `{expected}` in:\n{err}");
    }
    assert_eq!(b.four_trees(), before);
    assert!(!b.repo().exists() && !b.state_dir().exists());
    assert_eq!(
        fs::read_dir(&tmp).unwrap().count(),
        0,
        "the temp clone is removed"
    );
}

#[test]
fn a_second_apply_fails_while_the_lock_is_held() {
    let env = added(&[(".bashrc", b"one\n", 0o644)]);
    env.write_repo(".bashrc", b"two\n");
    let held = Lock::acquire(&env.state_dir().join("lock"), false).unwrap();
    assert!(held.is_held());
    for args in [&["apply"][..], &["apply", "--dry-run"]] {
        let err = env.fails(args);
        assert!(err.contains("another dotman process is running"), "{err}");
    }
    assert_eq!(env.read(".bashrc"), b"one\n");
}

#[test]
fn diff_prints_a_unified_diff_from_home_to_repo() {
    let env = added(&[(".bashrc", b"one\ntwo\n", 0o644), (".zshrc", b"z\n", 0o644)]);
    env.write(".bashrc", b"one\nTWO\n", 0o644);
    env.write_repo(".config/new.conf", b"new\n");
    let tmp = env.root().join("tmp");
    fs::create_dir(&tmp).unwrap();
    let before = env.four_trees();
    let diff = |args: &[&str]| {
        let out = env
            .dot()
            .env("TMPDIR", &tmp)
            .arg("diff")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    let all = diff(&[]);
    for expected in [
        "diff --git repo/.bashrc home/.bashrc",
        "--- repo/.bashrc",
        "+++ home/.bashrc",
        "@@ ",
        "-two",
        "+TWO",
        "--- repo/.config/new.conf",
    ] {
        assert!(all.contains(expected), "missing `{expected}` in:\n{all}");
    }
    assert!(!all.contains(".zshrc"), "{all}");
    assert!(!all.contains('\u{1b}'), "no colour into a pipe:\n{all}");

    let one = diff(&["--no-pager", ".bashrc"]);
    assert!(one.contains("+TWO") && !one.contains("new.conf"), "{one}");
    assert_eq!(diff(&[".zshrc"]), "");
    assert_eq!(env.four_trees(), before);
    assert_eq!(
        fs::read_dir(&tmp).unwrap().count(),
        0,
        "temp trees are removed"
    );
}

#[test]
fn special_bits_in_home_are_not_tracked_so_the_file_settles() {
    let env = added(&[("bin/tool", b"#!/bin/sh\n", 0o755)]);
    fs::set_permissions(
        env.home().join("bin/tool"),
        fs::Permissions::from_mode(0o2755),
    )
    .unwrap();
    assert_eq!(token(&env, "bin/tool").as_deref(), Some("clean"));
    let err = env.ok(&["sync"]);
    assert_eq!(
        err.matches("bin/tool: has the setgid bit, which dotman does not track")
            .count(),
        1,
        "once per run: {err}"
    );
    assert!(!err.contains("capture bin/tool"), "{err}");
    assert!(!env.ok(&["sync"]).contains("capture bin/tool"));
    assert_eq!(mode_of(&env.home().join("bin/tool")), 0o2755);
}

#[test]
fn edit_applies_nothing_when_another_run_changed_the_state_meanwhile() {
    let env = added(&[(".a", b"a\n", 0o644), (".b", b"b\n", 0o644)]);
    env.write_repo(".b", b"b upstream\n");
    // While the editor is open, another run applies `.b`, which changes the state.
    let editor = env.write_script(
        "ed.sh",
        &format!(
            "#!/bin/sh\necho edited >> \"$1\"\n\"{}\" apply .b >/dev/null 2>&1\n",
            env!("CARGO_BIN_EXE_dotman")
        ),
    );
    let editor = editor.to_string_lossy().into_owned();
    let out = env.run_with(&[("EDITOR", &editor), ("VISUAL", "")], &["edit", ".a"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("changed the state while the editor was open"),
        "{err}"
    );
    assert!(err.contains("run `dotman status`"), "{err}");
    assert_eq!(env.read(".a"), b"a\n");
    assert_eq!(env.read(".b"), b"b upstream\n");
    assert_eq!(fs::read(env.repo().join(".a")).unwrap(), b"a\nedited\n");
}

#[test]
fn edit_without_an_editor_names_visual_and_editor() {
    let env = added(&[(".a", b"a\n", 0o644)]);
    let vars = [("EDITOR", "dotman-no-such-editor"), ("VISUAL", "")];
    let out = env.run_with(&vars, &["edit", ".a"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("`dotman-no-such-editor` is not on PATH"),
        "{err}"
    );
    assert!(err.contains("set $VISUAL or $EDITOR"), "{err}");
}

/// Under umask 0277 the directories dotman creates to write into are still 0700, so apply backs
/// up both files it replaces, the second one's mode into the record the first one created, and a
/// second run opens the lock file and writes into the backups directory the first run made. diff
/// writes its two sides into a temp directory the same way.
#[test]
fn apply_under_umask_0277_backs_up_and_replaces() {
    let env = added(&[(".a", b"a\n", 0o644), (".b", b"b\n", 0o644)]);
    fs::remove_file(env.state_dir().join("lock")).expect("remove the lock file");
    let under_0277 = |command: &str| {
        let out = env
            .process_for(Path::new("/bin/sh"))
            .args(["-c", "umask 0277; exec \"$0\" \"$@\""])
            .arg(env!("CARGO_BIN_EXE_dotman"))
            .arg(command)
            .output()
            .expect("run dotman");
        assert_eq!(out.status.code(), Some(0), "{command}: {}", stderr(&out));
        out
    };
    let apply = || {
        under_0277("apply");
    };
    let saved = |key: &str| -> Vec<Vec<u8>> {
        env.backups(key)
            .into_iter()
            .map(|(bytes, _)| bytes)
            .collect()
    };
    env.write_repo(".a", b"a upstream\n");
    env.write_repo(".b", b"b upstream\n");
    apply();
    assert_eq!(env.read(".a"), b"a upstream\n");
    assert_eq!(env.read(".b"), b"b upstream\n");
    assert_eq!(saved(".a"), [b"a\n".to_vec()]);
    assert_eq!(saved(".b"), [b"b\n".to_vec()]);
    let backups = env.state_dir().join("backups");
    assert_eq!(mode_of(&backups), 0o700);
    let run = fs::read_dir(&backups)
        .expect("read backups")
        .next()
        .expect("a run")
        .expect("entry")
        .path();
    let modes = fs::read_to_string(run.join(".dotman/modes.txt")).expect("modes record");
    assert_eq!(modes.lines().count(), 2, "{modes}");

    env.write_repo(".a", b"a again\n");
    apply();
    assert_eq!(env.read(".a"), b"a again\n");
    assert_eq!(saved(".a").len(), 2);

    env.write(".config/x/c", b"c\n", 0o644);
    env.ok(&["add", ".config/x/c"]);
    env.write(".config/x/c", b"c edited\n", 0o644);
    let shown = String::from_utf8_lossy(&under_0277("diff").stdout).into_owned();
    assert!(shown.contains("+c edited"), "{shown}");
}
