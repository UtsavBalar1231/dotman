//! Post-apply hooks through the binary. Every hook appends to a marker file under the temp root,
//! outside HOME and the repo, so a run that must not fire a hook leaves the marker absent.

mod common;

use common::TestEnv;
use dotman::state::State;
use std::fs;
use std::path::PathBuf;

const A: &str = ".config/app/a.conf";
const B: &str = ".config/app/b.conf";

fn marker(env: &TestEnv) -> PathBuf {
    env.root().join("marker")
}

fn runs(env: &TestEnv) -> usize {
    fs::read_to_string(marker(env)).map_or(0, |text| text.lines().count())
}

fn append(env: &TestEnv) -> String {
    format!("echo ran >> '{}'", marker(env).display())
}

fn set_hooks(env: &TestEnv, command: &str) {
    env.write_repo(
        ".dotman.toml",
        format!("[hooks]\n\".config/app/**\" = \"{command}\"\n").as_bytes(),
    );
}

/// An initialised repo whose worktree holds A and B, with no hooks yet.
fn hooked() -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(A, b"a1\n");
    env.write_repo(B, b"b1\n");
    env
}

fn exits(env: &TestEnv, code: i32, args: &[&str]) -> String {
    let out = env.run(args);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(code), "dot {args:?}: {err}");
    err
}

#[test]
fn approving_runs_a_command_once_for_two_changed_files() {
    let env = hooked();
    set_hooks(&env, &append(&env));
    let err = exits(&env, 0, &["apply", "--approve-hooks"]);
    assert_eq!(env.read(A), b"a1\n");
    assert_eq!(env.read(B), b"b1\n");
    assert_eq!(runs(&env), 1, "{err}");
    assert!(err.contains("hook"), "{err}");
}

#[test]
fn the_hook_runs_in_home() {
    let env = hooked();
    set_hooks(&env, &format!("pwd >> '{}'", marker(&env).display()));
    exits(&env, 0, &["apply", "--approve-hooks"]);
    let home = fs::canonicalize(env.home()).expect("canonical home");
    let seen = fs::read_to_string(marker(&env)).expect("marker");
    assert_eq!(seen.trim(), home.to_string_lossy());
}

#[test]
fn an_approved_table_runs_later_without_the_flag() {
    let env = hooked();
    set_hooks(&env, &append(&env));
    exits(&env, 0, &["apply", "--approve-hooks"]);
    env.write_repo(A, b"a2\n");
    exits(&env, 0, &["apply"]);
    assert_eq!(runs(&env), 2);
    exits(&env, 0, &["apply"]);
    assert_eq!(runs(&env), 2, "nothing changed, so nothing ran");
}

#[test]
fn an_edited_table_needs_approval_again() {
    let env = hooked();
    set_hooks(&env, &append(&env));
    exits(&env, 0, &["apply", "--approve-hooks"]);
    assert_eq!(runs(&env), 1);

    set_hooks(&env, &format!("{}; true", append(&env)));
    env.write_repo(A, b"a2\n");
    let err = exits(&env, 0, &["apply"]);
    assert_eq!(env.read(A), b"a2\n", "the file is still applied");
    assert_eq!(runs(&env), 1, "{err}");
    assert!(err.contains("not approved"), "{err}");
    assert!(err.contains("--approve-hooks"), "{err}");

    env.write_repo(A, b"a3\n");
    exits(&env, 0, &["apply", "--approve-hooks"]);
    assert_eq!(runs(&env), 2);
    env.write_repo(A, b"a4\n");
    exits(&env, 0, &["apply"]);
    assert_eq!(runs(&env), 3, "the approval was recorded");
}

#[test]
fn spacing_and_order_in_the_table_do_not_need_a_new_approval() {
    let env = hooked();
    let command = append(&env);
    env.write_repo(
        ".dotman.toml",
        format!("[hooks]\n\"zzz\" = \"true\"\n\".config/app/**\" = \"{command}\"\n").as_bytes(),
    );
    exits(&env, 0, &["apply", "--approve-hooks"]);
    env.write_repo(
        ".dotman.toml",
        format!("[hooks]\n\".config/app/**\"   =   \"{command}\"\n\n\"zzz\"=\"true\"\n").as_bytes(),
    );
    env.write_repo(A, b"a2\n");
    exits(&env, 0, &["apply"]);
    assert_eq!(runs(&env), 2);
}

#[test]
fn an_unapproved_table_never_runs_in_a_non_interactive_run() {
    let env = hooked();
    set_hooks(&env, &append(&env));
    let err = exits(&env, 0, &["apply"]);
    assert_eq!(env.read(A), b"a1\n");
    assert!(!marker(&env).exists(), "{err}");
    assert!(err.contains("not approved"), "{err}");
}

#[test]
fn no_hooks_runs_nothing_and_records_nothing() {
    let env = hooked();
    set_hooks(&env, &append(&env));
    exits(&env, 0, &["apply", "--no-hooks"]);
    assert!(!marker(&env).exists());
    assert_eq!(env.read(A), b"a1\n");
    let state = fs::read_to_string(env.state_dir().join("state.toml")).expect("state");
    assert!(!state.contains("approved"), "{state}");
}

#[test]
fn a_dry_run_runs_nothing_even_with_the_flag() {
    let env = hooked();
    set_hooks(&env, &append(&env));
    let before = env.four_trees();
    let err = exits(&env, 0, &["apply", "--dry-run", "--approve-hooks"]);
    assert!(!marker(&env).exists());
    assert_eq!(env.four_trees(), before);
    assert!(err.contains("would run hook"), "{err}");
}

#[test]
fn a_dry_run_says_whether_the_hook_would_run() {
    let env = hooked();
    set_hooks(&env, &append(&env));
    let err = exits(&env, 0, &["apply", "--dry-run"]);
    assert!(
        err.contains("hook not approved on this machine, would be skipped:"),
        "{err}"
    );
    assert!(!err.contains("would run hook"), "{err}");

    exits(&env, 0, &["apply", "--approve-hooks"]);
    env.write_repo(A, b"a2\n");
    let err = exits(&env, 0, &["apply", "--dry-run"]);
    assert!(err.contains("would run hook:"), "{err}");

    set_hooks(&env, &format!("{}; true", append(&env)));
    let err = exits(&env, 0, &["apply", "--dry-run", "--approve-hooks"]);
    assert!(err.contains("would run hook:"), "{err}");
    assert_eq!(runs(&env), 1);
}

#[test]
fn approve_and_no_hooks_conflict() {
    let env = hooked();
    for command in ["apply", "sync"] {
        let err = exits(&env, 2, &[command, "--no-hooks", "--approve-hooks"]);
        assert!(err.contains("cannot be used with"), "{err}");
    }
}

#[test]
fn a_failing_hook_exits_1_after_every_write() {
    let env = hooked();
    set_hooks(&env, &format!("{}; exit 1", append(&env)));
    let err = exits(&env, 1, &["apply", "--approve-hooks"]);
    assert_eq!(env.read(A), b"a1\n");
    assert_eq!(env.read(B), b"b1\n");
    assert_eq!(runs(&env), 1);
    assert!(err.contains("failed"), "{err}");
}

#[test]
fn every_hook_runs_after_one_fails() {
    let env = hooked();
    let mark = marker(&env).display().to_string();
    env.write_repo(
        ".dotman.toml",
        format!(
            "[hooks]\n\".config/app/a*\" = \"exit 1\"\n\".config/app/b*\" = \"echo ran >> '{mark}'\"\n"
        )
        .as_bytes(),
    );
    exits(&env, 1, &["apply", "--approve-hooks"]);
    assert_eq!(runs(&env), 1);
}

#[test]
fn an_invalid_glob_is_an_error_before_any_hook_runs() {
    let env = hooked();
    let mark = marker(&env).display().to_string();
    env.write_repo(
        ".dotman.toml",
        format!("[hooks]\n\"a[\" = \"true\"\n\".config/app/**\" = \"echo ran >> '{mark}'\"\n")
            .as_bytes(),
    );
    let err = exits(&env, 1, &["apply", "--approve-hooks"]);
    assert!(err.contains("`a[`"), "{err}");
    assert!(!marker(&env).exists());
}

#[test]
fn a_path_that_fails_does_not_stop_the_hook_for_the_ones_written() {
    let env = hooked();
    set_hooks(&env, &append(&env));
    env.write_repo(".config/app/c{tmpl}.conf", b"{{ vars.missing }}\n");
    let err = exits(&env, 1, &["apply", "--approve-hooks"]);
    assert!(err.contains("1 path could not be applied"), "{err}");
    assert_eq!(env.read(A), b"a1\n");
    assert_eq!(runs(&env), 1);
}

#[test]
fn a_hook_matches_only_paths_this_run_changed() {
    let env = hooked();
    set_hooks(&env, &append(&env));
    exits(&env, 0, &["apply", "--approve-hooks"]);
    env.write_repo(".other", b"x\n");
    exits(&env, 0, &["apply"]);
    assert_eq!(runs(&env), 1);
}

#[test]
fn edit_runs_the_hook_for_its_path() {
    let env = hooked();
    set_hooks(&env, &append(&env));
    exits(&env, 0, &["apply", "--approve-hooks"]);
    let editor = env.write_script(
        "editor.sh",
        "#!/bin/sh\nfor f; do :; done\necho more >> \"$f\"\n",
    );
    let editor = editor.to_string_lossy().into_owned();
    let out = env.run_with(&[("EDITOR", &editor)], &["edit", A]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(runs(&env), 2);
}

fn first_device() -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".bashrc", b"base\n", 0o644);
    env.ok(&["add", ".bashrc"]);
    env.ok(&["sync"]);
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

#[test]
fn init_from_a_url_runs_hooks_only_with_the_flag() {
    let source = hooked();
    set_hooks(&source, &append(&source));
    source.push_repo();

    let without = source.second_device();
    let err = exits(&without, 0, &["init", &source.remote_url()]);
    assert_eq!(without.read(A), b"a1\n");
    assert!(!marker(&source).exists(), "{err}");
    assert!(err.contains("--approve-hooks"), "{err}");

    let with = source.second_device();
    exits(&with, 0, &["init", &source.remote_url(), "--approve-hooks"]);
    assert_eq!(runs(&source), 1);
}

#[test]
fn sync_runs_a_hook_once_and_still_pushes_when_it_fails() {
    let a = first_device();
    let b = a.second_device();
    b.ok(&["init", &b.remote_url(), "--take-repo"]);

    set_hooks(&a, &format!("{}; exit 1", append(&a)));
    a.write_repo(A, b"a1\n");
    a.write_repo(B, b"b1\n");
    exits(&a, 0, &["sync", "--no-hooks"]);
    assert!(!marker(&a).exists());

    b.write(".bashrc", b"edited on b\n", 0o644);
    let before = b.remote_refs();
    let err = exits(&b, 1, &["sync", "--approve-hooks"]);
    assert_eq!(b.read(A), b"a1\n");
    assert_eq!(b.read(B), b"b1\n");
    assert_eq!(runs(&a), 1, "{err}");
    assert_ne!(b.remote_refs(), before, "the push still happened: {err}");
}

#[test]
fn an_unattended_sync_runs_only_hooks_approved_on_this_machine() {
    let a = first_device();
    let b = a.second_device();
    b.ok(&["init", &b.remote_url(), "--take-repo"]);

    set_hooks(&a, &append(&a));
    a.write_repo(A, b"a1\n");
    exits(&a, 0, &["sync", "--no-hooks"]);
    let err = exits(&b, 0, &["sync"]);
    assert_eq!(b.read(A), b"a1\n");
    assert!(!marker(&a).exists(), "{err}");
    assert!(err.contains("not approved"), "{err}");

    exits(&b, 0, &["apply", "--approve-hooks"]);
    a.write_repo(A, b"a2\n");
    exits(&a, 0, &["sync", "--no-hooks"]);
    exits(&b, 0, &["sync"]);
    assert_eq!(runs(&a), 1);
}

#[test]
fn sync_without_a_remote_applies_then_runs_the_hook() {
    let env = hooked();
    set_hooks(&env, &append(&env));
    exits(&env, 0, &["sync", "--approve-hooks"]);
    assert_eq!(env.read(A), b"a1\n");
    assert_eq!(runs(&env), 1);
}

#[test]
fn a_blocked_target_keeps_exit_3_and_the_hook_still_runs_for_the_written_path() {
    let env = hooked();
    set_hooks(&env, &append(&env));
    fs::create_dir_all(env.home().join(B)).expect("directory in the way");
    fs::write(env.home().join(B).join("x"), "x").expect("file in it");
    exits(&env, 3, &["apply", "--approve-hooks"]);
    assert_eq!(runs(&env), 1);
}

fn approved_line(env: &TestEnv) -> Option<String> {
    fs::read_to_string(env.state_dir().join("state.toml"))
        .ok()?
        .lines()
        .find(|line| line.starts_with("approved"))
        .map(str::to_owned)
}

#[test]
fn a_nul_in_a_command_is_refused_and_nothing_is_approved() {
    let env = hooked();
    let mark = marker(&env).display().to_string();
    env.write_repo(
        ".dotman.toml",
        format!("[hooks]\n\".config/app/**\" = \"true\\u0000z*\\u0000echo ran >> '{mark}'\"\n")
            .as_bytes(),
    );
    let err = exits(&env, 1, &["apply", "--approve-hooks"]);
    assert!(err.contains("control character"), "{err}");
    assert!(
        err.contains("`.config/app/**` = `true\\0z*\\0echo ran"),
        "{err}"
    );
    assert!(!marker(&env).exists());
    assert_eq!(approved_line(&env), None, "{err}");
}

#[test]
fn a_table_swapped_after_approval_never_runs_unattended() {
    let env = hooked();
    let run = append(&env);
    // One entry whose command embeds NULs: joined with NULs, it is the two entries below.
    let one = format!("[hooks]\n\".config/app/**\" = \"true\\u0000z*\\u0000{run}\"\n");
    env.write_repo(".dotman.toml", one.as_bytes());
    exits(&env, 1, &["apply", "--approve-hooks"]);
    assert_eq!(approved_line(&env), None);

    // What approving the first table recorded before: the xxh3 of the NUL-joined entries.
    let file = env.state_dir().join("state.toml");
    let mut state = State::read(&file).expect("state").expect("a state");
    state.hooks.approved = Some(common::xxh3(
        format!(".config/app/**\0true\0z*\0{run}\0").as_bytes(),
    ));
    state.save(&file, false).expect("save state");

    let two = format!("[hooks]\n\".config/app/**\" = \"true\"\n\"z*\" = \"{run}\"\n");
    env.write_repo(".dotman.toml", two.as_bytes());
    env.write_repo(A, b"a2\n");
    env.write_repo("zfile", b"z\n");
    let err = exits(&env, 0, &["apply"]);
    assert!(err.contains("not approved"), "{err}");
    assert!(!marker(&env).exists(), "{err}");
    exits(&env, 0, &["apply", "--approve-hooks"]);
    assert!(!marker(&env).exists(), "nothing changed, so nothing is due");
}

#[test]
fn an_escape_sequence_in_a_hook_is_shown_escaped_even_with_quiet() {
    let env = hooked();
    let mark = marker(&env).display().to_string();
    env.write_repo(
        ".dotman.toml",
        format!(
            "[hooks]\n\"\\u001b]0;t\\u0007\" = \"echo ran >> '{mark}' #\\r\\u001b[2K  .config/app/** -> echo reloaded\"\n"
        )
        .as_bytes(),
    );
    for args in [
        &["apply", "--approve-hooks"][..],
        &["-q", "apply", "--approve-hooks"],
    ] {
        let out = env.run(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(
            !out.stderr.contains(&0x1b) && !out.stderr.contains(&b'\r'),
            "{args:?}"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("`\\u{1b}]0;t\\u{7}` = `echo ran"),
            "{args:?}: {err}"
        );
        assert!(
            err.contains("#\\r\\u{1b}[2K  .config/app/**"),
            "{args:?}: {err}"
        );
    }
    assert!(!marker(&env).exists());
}

#[test]
fn saving_an_approval_prints_the_table_even_with_quiet() {
    let env = hooked();
    set_hooks(&env, &append(&env));
    let err = exits(&env, 0, &["-q", "apply", "--approve-hooks"]);
    assert!(err.contains("approved this [hooks] table"), "{err}");
    assert!(
        err.contains(&format!(".config/app/** -> {}", append(&env))),
        "{err}"
    );
    env.write_repo(A, b"a2\n");
    let err = exits(&env, 0, &["-q", "apply"]);
    assert!(
        !err.contains("approved this"),
        "an approval already saved is not shown: {err}"
    );
    assert_eq!(runs(&env), 2);
}

#[test]
fn the_approval_prompt_shows_the_table_even_with_quiet() {
    for quiet in [false, true] {
        let env = hooked();
        set_hooks(&env, &append(&env));
        let args: &[&str] = if quiet { &["-q", "apply"] } else { &["apply"] };
        // Colour off, so any escape byte in what the terminal shows came from the repo.
        let plain = [("NO_COLOR", "1"), ("TERM", "dumb")];
        let Some(seen) = common::in_a_terminal(&env, &plain, args, b"y\n") else {
            eprintln!("skipped: no script(1) to give dotman a terminal");
            return;
        };
        assert!(seen.contains("1 hook in .dotman.toml:"), "{seen}");
        assert!(
            seen.contains(&format!(".config/app/** -> {}", append(&env))),
            "{seen}"
        );
        assert!(
            seen.contains("Approval covers these command lines"),
            "{seen}"
        );
        assert!(!seen.contains('\u{1b}'), "{seen:?}");
        assert_eq!(runs(&env), 1, "{seen}");
        assert!(approved_line(&env).is_some(), "{seen}");
    }
}
