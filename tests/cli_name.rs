//! The binary is `dotman`; hints, usage and completions use the name it was started as, and
//! `dot` is only an alias.

mod common;

use common::{R, TestEnv, text};
use dotman::VERSION;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;

const BIN: &str = env!("CARGO_BIN_EXE_dotman");

/// `name` in `dir`, as a link to the built binary.
fn linked(dir: &Path, name: &str) -> R<PathBuf> {
    fs::create_dir_all(dir)?;
    let path = dir.join(name);
    symlink(BIN, &path)?;
    Ok(path)
}

fn run(env: &TestEnv, exe: &Path, args: &[&str]) -> io::Result<Output> {
    env.process_for(exe).args(args).output()
}

/// Stderr of a command that needs a repo, where there is none: it names `init` in a hint.
fn hint(env: &TestEnv, exe: &Path) -> R<String> {
    let out = run(env, exe, &["status"])?;
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    Ok(String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn version_is_dotman_under_either_name() -> R {
    let env = TestEnv::new();
    let tmp = tempfile::tempdir()?;
    for exe in [PathBuf::from(BIN), linked(tmp.path(), "dot")?] {
        let out = run(&env, &exe, &["--version"])?;
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("dotman {VERSION}\n"),
            "{}",
            exe.display()
        );
    }
    Ok(())
}

#[test]
fn hints_and_usage_use_the_name_the_binary_was_started_as() -> R {
    let env = TestEnv::new();
    let tmp = tempfile::tempdir()?;
    let copied = tmp.path().join("copy/dot");
    fs::create_dir_all(copied.parent().ok_or("no parent")?)?;
    fs::copy(BIN, &copied)?;
    let cases = [
        (PathBuf::from(BIN), "dotman"),
        (linked(tmp.path(), "dotman")?, "dotman"),
        (linked(tmp.path(), "dot")?, "dot"),
        (copied, "dot"),
        (linked(tmp.path(), "dot2")?, "dotman"),
        (linked(tmp.path(), "mydot")?, "dotman"),
    ];
    for (exe, name) in cases {
        let err = hint(&env, &exe)?;
        assert!(
            err.contains(&format!("run `{name} init` first")),
            "{exe:?}: {err}"
        );
        let help = run(&env, &exe, &["--help"])?;
        let help = String::from_utf8_lossy(&help.stdout);
        assert!(help.contains(&format!("Usage: {name} ")), "{exe:?}: {help}");
        let sub = run(&env, &exe, &["add", "--help"])?;
        let sub = String::from_utf8_lossy(&sub.stdout);
        assert!(
            sub.contains(&format!("Usage: {name} add ")),
            "{exe:?}: {sub}"
        );
    }
    Ok(())
}

#[test]
fn argv0_alone_decides_the_name() -> R {
    let env = TestEnv::new();
    for (argv0, name) in [
        ("dot", "dot"),
        ("/some/where/dot", "dot"),
        ("dotman", "dotman"),
    ] {
        let out = env
            .process_for(Path::new(BIN))
            .arg0(argv0)
            .arg("status")
            .output()?;
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(&format!("run `{name} init` first")),
            "{argv0}: {err}"
        );
    }
    Ok(())
}

#[test]
fn completions_are_generated_for_the_name_used() -> R {
    let env = TestEnv::new();
    let tmp = tempfile::tempdir()?;
    let out = run(&env, Path::new(BIN), &["completion", "bash"])?;
    assert!(String::from_utf8_lossy(&out.stdout).contains("_dotman()"));
    let out = run(&env, &linked(tmp.path(), "dot")?, &["completion", "bash"])?;
    assert!(String::from_utf8_lossy(&out.stdout).contains("_dot()"));
    Ok(())
}
