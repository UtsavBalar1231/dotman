//! Every subcommand listed in `dot --help` needs a row in `MINIMAL_ARGS`; add one with the command.

mod common;

use common::TestEnv;

const NOT_COVERED: &[&str] = &["git", "doctor", "completion", "system-write", "help"];

const MINIMAL_ARGS: &[(&str, &[&str])] = &[
    ("init", &[]),
    ("add", &[".bashrc"]),
    ("forget", &[".bashrc"]),
    ("list", &[]),
    ("status", &[]),
    ("apply", &[]),
    ("diff", &[]),
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
