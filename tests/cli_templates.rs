//! Templates and `dotman edit`, through the binary. The identity is never relied on: templates read
//! only `[vars]` and the allowlisted environment.

mod common;

use common::TestEnv;
use std::fs;
use std::path::PathBuf;
use std::process::Output;

const SOURCE: &str = ".config/kitty/kitty{tmpl}.conf";
const KEY: &str = ".config/kitty/kitty.conf";

fn exits(out: &Output, code: i32) -> String {
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(code), "{err}");
    err
}

fn kitty(template: &str) -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_machine_config("[vars]\nfont = \"14\"\n");
    env.write_repo(SOURCE, template.as_bytes());
    env
}

fn token(env: &TestEnv, key: &str) -> Option<String> {
    env.token_of("status", key)
}

/// A script that appends a line to the file it is given as its last argument.
fn stub_editor(env: &TestEnv, exit: i32) -> PathBuf {
    let script = format!("#!/bin/sh\nfor f; do :; done\necho '# edited' >> \"$f\"\nexit {exit}\n");
    env.write_script("stub-editor.sh", &script)
}

#[test]
fn a_template_renders_with_the_machine_vars() {
    let env = kitty("font_size {{ vars.font }}\n");
    exits(&env.run(&["apply"]), 0);
    assert_eq!(env.read(KEY), b"font_size 14\n");
    assert_eq!(token(&env, KEY).as_deref(), Some("clean"));
    assert!(!env.home().join(".config/kitty/kitty{tmpl}.conf").exists());
}

#[test]
fn an_undefined_variable_fails_naming_the_file_and_the_variable() {
    let env = kitty("font_size {{ vars.size }}\n");
    env.write_repo(".plain", b"x\n");
    let err = exits(&env.run(&["apply"]), 1);
    assert!(err.contains("kitty{tmpl}.conf"), "{err}");
    assert!(err.contains("vars.size"), "{err}");
    assert!(
        err.contains(
            ".config/kitty/kitty.conf: skipped: template-error: .config/kitty/kitty{tmpl}.conf:1: "
        ),
        "{err}"
    );
    assert!(err.contains("1 path could not be applied"), "{err}");
    assert!(!env.home().join(KEY).exists());
    assert_eq!(env.read(".plain"), b"x\n", "other paths still apply");
    assert_eq!(token(&env, KEY).as_deref(), Some("template-error"));
    let out = env.run(&["status"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(
            ".config/kitty/kitty.conf: template-error: .config/kitty/kitty{tmpl}.conf:1: "
        ) && err.contains("vars.size"),
        "{err}"
    );
    let shown = String::from_utf8_lossy(&out.stdout);
    assert!(
        shown.contains("template-error            .config/kitty/kitty.conf"),
        "{shown}"
    );
}

#[test]
fn only_allowlisted_environment_variables_render() {
    let env = kitty("token {{ env.SECRET }}\n");
    let secret = [("SECRET", "hunter2")];
    let err = exits(&env.run_with(&secret, &["apply"]), 1);
    assert!(err.contains("env.SECRET"), "{err}");
    assert!(!env.home().join(KEY).exists());

    env.write_machine_config("[template]\nenv = [\"SECRET\"]\n");
    exits(&env.run_with(&secret, &["apply"]), 0);
    assert_eq!(env.read(KEY), b"token hunter2\n");

    let err = exits(&env.run(&["apply"]), 1);
    assert!(
        err.contains("env.SECRET"),
        "an allowlisted name that is unset is undefined: {err}"
    );
}

/// Both commands of the drift hint run as printed on a path with a space, and do what they say.
#[test]
fn the_printed_drift_commands_run_as_printed_on_a_path_with_a_space() {
    let source = ".config/my kitty/kitty{tmpl}.conf";
    let key = ".config/my kitty/kitty.conf";
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_machine_config("[vars]\nfont = \"14\"\n");
    env.write_repo(source, b"font_size {{ vars.font }}\n");
    exits(&env.run(&["apply"]), 0);

    env.write(key, b"font_size 99\n", 0o644);
    let err = exits(&env.run(&["apply"]), 3);
    let out = common::run_hint(&env, &err, "apply --take-repo");
    exits(&out, 0);
    assert_eq!(env.read(key), b"font_size 14\n");

    env.write(key, b"font_size 99\n", 0o644);
    let err = exits(&env.run(&["apply"]), 3);
    let editor = stub_editor(&env, 0).to_string_lossy().into_owned();
    let args = common::hint(&err, "edit");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    exits(&env.run_with(&[("EDITOR", &editor)], &args), 0);
    assert_eq!(
        fs::read(env.repo().join(source)).unwrap(),
        b"font_size {{ vars.font }}\n# edited\n"
    );
    assert_eq!(env.read(key), b"font_size 14\n# edited\n");
}

#[test]
fn a_local_edit_is_template_drift_and_is_never_captured() {
    let env = kitty("font_size {{ vars.font }}\n");
    exits(&env.run(&["apply"]), 0);
    env.write(KEY, b"font_size 99\n", 0o644);

    assert_eq!(token(&env, KEY).as_deref(), Some("template-drift"));
    let err = exits(&env.run(&["apply"]), 3);
    let path = env.home().join(KEY).display().to_string();
    assert!(
        err.contains(&format!(
            "template-drift; edit the template with `dotman edit {path}`, or discard the local change with `dotman apply --take-repo {path}`"
        )),
        "{err}"
    );
    exits(&env.run(&["sync"]), 3);
    assert_eq!(
        fs::read(env.repo().join(SOURCE)).unwrap(),
        b"font_size {{ vars.font }}\n"
    );
    assert_eq!(env.read(KEY), b"font_size 99\n");
    assert!(!env.repo().join(KEY).exists());

    // Both changed is drift too.
    env.write_machine_config("[vars]\nfont = \"16\"\n");
    assert_eq!(token(&env, KEY).as_deref(), Some("template-drift"));
}

#[test]
fn a_changed_variable_is_written_like_any_repo_change() {
    let env = kitty("font_size {{ vars.font }}\n");
    exits(&env.run(&["apply"]), 0);
    env.write_machine_config("[vars]\nfont = \"16\"\n");
    assert_eq!(token(&env, KEY).as_deref(), Some("changed-in-repo"));
    exits(&env.run(&["apply"]), 0);
    assert_eq!(env.read(KEY), b"font_size 16\n");
}

#[test]
fn edit_opens_the_source_then_applies_the_new_rendering() {
    let env = kitty("font_size {{ vars.font }}\n");
    exits(&env.run(&["apply"]), 0);
    let editor = stub_editor(&env, 0);
    // The command is split like a shell would, with the file appended last.
    let command = format!("{} --wait", editor.display());
    exits(&env.run_with(&[("EDITOR", &command)], &["edit", KEY]), 0);
    assert_eq!(
        fs::read(env.repo().join(SOURCE)).unwrap(),
        b"font_size {{ vars.font }}\n# edited\n"
    );
    assert_eq!(env.read(KEY), b"font_size 14\n# edited\n");
    assert_eq!(token(&env, KEY).as_deref(), Some("clean"));
}

#[test]
fn visual_wins_over_editor_and_a_failing_editor_applies_nothing() {
    let env = kitty("font_size {{ vars.font }}\n");
    exits(&env.run(&["apply"]), 0);
    let failing = stub_editor(&env, 1);
    let err = exits(
        &env.run_with(
            &[("VISUAL", &failing.to_string_lossy()), ("EDITOR", "true")],
            &["edit", KEY],
        ),
        1,
    );
    assert!(err.contains("nothing was applied"), "{err}");
    assert_eq!(env.read(KEY), b"font_size 14\n");
}

#[test]
fn edit_refuses_unmanaged_paths_without_opening_the_editor() {
    let env = kitty("x\n");
    let editor = stub_editor(&env, 0);
    let vars = [("EDITOR", editor.to_str().unwrap())];
    let err = exits(&env.run_with(&vars, &["edit", ".nothing"]), 1);
    assert!(err.contains("not managed"), "{err}");
    assert_eq!(fs::read(env.repo().join(SOURCE)).unwrap(), b"x\n");
}

#[test]
fn restore_brings_back_the_template_and_renders_it() {
    let env = kitty("font_size {{ vars.font }}\n");
    exits(&env.run(&["apply"]), 0);
    exits(&env.run(&["sync"]), 0);
    env.write_repo(SOURCE, b"font_size {{ vars.font }}\nbold yes\n");
    exits(&env.run(&["sync"]), 0);
    exits(&env.run(&["apply"]), 0);
    assert_eq!(env.read(KEY), b"font_size 14\nbold yes\n");
    let state = fs::read(env.state_dir().join("state.toml")).unwrap();

    let before = env.four_trees();
    exits(
        &env.run(&["restore", KEY, "--from", "HEAD~1", "--dry-run"]),
        0,
    );
    assert_eq!(env.four_trees(), before);

    exits(&env.run(&["restore", KEY, "--from", "HEAD~1"]), 0);
    assert_eq!(
        fs::read(env.repo().join(SOURCE)).unwrap(),
        b"font_size {{ vars.font }}\n"
    );
    assert_eq!(env.read(KEY), b"font_size 14\n");
    // The sync that applied the edited template backed up the old rendering; restore the new one.
    let mut backups = env.backups(KEY);
    backups.sort();
    assert_eq!(
        backups,
        [
            (b"font_size 14\n".to_vec(), 0o600),
            (b"font_size 14\nbold yes\n".to_vec(), 0o600)
        ]
    );
    assert_eq!(
        fs::read(env.state_dir().join("state.toml")).unwrap(),
        state,
        "S is left alone"
    );
}

#[test]
fn add_never_overwrites_a_template_with_its_rendering() {
    let env = kitty("font_size {{ vars.font }}\n");
    exits(&env.run(&["apply"]), 0);
    let err = exits(&env.run(&["add", KEY]), 1);
    assert!(err.contains("dotman edit"), "{err}");
    assert_eq!(
        fs::read(env.repo().join(SOURCE)).unwrap(),
        b"font_size {{ vars.font }}\n"
    );
}

/// The `edit` that the refusal of `add` prints runs as printed on a path with a space.
#[test]
fn the_edit_printed_when_add_meets_a_template_runs_as_printed_on_a_path_with_a_space() {
    let source = ".config/my kitty/kitty{tmpl}.conf";
    let key = ".config/my kitty/kitty.conf";
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_machine_config("[vars]\nfont = \"14\"\n");
    env.write_repo(source, b"font_size {{ vars.font }}\n");
    exits(&env.run(&["apply"]), 0);

    let err = exits(&env.run(&["add", key]), 1);
    let editor = stub_editor(&env, 0).to_string_lossy().into_owned();
    let args = common::hint(&err, "edit");
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    exits(&env.run_with(&[("EDITOR", &editor)], &args), 0);
    assert_eq!(env.read(key), b"font_size 14\n# edited\n");
}

/// A kitty template applied once, then edited locally.
fn drifted() -> TestEnv {
    let env = kitty("font_size {{ vars.font }}\n");
    exits(&env.run(&["apply"]), 0);
    env.write(KEY, b"font_size 99\n", 0o644);
    env
}

#[test]
fn an_edit_that_changes_the_source_replaces_the_drifted_file_after_a_backup() {
    let env = drifted();
    let editor = stub_editor(&env, 0);
    let vars = [("EDITOR", editor.to_str().unwrap())];
    exits(&env.run_with(&vars, &["edit", KEY]), 0);
    assert_eq!(env.read(KEY), b"font_size 14\n# edited\n");
    assert_eq!(token(&env, KEY).as_deref(), Some("clean"));
    assert_eq!(env.backups(KEY), [(b"font_size 99\n".to_vec(), 0o600)]);
}

#[test]
fn an_edit_that_changes_nothing_leaves_the_drift_alone() {
    let env = drifted();
    let err = exits(&env.run_with(&[("EDITOR", "true")], &["edit", KEY]), 3);
    assert!(err.contains("template-drift"), "{err}");
    assert_eq!(env.read(KEY), b"font_size 99\n");
    assert_eq!(env.backups(KEY), []);
    assert_eq!(token(&env, KEY).as_deref(), Some("template-drift"));
}

#[test]
fn take_repo_answers_template_drift() {
    let env = drifted();
    let before = env.four_trees();
    exits(&env.run(&["apply", "--take-repo", "--dry-run"]), 0);
    assert_eq!(env.four_trees(), before);

    exits(&env.run(&["apply", "--take-repo"]), 0);
    assert_eq!(env.read(KEY), b"font_size 14\n");
    assert_eq!(env.backups(KEY), [(b"font_size 99\n".to_vec(), 0o600)]);
    assert_eq!(token(&env, KEY).as_deref(), Some("clean"));
}

#[test]
fn sync_take_repo_writes_the_drifted_file_once_and_captures_nothing() {
    let env = kitty("font_size {{ vars.font }}\n");
    exits(&env.run(&["init", "--remote", &env.remote_url()]), 0);
    exits(&env.run(&["apply"]), 0);
    exits(&env.run(&["sync"]), 0);
    let head = env.git(&["-C", &env.repo().to_string_lossy(), "rev-parse", "HEAD"]);
    env.write(KEY, b"font_size 99\n", 0o644);

    exits(&env.run(&["sync", "--take-repo"]), 0);
    assert_eq!(env.read(KEY), b"font_size 14\n");
    assert_eq!(env.backups(KEY), [(b"font_size 99\n".to_vec(), 0o600)]);
    assert_eq!(
        env.git(&["-C", &env.repo().to_string_lossy(), "rev-parse", "HEAD"]),
        head
    );
    assert!(!env.repo().join(KEY).exists());
}

#[test]
fn restore_keeps_a_copy_of_the_uncommitted_source_it_replaces() {
    let env = kitty("font_size {{ vars.font }}\n");
    exits(&env.run(&["apply"]), 0);
    exits(&env.run(&["sync"]), 0);
    env.write_repo(SOURCE, b"font_size {{ vars.font }}\nbold yes\n");
    exits(&env.run(&["sync"]), 0);
    env.write_repo(SOURCE, b"font_size 1\n");

    exits(&env.run(&["restore", KEY, "--from", "HEAD~1"]), 0);
    assert_eq!(env.backups(SOURCE), [(b"font_size 1\n".to_vec(), 0o600)]);
}

#[test]
fn sync_without_a_remote_still_applies_after_capturing() {
    let env = drifted();
    exits(&env.run(&["sync", "--take-repo"]), 0);
    assert_eq!(env.read(KEY), b"font_size 14\n");
    assert_eq!(env.backups(KEY), [(b"font_size 99\n".to_vec(), 0o600)]);
    assert_eq!(token(&env, KEY).as_deref(), Some("clean"));

    env.write_machine_config("[vars]\nfont = \"16\"\n");
    exits(&env.run(&["sync"]), 0);
    assert_eq!(env.read(KEY), b"font_size 16\n");
}

#[test]
fn a_dry_run_sync_without_a_remote_previews_the_apply() {
    let env = drifted();
    let before = env.four_trees();
    let err = exits(&env.run(&["sync", "--take-repo", "--dry-run"]), 0);
    assert!(err.contains(KEY), "{err}");
    assert_eq!(env.four_trees(), before);
}
