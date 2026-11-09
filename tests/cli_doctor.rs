mod common;

use common::{TestEnv, tree_checksum};
use dotman::state::State;
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn doctor(env: &TestEnv, args: &[&str]) -> (Option<i32>, String) {
    let mut full = vec!["doctor"];
    full.extend_from_slice(args);
    let out = env.run(&full);
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn line<'a>(out: &'a str, tag: &str, needle: &str) -> Option<&'a str> {
    out.lines()
        .find(|l| l.starts_with(tag) && l.contains(needle))
}

fn healthy() -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env
}

#[test]
fn a_healthy_environment_exits_0() {
    let env = healthy();
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(!out.lines().any(|l| l.starts_with("fail")), "{out}");
    assert!(line(&out, "pass", "git identity").is_some(), "{out}");
    assert!(line(&out, "info", "backups").is_some(), "{out}");
}

#[test]
fn a_missing_identity_fails() {
    let env = healthy();
    fs::write(env.home().join(".gitconfig"), "").expect("write .gitconfig");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(line(&out, "fail", "git identity").is_some(), "{out}");
}

#[test]
fn a_corrupt_state_is_named_and_fails() {
    let env = healthy();
    let state = env.state_dir().join("state.toml");
    fs::create_dir_all(env.state_dir()).expect("create state dir");
    fs::write(&state, "not = [toml").expect("write state");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        line(&out, "fail", &state.to_string_lossy()).is_some(),
        "{out}"
    );
}

#[test]
fn a_wide_state_dir_warns() {
    let env = healthy();
    fs::create_dir_all(env.state_dir()).expect("create state dir");
    fs::set_permissions(env.state_dir(), fs::Permissions::from_mode(0o755)).expect("chmod");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(line(&out, "warn", "state dir").is_some(), "{out}");
}

/// An unfinished merge is named with the merge's own commands, and the continue step needs no
/// editor.
#[test]
fn an_unfinished_operation_gets_its_own_commands() {
    let env = healthy();
    fs::write(env.repo().join(".git/MERGE_HEAD"), "x").expect("mark the merge");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    let fail = line(&out, "fail", "a merge is in progress").unwrap_or_default();
    assert!(
        fail.ends_with(
            "run dotman git -c core.editor=true merge --continue or dotman git merge --abort"
        ),
        "{out}"
    );
    fs::remove_file(env.repo().join(".git/MERGE_HEAD")).expect("clear the merge");
    fs::write(env.repo().join(".git/BISECT_LOG"), "x").expect("mark the bisect");
    let (_, out) = doctor(&env, &[]);
    let fail = line(&out, "fail", "a bisect is in progress").unwrap_or_default();
    assert!(fail.ends_with("run dotman git bisect reset"), "{out}");
}

#[test]
fn online_reaches_a_file_remote() {
    let env = healthy();
    let (code, out) = doctor(&env, &["--online"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(line(&out, "pass", "remote reachable").is_some(), "{out}");
}

/// A healthy repo with `.bashrc` committed and pushed.
fn committed() -> TestEnv {
    let env = healthy();
    env.write(".bashrc", b"x\n", 0o644);
    env.ok(&["add", ".bashrc"]);
    env.ok(&["sync"]);
    env
}

/// The same check and step as sync's, and the step run as printed clears it.
#[test]
fn a_stale_git_lock_fails_with_the_removal_step_that_works() {
    let env = committed();
    let lock = env.repo().join(".git/index.lock");
    fs::write(&lock, b"").expect("leave a lock");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    let failed = line(&out, "fail", "git's lock files are in the repo").unwrap_or_default();
    assert!(failed.contains(&lock.display().to_string()), "{out}");
    let step = failed.split('`').nth(1).unwrap_or_default();
    assert!(step.starts_with("rm "), "{out}");
    let ran = env
        .process_for(std::path::Path::new("sh"))
        .args(["-c", step])
        .output()
        .expect("run the step");
    assert!(ran.status.success(), "{step}");
    assert!(!lock.exists());
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
}

const PINNED: &str = "* -text -filter -ident -working-tree-encoding merge\n";

/// The attributes lines of older dotman versions, without `merge` or with `merge=text`, and a
/// pinned line that is not last, are flagged before any command writes. The step doctor prints,
/// run as printed, puts the pinned line last, keeps the user's lines, and leaves a pending change
/// out of `$HOME`. A user's line above the pinned one passes.
#[test]
fn an_attributes_file_without_the_pinned_line_last_warns_with_a_step_that_works() {
    let env = committed();
    let attributes = env.repo().join(".git/info/attributes");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(line(&out, "pass", "git attributes").is_some(), "{out}");
    env.write_repo(".bashrc", b"from upstream\n");

    for old in [
        "* -text -filter -ident -working-tree-encoding\n*.age diff=age\n".to_owned(),
        "* -text -filter -ident -working-tree-encoding merge=text\n".to_owned(),
        format!("{PINNED}*.age diff=age\n"),
    ] {
        fs::write(&attributes, &old).expect("age it");
        let (code, out) = doctor(&env, &[]);
        assert_eq!(code, Some(0), "{out}");
        let warned = line(&out, "warn", "git attributes").unwrap_or_default();
        assert!(warned.contains(&attributes.display().to_string()), "{out}");
        assert_eq!(
            fs::read_to_string(&attributes).expect("doctor writes nothing"),
            old
        );
        let home = tree_checksum(&env.home());
        let ran = common::run_hint(&env, warned, "init");
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        assert_eq!(tree_checksum(&env.home()), home, "{old}");
        let user = if old.contains("diff=age") {
            "*.age diff=age\n"
        } else {
            ""
        };
        assert_eq!(
            fs::read_to_string(&attributes).expect("read"),
            format!("{user}{PINNED}")
        );
        let (_, out) = doctor(&env, &[]);
        assert!(line(&out, "pass", "git attributes").is_some(), "{out}");
    }
    assert_eq!(env.read(".bashrc"), b"x\n");
}

/// When no command can write the attributes file, as with a read-only `.git/info` or a directory
/// in its place, doctor fails, names the file and why, and writes nothing.
#[test]
fn attributes_that_no_command_can_write_fail() {
    let env = committed();
    let info = env.repo().join(".git/info");
    let attributes = info.join("attributes");
    fs::write(&attributes, "* -text\n").expect("age it");
    fs::set_permissions(&info, fs::Permissions::from_mode(0o555)).expect("chmod");
    let (code, out) = doctor(&env, &[]);
    fs::set_permissions(&info, fs::Permissions::from_mode(0o755)).expect("chmod");
    assert_eq!(code, Some(1), "{out}");
    let failed = line(&out, "fail", "git attributes").unwrap_or_default();
    assert!(
        failed.contains(&format!(
            "{} is not a directory you can write to",
            info.display()
        )),
        "{out}"
    );
    assert_eq!(fs::read_to_string(&attributes).expect("read"), "* -text\n");

    fs::remove_file(&attributes).expect("remove it");
    fs::create_dir(&attributes).expect("a directory in its place");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    let failed = line(&out, "fail", "git attributes").unwrap_or_default();
    assert!(failed.contains("it cannot be read"), "{out}");
    assert!(attributes.is_dir());
}

#[test]
fn damaged_objects_fail_the_object_check() {
    let env = committed();
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
    assert!(line(&out, "pass", "repo objects").is_some(), "{out}");

    let objects = env.repo().join(".git/objects");
    let loose = tree_checksum(&objects)
        .into_keys()
        .filter(|name| name.len() == 41 && name.as_bytes().get(2) == Some(&b'/'));
    for name in loose {
        let file = objects.join(name);
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).expect("chmod");
        fs::write(&file, b"").expect("truncate");
    }
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    let failed = line(&out, "fail", "repo objects").unwrap_or_default();
    assert!(
        failed.contains("git fsck failed") && failed.contains("init <url>"),
        "{out}"
    );
}

#[test]
fn an_empty_or_missing_dotman_toml_fails_with_the_restore_command() {
    let env = committed();
    let repo = env.repo().to_string_lossy().into_owned();
    let config = env.repo().join(".dotman.toml");
    assert_ne!(env.git(&["-C", &repo, "show", "HEAD:.dotman.toml"]), "");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
    // A newline or a comment alone holds no settings, so it loses them just as an empty file does.
    for (how, written) in [
        ("is empty", Some(&b""[..])),
        ("is missing", None),
        ("holds no settings", Some(b"\n")),
        ("holds no settings", Some(b"# todo\n")),
    ] {
        match written {
            Some(bytes) => fs::write(&config, bytes).expect("overwrite"),
            None => fs::remove_file(&config).expect("remove"),
        }
        let (code, out) = doctor(&env, &[]);
        assert_eq!(code, Some(1), "{how}: {out}");
        let failed = line(&out, "fail", ".dotman.toml").unwrap_or_default();
        assert!(failed.contains(how), "{how}: {out}");
        assert!(
            failed.contains("dotman git checkout -- .dotman.toml"),
            "{how}: {out}"
        );
        env.git(&["-C", &repo, "checkout", "--", ".dotman.toml"]);
        let (code, out) = doctor(&env, &[]);
        assert_eq!(code, Some(0), "restored after {how}: {out}");
    }

    fs::write(&config, b"").expect("truncate");
    env.git(&["-C", &repo, "commit", "-qam", "an empty config"]);
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "the last commit has it empty too: {out}");
    assert!(line(&out, "fail", ".dotman.toml").is_none(), "{out}");
}

#[test]
fn a_half_made_repo_is_named_with_a_fix_that_keeps_commits() {
    let repo_line = |env: &TestEnv| {
        let (code, out) = doctor(env, &[]);
        assert_eq!(code, Some(1), "{out}");
        assert!(
            line(&out, "fail", "repo id").is_none(),
            "no circular hint: {out}"
        );
        assert!(
            line(&out, "pass", "repo ").is_none(),
            "an incomplete repo never also passes: {out}"
        );
        line(&out, "fail", "is incomplete")
            .unwrap_or_default()
            .to_owned()
    };
    let repo = |env: &TestEnv| env.repo().display().to_string();

    let env = TestEnv::new();
    env.ok(&["init"]);
    fs::remove_file(env.repo().join(".git/dotman-id")).expect("remove id");
    let failed = repo_line(&env);
    assert!(failed.contains(".git/dotman-id is missing"), "{failed}");
    assert!(
        failed.contains(&format!("remove {} and rerun", repo(&env))),
        "{failed}"
    );

    let env = TestEnv::new();
    env.ok(&["init"]);
    fs::remove_file(env.repo().join(".git/HEAD")).expect("remove HEAD");
    let failed = repo_line(&env);
    assert!(failed.contains(".git/HEAD is missing"), "{failed}");
    assert!(failed.contains("rerun dotman init"), "{failed}");

    let env = committed();
    fs::remove_file(env.repo().join(".git/dotman-id")).expect("remove id");
    let failed = repo_line(&env);
    assert!(failed.contains("holds commits, so move"), "{failed}");
    assert!(
        !failed.contains(&format!("remove {}", repo(&env))),
        "{failed}"
    );

    let env = committed();
    let lock = env.repo().join(".git/config.lock");
    fs::write(&lock, "").expect("write lock");
    let failed = repo_line(&env);
    assert!(failed.contains("config.lock is left"), "{failed}");
    assert!(
        failed.contains(&format!("remove {}", lock.display())),
        "{failed}"
    );
}

#[test]
fn a_reftable_repo_that_holds_commits_is_never_told_to_be_removed() {
    let env = committed();
    let repo = env.repo().display().to_string();
    let migrated = env.git_raw(&["-C", &repo, "refs", "migrate", "--ref-format=reftable"]);
    if !migrated.status.success() {
        eprintln!("skipped: this git cannot migrate refs to reftable");
        return;
    }
    assert!(
        !env.repo().join(".git/refs/heads").is_dir(),
        "the refs live in the reftable"
    );
    fs::remove_file(env.repo().join(".git/dotman-id")).expect("remove id");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    let failed = line(&out, "fail", "is incomplete").unwrap_or_default();
    assert!(failed.contains("holds commits, so move"), "{out}");
    assert!(!failed.contains(&format!("remove {repo}")), "{out}");
}

#[test]
fn a_readable_age_identity_warns() {
    let env = healthy();
    let identity = env.xdg_config().join("dotman/age-identity.txt");
    fs::create_dir_all(identity.parent().expect("parent")).expect("mkdir");
    fs::write(&identity, "AGE-SECRET-KEY-1QQQQ\n").expect("write identity");
    fs::set_permissions(&identity, fs::Permissions::from_mode(0o600)).expect("chmod");
    let (_, out) = doctor(&env, &[]);
    assert!(line(&out, "warn", "age identity").is_none(), "{out}");

    fs::set_permissions(&identity, fs::Permissions::from_mode(0o644)).expect("chmod");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
    let warned = line(&out, "warn", "age identity").unwrap_or_default();
    assert!(
        warned.contains("mode 0644") && warned.contains("chmod 600"),
        "{out}"
    );
}

#[test]
fn the_old_store_and_an_old_dot_on_path_warn() {
    let mut env = healthy();
    let bin = env.root().join("fakebin");
    fs::create_dir_all(&bin).expect("mkdir");
    env.prepend_path(&bin);
    let dot = |body: &str| env.write_script("fakebin/dot", &format!("#!/bin/sh\n{body}\n"));

    dot("echo dot 0.2.0");
    let (_, out) = doctor(&env, &[]);
    assert!(line(&out, "warn", "legacy store").is_none(), "{out}");
    assert!(line(&out, "warn", "dot:").is_none(), "{out}");

    fs::create_dir_all(env.home().join(".dotman")).expect("mkdir");
    fs::write(env.home().join(".dotman/index.bin"), b"").expect("write index");
    dot("echo dot 0.0.1");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
    let store = line(&out, "warn", "legacy store").unwrap_or_default();
    assert!(
        store.contains("dotman import ") && store.contains(".dotman`"),
        "{out}"
    );
    let old = line(&out, "warn", "dot:").unwrap_or_default();
    assert!(old.contains("cargo uninstall dotman"), "{out}");

    dot("exec sleep 30");
    let started = std::time::Instant::now();
    let (_, out) = doctor(&env, &[]);
    assert!(started.elapsed().as_secs() < 20, "the probe is bounded");
    assert!(line(&out, "warn", "dot:").is_none(), "{out}");

    // A child that outlives the probe keeps its stdout open.
    dot("sleep 30 &\necho dot 0.0.1");
    let started = std::time::Instant::now();
    let (_, out) = doctor(&env, &[]);
    assert!(
        started.elapsed().as_secs() < 20,
        "a forked child does not hold doctor up"
    );
    assert!(line(&out, "warn", "dot:").is_some(), "{out}");
}

#[test]
fn an_unapproved_or_changed_hook_table_warns() {
    let env = healthy();
    let (_, out) = doctor(&env, &[]);
    assert!(!out.contains("hooks"), "no table, no line: {out}");

    env.write_repo(".dotman.toml", b"[hooks]\n\"a\" = \"true\"\n");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(0), "{out}");
    let warned = line(&out, "warn", "hooks: not approved").unwrap_or_default();
    assert!(warned.contains("--approve-hooks"), "{out}");

    env.ok(&["apply", "--approve-hooks"]);
    let (_, out) = doctor(&env, &[]);
    assert!(line(&out, "pass", "hooks approved").is_some(), "{out}");

    env.write_repo(".dotman.toml", b"[hooks]\n\"a\" = \"false\"\n");
    let (_, out) = doctor(&env, &[]);
    assert!(
        line(&out, "warn", "hooks: the table changed").is_some(),
        "{out}"
    );

    let file = env.state_dir().join("state.toml");
    let mut state = State::read(&file).expect("state").expect("a state");
    state.hooks.approved = Some("55c7a64d8992c4767b4d28afab1811bd".to_owned());
    state.save(&file, false).expect("save state");
    let (_, out) = doctor(&env, &[]);
    let warned = line(&out, "warn", "hooks: the approval format changed").unwrap_or_default();
    assert!(warned.contains("--approve-hooks"), "{out}");

    env.write_repo(".dotman.toml", b"[hooks]\n\"a\" = \"x\\ry\"\n");
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(
        line(&out, "fail", "holds U+000D carriage return").is_some(),
        "{out}"
    );
}

#[test]
fn backups_are_counted_and_plaintext_copies_are_named() {
    let env = committed();
    env.write(".bashrc", b"local\n", 0o644);
    env.write_repo(".bashrc", b"repo\n");
    env.ok(&["apply", "--take-repo"]);
    let (_, out) = doctor(&env, &[]);
    assert!(line(&out, "info", " from 1 run, in ").is_some(), "{out}");
    assert!(!out.contains("plaintext"), "{out}");

    let run = env.state_dir().join("backups/20200101T000000Z-1/.dotman");
    fs::create_dir_all(&run).expect("mkdir");
    fs::write(run.join("age-identity.txt"), "AGE-SECRET-KEY-1QQQQ\n").expect("write");
    let (_, out) = doctor(&env, &[]);
    assert!(
        line(&out, "info", "files from 2 runs, in ").is_some(),
        "{out}"
    );
    let plain = line(&out, "info", "plaintext").unwrap_or_default();
    assert!(plain.starts_with("info  backups: 1 of them"), "{out}");
}

#[test]
fn a_missing_repo_fails_but_other_checks_still_run() {
    let env = TestEnv::new();
    let (code, out) = doctor(&env, &[]);
    assert_eq!(code, Some(1), "{out}");
    assert!(line(&out, "fail", "repo: not found").is_some(), "{out}");
    assert!(line(&out, "pass", "git identity").is_some(), "{out}");
}
