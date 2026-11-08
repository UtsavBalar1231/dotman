//! Every subcommand listed in `dotman --help` needs a row in `MINIMAL_ARGS`; add one with the command.

mod common;

use common::TestEnv;
use std::fs;

const NOT_COVERED: &[&str] = &["git", "doctor", "completion", "system-write", "help"];

const MINIMAL_ARGS: &[(&str, &[&str])] = &[
    ("init", &[]),
    ("add", &[".bashrc"]),
    ("forget", &[".bashrc"]),
    ("list", &[]),
    ("status", &[]),
    ("apply", &[]),
    ("diff", &[]),
    ("edit", &[".bashrc"]),
    ("sync", &[]),
    ("log", &[]),
    ("restore", &[".bashrc"]),
    ("key", &["init"]),
    ("import", &[]),
];

fn listed_subcommands(help: &str) -> Vec<String> {
    help.lines()
        .skip_while(|line| line.trim_end() != "Commands:")
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| !NOT_COVERED.contains(name))
        .map(str::to_owned)
        .collect()
}

fn subcommands() -> Vec<String> {
    let out = TestEnv::new().run(&["--help"]);
    assert!(out.status.success(), "dot --help failed");
    let listed = listed_subcommands(&String::from_utf8_lossy(&out.stdout));
    assert!(!listed.is_empty(), "found no subcommands in `dot --help`");
    listed
}

#[test]
fn every_subcommand_has_minimal_args() {
    let missing: Vec<String> = subcommands()
        .into_iter()
        .filter(|name| !MINIMAL_ARGS.iter().any(|(known, _)| known == name))
        .collect();
    assert!(
        missing.is_empty(),
        "add a row to MINIMAL_ARGS in tests/guard.rs for: {missing:?}"
    );
}

#[test]
fn minimal_args_are_accepted_by_the_parser() {
    for (name, args) in MINIMAL_ARGS {
        let out = TestEnv::new()
            .dot()
            .arg(name)
            .args(*args)
            .output()
            .expect("run dot");
        assert!(
            out.status
                .code()
                .is_some_and(|code| code != 2 && code != 101),
            "`dot {name} {args:?}` was rejected by clap or panicked: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn stderr_of(env: &TestEnv, args: &[&str]) -> (Option<i32>, String) {
    let out = env.run(args);
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn every_subcommand_exits_4_while_a_rebase_is_in_progress() {
    for (name, args) in MINIMAL_ARGS {
        let env = TestEnv::new();
        env.ok(&["init"]);
        fs::create_dir(env.repo().join(".git/rebase-merge")).expect("mark the rebase");
        let mut full = vec![*name];
        full.extend_from_slice(args);
        let (code, err) = stderr_of(&env, &full);
        assert_eq!(code, Some(4), "`dot {full:?}`: {err}");
        assert!(err.contains("a rebase is in progress"), "{name}: {err}");
        assert!(
            err.contains("dotman git rebase --continue"),
            "{name}: {err}"
        );
        assert!(err.contains("dotman git rebase --abort"), "{name}: {err}");
    }
}

#[test]
fn the_other_markers_are_refused_and_named() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let marker = env.repo().join(".git/rebase-apply");
    fs::create_dir(&marker).expect("mark the rebase");
    assert_eq!(stderr_of(&env, &["status"]).0, Some(4));
    fs::remove_dir(&marker).expect("clear the rebase");

    fs::write(env.repo().join(".git/MERGE_HEAD"), "x").expect("mark the merge");
    let (code, err) = stderr_of(&env, &["status"]);
    assert_eq!(code, Some(4), "{err}");
    assert!(err.contains("a merge is in progress"), "{err}");
    assert!(err.contains("dotman git merge --continue"), "{err}");
    assert!(err.contains("dotman git merge --abort"), "{err}");
}
