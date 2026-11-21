mod common;

use common::{TestEnv, xxh3 as hash_hex};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::process::Output;

type R<T = ()> = Result<T, Box<dyn Error>>;

fn at<'a>(value: &'a Value, path: &str) -> &'a Value {
    value.pointer(path).unwrap_or(&Value::Null)
}

fn records(out: &Output, code: i32) -> R<Vec<Value>> {
    assert_eq!(
        out.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut lines: Vec<_> = out.stdout.split(|byte| *byte == b'\n').collect();
    assert_eq!(lines.pop(), Some(&b""[..]), "missing terminal LF");
    let records: Vec<Value> = lines
        .into_iter()
        .map(serde_json::from_slice)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        records
            .iter()
            .filter(|record| record["record"] == "result")
            .count(),
        1
    );
    let result = records.last().ok_or("missing final result")?;
    assert_eq!(result["record"], "result");
    assert_eq!(result["exit_code"], code);
    Ok(records)
}

fn find<'a>(records: &'a [Value], kind: &str) -> R<&'a Value> {
    records
        .iter()
        .find(|record| record["record"] == kind)
        .ok_or_else(|| format!("missing {kind}").into())
}

fn keys(records: &[Value]) -> BTreeSet<String> {
    records
        .iter()
        .filter(|record| record["record"] == "undecided" && record["attention"] == true)
        .filter_map(|record| {
            record
                .pointer("/key/raw")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

fn run_argv(env: &TestEnv, step: &Value) -> R<Output> {
    let argv: Vec<OsString> = step["argv"]
        .as_array()
        .ok_or("missing argv")?
        .iter()
        .map(|value| {
            if let Some(text) = value.as_str() {
                return Ok(OsString::from(text));
            }
            let bytes = value["bytes"]
                .as_array()
                .ok_or("argument bytes")?
                .iter()
                .map(|value| {
                    value
                        .as_u64()
                        .and_then(|n| u8::try_from(n).ok())
                        .ok_or("byte")
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, Box<dyn Error>>(OsString::from_vec(bytes))
        })
        .collect::<Result<_, _>>()?;
    match argv.first().and_then(|arg| arg.to_str()) {
        Some("dotman") => Ok(env.dot_process().args(argv.iter().skip(1)).output()?),
        Some("env") => {
            assert_eq!(
                argv.get(1).and_then(|arg| arg.to_str()),
                Some("GIT_EDITOR=true")
            );
            assert_eq!(argv.get(2).and_then(|arg| arg.to_str()), Some("dotman"));
            Ok(env
                .dot_process()
                .env("GIT_EDITOR", "true")
                .args(argv.iter().skip(3))
                .output()?)
        }
        Some("rm") => {
            assert_eq!(argv.get(1).and_then(|arg| arg.to_str()), Some("--"));
            assert!(
                argv.iter()
                    .skip(2)
                    .all(|arg| std::path::Path::new(arg).starts_with(env.repo().join(".git")))
            );
            Ok(env
                .process_for(std::path::Path::new("rm"))
                .args(argv.iter().skip(1))
                .output()?)
        }
        _ => Err("unexpected fixture program".into()),
    }
}

#[test]
fn json_outcomes_frame_writes_attention_failures_and_nullable_context() -> R {
    let env = TestEnv::new();
    for command in ["apply", "sync", "add"] {
        let mut args = vec![command, "--json", "--dry-run"];
        if command == "add" {
            args.push(".a");
        }
        let before = env.four_trees();
        let report = records(&env.run(&args), 1)?;
        assert!(find(&report, "context")?["repo_id"].is_null());
        assert_eq!(find(&report, "result")?["error_kind"], "other");
        assert_eq!(env.four_trees(), before);
    }
    env.ok(&["init"]);
    env.write(".a", b"original-content", 0o600);
    let report = records(&env.run(&["add", "--json", ".a"]), 0)?;
    assert_eq!(find(&report, "context")?["command"], "add");
    env.write_repo(".a", b"repo-version");
    env.write(".a", b"home-version", 0o600);
    let report = records(&env.run(&["apply", "--json"]), 3)?;
    assert_eq!(keys(&report), BTreeSet::from([".a".to_owned()]));
    assert_eq!(env.read(".a"), b"home-version");
    let report = records(&env.run(&["apply", "--json", "--take-repo"]), 0)?;
    assert!(keys(&report).is_empty());
    assert_eq!(env.read(".a"), b"repo-version");
    let before = env.four_trees();
    let report = records(&env.run(&["add", "--json", "../outside"]), 1)?;
    assert_eq!(find(&report, "result")?["error_kind"], "other");
    assert_eq!(env.four_trees(), before);
    let lock = dotman::lock::Lock::acquire(&env.state_dir().join("lock"), false)?;
    let report = records(&env.run(&["apply", "--json"]), 1)?;
    assert_eq!(find(&report, "result")?["error_kind"], "lock_held");
    drop(lock);
    Ok(())
}

#[test]
fn json_outgoing_review_binds_observed_captures_stored_edits_head_and_messages() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", b"base", 0o600);
    env.write(".deleted", b"deleted", 0o644);
    env.ok(&["add", ".a", ".deleted"]);
    env.ok(&["sync"]);
    env.write(".a", b"raw-home-edit", 0o600);
    env.write_repo(".staged", b"staged");
    env.ok(&["git", "add", "--", ".staged"]);
    env.write_repo(".staged", b"unstaged-wins");
    env.write_repo(".unstaged", b"unstaged");
    env.write_repo("install.sh", b"bootstrap-edit");
    fs::remove_file(env.repo().join(".deleted"))?;
    let head = env.git(&[
        "-C",
        env.repo().to_str().ok_or("repo path")?,
        "rev-parse",
        "HEAD",
    ]);
    let before = env.four_trees();
    let report = records(
        &env.run(&[
            "sync",
            "--dry-run",
            "--json",
            "--message=review \u{2028} 'quoted'",
        ]),
        0,
    )?;
    assert_eq!(env.four_trees(), before);
    assert_eq!(
        find(&report, "outgoing")?.pointer("/head/raw"),
        Some(&json!(head.trim()))
    );
    assert!(find(&report, "outgoing")?["tracking_commit"].is_null());
    find(&report, "ahead_commit")?;
    let capture = find(&report, "capture")?;
    assert_eq!(at(capture, "/key/raw"), ".a");
    assert_eq!(at(capture, "/source/raw"), ".a");
    assert_eq!(
        at(capture, "/raw_h/tuple/hash"),
        &json!(hash_hex(b"raw-home-edit"))
    );
    assert_eq!(at(capture, "/raw_h/tuple/mode"), "0600");
    assert_eq!(
        at(capture, "/stored_before/tuple/hash"),
        &json!(hash_hex(b"base"))
    );
    assert_eq!(at(capture, "/stored_after/tuple/mode"), "0644");
    assert_eq!(capture["hypothetical"], true);
    let edit = report
        .iter()
        .find(|record| at(record, "/path/raw") == ".staged")
        .ok_or("missing staged edit")?;
    assert_eq!(
        at(edit, "/stored/tuple/hash"),
        &json!(hash_hex(b"unstaged-wins"))
    );
    let deleted = report
        .iter()
        .find(|record| at(record, "/path/raw") == ".deleted")
        .ok_or("missing deletion")?;
    assert_eq!(at(deleted, "/stored/state"), "missing");
    let own = report
        .iter()
        .find(|record| at(record, "/path/raw") == "install.sh")
        .ok_or("missing own edit")?;
    assert_eq!(own["record"], "own");
    assert_eq!(
        at(own, "/stored/tuple/hash"),
        &json!(hash_hex(b"bootstrap-edit"))
    );
    assert_eq!(
        at(find(&report, "message")?, "/message/raw"),
        "review \u{2028} 'quoted'"
    );
    assert!(!serde_json::to_string(&report)?.contains("raw-home-edit"));
    Ok(())
}

#[test]
fn json_outgoing_encryption_is_hypothetical_and_hooks_stay_off_stdout() -> R {
    if !common::age_installed() {
        return Ok(());
    }
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["key", "init"]);
    env.write(".secret", b"private-original", 0o600);
    env.ok(&["add", "--encrypt", ".secret"]);
    env.ok(&["sync"]);
    env.write(".secret", b"private-changed", 0o600);
    let before = env.four_trees();
    let report = records(&env.run(&["sync", "--dry-run", "--json"]), 0)?;
    assert_eq!(env.four_trees(), before);
    let capture = find(&report, "capture")?;
    assert_eq!(at(capture, "/source/raw"), ".secret{age}");
    assert_eq!(
        at(capture, "/stored_after/reason"),
        "hypothetical_ciphertext"
    );
    assert_eq!(
        at(capture, "/raw_h/tuple/hash"),
        &json!(hash_hex(b"private-changed"))
    );
    assert_ne!(
        at(capture, "/stored_before/tuple/hash"),
        at(capture, "/raw_h/tuple/hash")
    );
    assert!(!serde_json::to_string(&report)?.contains("private-changed"));
    env.write_repo(".hook", b"apply-hook-fixture");
    let mut config = env.repo_config();
    config.hooks.insert(
        ".hook".to_owned(),
        "printf 'hook-child-output'; printf 'hook-error-output' >&2".to_owned(),
    );
    config.save(&env.repo().join(".dotman.toml"), false)?;
    let out = env.run(&["apply", "--json", "--approve-hooks"]);
    let report = records(&out, 0)?;
    assert!(String::from_utf8(out.stderr)?.contains("hook-child-output"));
    assert!(
        report
            .iter()
            .any(|record| record["record"] == "hook" && record["outcome"] == "succeeded")
    );
    Ok(())
}

#[test]
fn json_pull_conflict_recovery_argv_is_literal_and_runs_as_emitted() -> R {
    let a = TestEnv::new();
    a.ok(&["init", "--remote", &a.remote_url()]);
    for key in [
        "--approve-hooks",
        "--take-repo",
        "-m",
        "~x",
        "space ' quote",
    ] {
        a.write(key, b"base\n", 0o644);
        a.dot()
            .args(["add", "--"])
            .arg(a.home().join(key))
            .assert()
            .success();
    }
    a.ok(&["sync"]);
    a.git(&[
        "--git-dir",
        a.remote_path().to_str().ok_or("remote path")?,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    let b = a.second_device();
    b.ok(&["init", &b.remote_url(), "--take-repo"]);
    for key in [
        "--approve-hooks",
        "--take-repo",
        "-m",
        "~x",
        "space ' quote",
    ] {
        a.write(key, b"from a\n", 0o644);
        b.write(key, b"from b\n", 0o644);
    }
    a.ok(&["sync"]);
    let out = b.run(&["sync", "--json"]);
    let report = records(&out, 4)?;
    let result = find(&report, "result")?;
    assert_eq!(result["error_kind"], "pull_conflict");
    let steps = result["recovery"].as_array().ok_or("missing recovery")?;
    let step = steps
        .iter()
        .find(|step| step["argv"] == json!(["dotman", "sync", "--take-repo"]))
        .ok_or("missing take-repo recovery")?;
    assert!(run_argv(&b, step)?.status.success());
    for key in [
        "--approve-hooks",
        "--take-repo",
        "-m",
        "~x",
        "space ' quote",
    ] {
        assert_eq!(b.read(key), b"from a\n");
    }
    let state = dotman::state::State::read(&b.state_dir().join("state.toml"))?
        .ok_or("missing state after recovery")?;
    assert!(state.hooks.approved.is_none());
    Ok(())
}

#[test]
fn json_mass_delete_and_in_progress_have_typed_final_categories() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    for n in 0..12 {
        env.write_repo(&format!(".file-{n}"), b"base");
    }
    env.ok(&["sync"]);
    for n in 0..12 {
        fs::remove_file(env.repo().join(format!(".file-{n}")))?;
    }
    let before = env.four_trees();
    let report = records(&env.run(&["sync", "--dry-run", "--json"]), 1)?;
    assert_eq!(find(&report, "result")?["error_kind"], "mass_delete");
    assert_eq!(env.four_trees(), before);
    env.ok(&["git", "checkout", "--", "."]);
    env.ok(&["git", "bisect", "start"]);
    let report = records(&env.run(&["apply", "--json"]), 4)?;
    assert!(find(&report, "context")?["repo_id"].is_null());
    let result = find(&report, "result")?;
    assert_eq!(result["error_kind"], "in_progress");
    let step = result["recovery"]
        .as_array()
        .ok_or("missing recovery")?
        .iter()
        .find(|step| step["argv"] == json!(["dotman", "git", "bisect", "reset"]))
        .ok_or("missing reset")?;
    assert!(run_argv(&env, step)?.status.success());
    records(&env.run(&["apply", "--json"]), 0)?;
    Ok(())
}

#[test]
fn json_system_capture_binds_the_synthetic_target_and_stored_source() -> R {
    let env = common::system_setup_none(&["/etc"]);
    let key = "/etc/keyd/default.conf";
    env.write_system(key, b"system-base", 0o600);
    env.ok(&["add", "--system", key]);
    env.ok(&["sync"]);
    env.write_system(key, b"system-edit", 0o640);
    let before = env.four_trees();
    let system_before = common::tree_checksum(&env.system_root());
    let report = records(&env.run(&["sync", "--dry-run", "--json"]), 0)?;
    assert_eq!(env.four_trees(), before);
    assert_eq!(common::tree_checksum(&env.system_root()), system_before);
    let capture = find(&report, "capture")?;
    assert_eq!(at(capture, "/key/raw"), key);
    assert_eq!(
        at(capture, "/source/raw"),
        ".dotman/system/etc/keyd/default.conf"
    );
    assert_eq!(capture["scope"], "system");
    assert_eq!(
        at(capture, "/raw_h/tuple/hash"),
        &json!(hash_hex(b"system-edit"))
    );
    assert_eq!(at(capture, "/raw_h/tuple/mode"), "0640");
    assert_eq!(at(capture, "/stored_after/tuple/mode"), "0644");
    assert_eq!(
        at(capture, "/logical_r/tuple/hash"),
        &json!(hash_hex(b"system-base"))
    );
    Ok(())
}

#[test]
fn json_remote_dry_run_reviews_current_writes_pending_hooks_and_unshown_tables() -> R {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write_repo(".pending", b"pending-base");
    let mut config = env.repo_config();
    config
        .hooks
        .insert(".*".to_owned(), "echo ran >> hook-marker".to_owned());
    config.save(&env.repo().join(".dotman.toml"), false)?;
    env.ok(&["apply"]);
    env.write_repo(".fresh", b"fresh-base");
    let before = env.four_trees();
    let report = records(
        &env.run(&["sync", "--dry-run", "--json", "--approve-hooks"]),
        0,
    )?;
    assert_eq!(env.four_trees(), before);
    let hooks = find(&report, "hooks")?;
    assert_eq!(hooks["approved"], true);
    assert_eq!(at(hooks, "/pending_keys/0/raw"), ".pending");
    assert_eq!(at(hooks, "/commands/0/raw"), "echo ran >> hook-marker");
    assert_eq!(
        at(hooks, "/pending_commands/0/raw"),
        "echo ran >> hook-marker"
    );
    assert_eq!(hooks["commands"].as_array().ok_or("commands")?.len(), 1);
    assert!(
        report
            .iter()
            .any(|record| record["record"] == "hook" && record["outcome"] == "would_run")
    );
    assert!(
        !report
            .iter()
            .any(|record| record["record"] == "hook" && record["outcome"] == "started")
    );
    assert!(!env.home().join("hook-marker").exists());
    config
        .hooks
        .insert(".*".to_owned(), "echo changed >> hook-marker".to_owned());
    config.save(&env.repo().join(".dotman.toml"), false)?;
    let before = env.four_trees();
    let report = records(
        &env.run(&["sync", "--dry-run", "--json", "--approve-hooks"]),
        3,
    )?;
    assert_eq!(env.four_trees(), before);
    assert_eq!(find(&report, "hooks")?["unshown"], true);
    assert!(
        report
            .iter()
            .any(|record| record["record"] == "hook" && record["outcome"] == "unshown")
    );
    assert!(!env.home().join("hook-marker").exists());
    Ok(())
}

fn outgoing_paths(report: &[Value]) -> R<BTreeSet<String>> {
    report
        .iter()
        .filter(|record| matches!(record["record"].as_str(), Some("own" | "repo_edit")))
        .map(|record| {
            at(record, "/path/raw")
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "missing outgoing path".into())
        })
        .collect()
}

fn committed_paths(env: &TestEnv) -> R<BTreeSet<String>> {
    let out = env.git_raw(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "diff-tree",
        "--no-commit-id",
        "--name-only",
        "-r",
        "-z",
        "HEAD",
    ]);
    assert!(out.status.success());
    out.stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| Ok(std::str::from_utf8(path)?.to_owned()))
        .collect()
}

fn assert_stored_after_sync(env: &TestEnv, report: &[Value]) -> R {
    for record in report
        .iter()
        .filter(|record| matches!(record["record"].as_str(), Some("own" | "repo_edit")))
    {
        let path = env
            .repo()
            .join(at(record, "/path/raw").as_str().ok_or("path")?);
        if at(record, "/stored/state") == "missing" {
            assert!(!path.exists(), "{}", path.display());
        } else {
            assert_eq!(
                at(record, "/stored/tuple/hash"),
                &json!(hash_hex(&fs::read(&path)?))
            );
            assert_eq!(
                at(record, "/stored/tuple/mode"),
                &json!(format!("{:04o}", common::mode_of(&path) & 0o777))
            );
        }
    }
    Ok(())
}

#[test]
fn outgoing_projection_matches_real_capture_settings_and_forget_all_variants() -> R {
    for case in ["mode", "edited", "forgotten"] {
        let review = TestEnv::new();
        let real = TestEnv::new();
        for env in [&review, &real] {
            env.ok(&["init", "--host", "testbox"]);
            env.write(".a", b"base", 0o644);
            env.ok(&["add", ".a"]);
            env.write_repo(".a{host=other}", b"other-host");
            env.ok(&["sync"]);
            match case {
                "mode" => fs::set_permissions(
                    env.home().join(".a"),
                    std::os::unix::fs::PermissionsExt::from_mode(0o600),
                )?,
                "edited" => {
                    env.write(".a", b"capture-this", 0o600);
                    env.write_repo(".a", b"unsaved-source");
                }
                _ => fs::remove_file(env.home().join(".a"))?,
            }
        }
        let answer = if case == "forgotten" {
            "--forget-deleted"
        } else {
            "--keep-local"
        };
        let before = review.four_trees();
        let index = fs::read(review.repo().join(".git/index"))?;
        let report = records(&review.run(&["sync", "--json", "--dry-run", answer]), 0)?;
        assert_eq!(review.four_trees(), before);
        assert_eq!(fs::read(review.repo().join(".git/index"))?, index);
        real.ok(&["sync", answer]);
        assert_eq!(outgoing_paths(&report)?, committed_paths(&real)?, "{case}");
        assert_stored_after_sync(&real, &report)?;
        let subject = real.git(&[
            "-C",
            real.repo().to_str().ok_or("repo")?,
            "log",
            "-1",
            "--format=%s",
        ]);
        assert_eq!(
            at(find(&report, "message")?, "/message/raw"),
            subject.trim(),
            "{case}"
        );
        if case == "mode" {
            assert_eq!(
                outgoing_paths(&report)?,
                BTreeSet::from([".dotman.toml".to_owned()])
            );
        }
        if case == "forgotten" {
            assert!(!real.repo().join(".a{host=other}").exists());
        }
    }
    Ok(())
}

#[test]
fn outgoing_projection_preserves_excluded_staged_deletion_and_cancels_reverted_index_edit() -> R {
    for removed in [true, false] {
        let env = TestEnv::new();
        env.ok(&["init"]);
        env.write_repo(".orphan", b"head");
        env.ok(&["git", "add", "-A"]);
        env.ok(&["git", "commit", "-m", "base"]);
        if removed {
            let mut config = env.repo_config();
            config.ignore.patterns.push(".orphan".to_owned());
            config.save(&env.repo().join(".dotman.toml"), false)?;
            env.ok(&["git", "add", "--", ".dotman.toml"]);
            env.ok(&["git", "commit", "-m", "ignore"]);
            env.ok(&["git", "rm", "--cached", "--", ".orphan"]);
        } else {
            env.write_repo(".orphan", b"index");
            env.ok(&["git", "add", "--", ".orphan"]);
            env.write_repo(".orphan", b"head");
        }
        let before = env.four_trees();
        let index = fs::read(env.repo().join(".git/index"))?;
        let head = env.git(&[
            "-C",
            env.repo().to_str().ok_or("repo")?,
            "rev-parse",
            "HEAD",
        ]);
        let report = records(&env.run(&["sync", "--json", "--dry-run"]), 0)?;
        assert_eq!(env.four_trees(), before);
        assert_eq!(fs::read(env.repo().join(".git/index"))?, index);
        if removed {
            assert_eq!(
                outgoing_paths(&report)?,
                BTreeSet::from([".orphan".to_owned()])
            );
            assert_eq!(at(find(&report, "repo_edit")?, "/stored/state"), "missing");
        } else {
            assert!(outgoing_paths(&report)?.is_empty());
            assert!(!report.iter().any(|record| record["record"] == "message"));
        }
        env.ok(&["sync"]);
        if removed {
            assert_eq!(committed_paths(&env)?, outgoing_paths(&report)?);
            assert_eq!(fs::read(env.repo().join(".orphan"))?, b"head");
        } else {
            assert_eq!(
                env.git(&[
                    "-C",
                    env.repo().to_str().ok_or("repo")?,
                    "rev-parse",
                    "HEAD"
                ]),
                head
            );
        }
    }
    Ok(())
}

#[test]
fn in_progress_recovery_checkout_and_add_keep_git_pathspec_neighbours_untouched() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let targets = ["x[1]", "star*", ":(glob)colon*"];
    let neighbours = ["x1", "star-neighbour", "colon-neighbour"];
    for key in targets.into_iter().chain(neighbours) {
        env.write_repo(key, b"base\n");
    }
    env.ok(&["git", "add", "-A"]);
    env.ok(&["git", "commit", "-m", "base"]);
    env.ok(&["git", "checkout", "-b", "side"]);
    for key in targets {
        env.write_repo(key, b"side\n");
    }
    env.ok(&["git", "add", "-A"]);
    env.ok(&["git", "commit", "-m", "side"]);
    env.ok(&["git", "checkout", "main"]);
    for key in targets {
        env.write_repo(key, b"mine\n");
    }
    env.ok(&["git", "add", "-A"]);
    env.ok(&["git", "commit", "-m", "mine"]);
    for key in neighbours {
        env.write_repo(key, b"neighbour-hand-edit\n");
    }
    assert_eq!(env.run(&["git", "merge", "side"]).status.code(), Some(1));
    env.ok(&["git", "merge", "--quit"]);
    let report = records(&env.run(&["apply", "--json"]), 4)?;
    let recovery = find(&report, "result")?["recovery"]
        .as_array()
        .ok_or("recovery")?;
    let checkout = recovery
        .iter()
        .find(|step| at(step, "/argv/2") == "checkout")
        .ok_or("checkout")?;
    let add = recovery
        .iter()
        .find(|step| at(step, "/argv/2") == "add")
        .ok_or("add")?;
    for step in [checkout, add] {
        let argv = step["argv"].as_array().ok_or("argv")?;
        let start = argv
            .iter()
            .position(|arg| arg == "--")
            .ok_or("path boundary")?
            .saturating_add(1);
        assert_eq!(argv.get(start..).ok_or("paths")?.len(), 3);
        assert!(argv.get(start..).ok_or("paths")?.iter().all(|arg| {
            arg.as_str()
                .is_some_and(|arg| arg.starts_with(":(literal)"))
        }));
    }
    assert!(run_argv(&env, checkout)?.status.success());
    for key in targets {
        assert_eq!(fs::read(env.repo().join(key))?, b"mine\n");
        env.write_repo(key, b"resolved\n");
    }
    assert!(run_argv(&env, add)?.status.success());
    for key in neighbours {
        assert_eq!(fs::read(env.repo().join(key))?, b"neighbour-hand-edit\n");
    }
    let staged = env.git_raw(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "diff",
        "--cached",
        "--name-only",
        "-z",
    ]);
    assert!(staged.status.success());
    let actual: BTreeSet<_> = staged
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8(path.to_vec()))
        .collect::<Result<_, _>>()?;
    assert_eq!(actual, targets.into_iter().map(str::to_owned).collect());
    Ok(())
}

#[test]
fn staged_held_rename_is_reset_for_review_and_restored_after_the_real_commit() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".old", b"rename-base", 0o644);
    env.ok(&["add", ".old"]);
    env.ok(&["sync"]);
    env.ok(&["git", "mv", "--", ".old", ".new"]);
    let credential = format!("{}{}", "ghp_", "a".repeat(36));
    env.write_repo(".new", credential.as_bytes());
    let before = env.four_trees();
    let index = fs::read(env.repo().join(".git/index"))?;
    let head = env.git(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "rev-parse",
        "HEAD",
    ]);
    let staged = env.git_raw(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "diff",
        "--cached",
        "--raw",
        "-z",
    ]);
    let report = records(&env.run(&["sync", "--dry-run", "--json"]), 3)?;
    assert_eq!(env.four_trees(), before);
    assert_eq!(fs::read(env.repo().join(".git/index"))?, index);
    assert!(outgoing_paths(&report)?.is_empty());
    records(&env.run(&["sync", "--json"]), 3)?;
    assert_eq!(
        env.git(&[
            "-C",
            env.repo().to_str().ok_or("repo")?,
            "rev-parse",
            "HEAD"
        ]),
        head
    );
    assert_eq!(
        env.git_raw(&[
            "-C",
            env.repo().to_str().ok_or("repo")?,
            "diff",
            "--cached",
            "--raw",
            "-z"
        ])
        .stdout,
        staged.stdout
    );
    assert_eq!(fs::read(env.repo().join(".new"))?, credential.as_bytes());
    Ok(())
}

fn directory_conflict(name: &str) -> R<(TestEnv, Vec<Value>, String)> {
    let a = TestEnv::new();
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write(".base", b"base", 0o644);
    a.ok(&["add", ".base"]);
    a.ok(&["sync"]);
    a.git(&[
        "--git-dir",
        a.remote_path().to_str().ok_or("remote")?,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    let b = a.second_device();
    b.ok(&["init", &b.remote_url(), "--take-repo"]);
    a.write(format!("{name}/child"), b"directory-child", 0o644);
    a.dot()
        .args(["add", "--"])
        .arg(a.home().join(name))
        .assert()
        .success();
    a.ok(&["sync"]);
    b.write(name, b"this-machine-file", 0o644);
    b.dot()
        .args(["add", "--"])
        .arg(b.home().join(name))
        .assert()
        .success();
    let out = b.run(&["sync", "--json", "--keep-local"]);
    let report = records(&out, 4)?;
    Ok((b, report, String::from_utf8(out.stderr)?))
}

fn parked_copy(env: &TestEnv, name: &str) -> R<(String, Value)> {
    let prefix = format!("{name}~");
    let copies = fs::read_dir(env.repo())?
        .map(|entry| Ok(entry?.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    let copies: Vec<_> = copies
        .iter()
        .filter_map(|path| path.to_str().filter(|path| path.starts_with(&prefix)))
        .collect();
    let &[copy] = copies.as_slice() else {
        return Err(format!("expected one parked copy: {copies:?}").into());
    };
    let path = env.repo().join(copy);
    assert_eq!(fs::read(&path)?, b"this-machine-file");
    let literal = format!(":(literal){copy}");
    let tracked = common::git_in(env, &env.repo(), &["ls-files", "-z", "--", &literal]);
    let argv = if tracked.is_empty() {
        json!(["rm", "-f", "--", path.to_str().ok_or("copy path")?])
    } else {
        assert!(tracked.split_terminator('\0').all(|entry| entry == copy));
        json!(["dotman", "git", "rm", "-q", "-f", "--", literal])
    };
    Ok((copy.to_owned(), argv))
}

fn is_copy_removal(step: &Value) -> bool {
    at(step, "/argv/0") == "rm" || at(step, "/argv/2") == "rm"
}

fn run_copy_removal(env: &TestEnv, step: &Value, expected: &Value) -> R<Output> {
    assert_eq!(&step["argv"], expected);
    if at(step, "/argv/0") == "rm" {
        let path = at(step, "/argv/3").as_str().ok_or("copy path")?;
        Ok(env
            .process_for(std::path::Path::new("rm"))
            .args(["-f", "--", path])
            .output()?)
    } else {
        run_argv(env, step)
    }
}

#[test]
fn structured_and_printed_destructive_recovery_keep_literal_copy_neighbours() -> R {
    for name in ["x[1]", "star*", ":(glob)colon*"] {
        let (env, report, stderr) = directory_conflict(name)?;
        let recovery = find(&report, "result")?["recovery"]
            .as_array()
            .ok_or("recovery")?;
        let pull = recovery
            .iter()
            .find(|step| at(step, "/argv/2") == "pull")
            .ok_or("pull")?;
        assert_eq!(run_argv(&env, pull)?.status.code(), Some(1));
        let (copy, expected) = parked_copy(&env, name)?;
        let remove = recovery
            .iter()
            .find(|step| is_copy_removal(step))
            .ok_or("copy removal")?;
        let neighbour = copy
            .replace("[1]", "1")
            .replace('*', "neighbour")
            .replace(":(glob)", "");
        assert_ne!(neighbour, copy);
        env.write_repo(&neighbour, b"neighbour-index");
        env.dot()
            .args(["git", "add", "--"])
            .arg(format!(":(literal){neighbour}"))
            .assert()
            .success();
        env.write_repo(&neighbour, b"neighbour-hand-edit");
        let index_neighbour = env
            .git_raw(&[
                "-C",
                env.repo().to_str().ok_or("repo")?,
                "show",
                &format!(":{neighbour}"),
            ])
            .stdout;
        assert!(run_copy_removal(&env, remove, &expected)?.status.success());
        assert!(!env.repo().join(&copy).exists());
        assert_eq!(
            fs::read(env.repo().join(&neighbour))?,
            b"neighbour-hand-edit"
        );
        env.write_repo(&copy, b"this-machine-file");
        if at(remove, "/argv/0") == "dotman" {
            env.dot()
                .args(["git", "add", "--"])
                .arg(format!(":(literal){copy}"))
                .assert()
                .success();
        }
        let printed = stderr
            .lines()
            .map(str::trim_start)
            .find(|line| {
                line.starts_with("dotman git rm -q -f -- ") || line.starts_with("rm -f -- ")
            })
            .ok_or("printed removal")?;
        assert_eq!(json!(shell_words::split(printed)?), expected);
        let out = common::paste(&env, printed);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!env.repo().join(copy).exists());
        assert_eq!(
            fs::read(env.repo().join(&neighbour))?,
            b"neighbour-hand-edit"
        );
        assert_eq!(
            env.git_raw(&[
                "-C",
                env.repo().to_str().ok_or("repo")?,
                "show",
                &format!(":{neighbour}")
            ])
            .stdout,
            index_neighbour
        );
    }
    Ok(())
}

#[test]
fn system_diff_preview_and_helper_output_do_not_contaminate_json() -> R {
    let env = common::system_setup_none(&["/etc"]);
    let key = "/etc/keyd/default.conf";
    env.write_system(key, b"system-before", 0o600);
    env.ok(&["add", "--system", key]);
    env.write_repo(
        ".dotman/system/etc/keyd/default.conf",
        b"system-preview-content",
    );
    let before = env.four_trees();
    let system = common::tree_checksum(&env.system_root());
    let out = env.run(&["apply", "--system", "--yes", "--dry-run", "--json"]);
    let report = records(&out, 0)?;
    assert_eq!(env.four_trees(), before);
    assert_eq!(common::tree_checksum(&env.system_root()), system);
    assert!(String::from_utf8(out.stderr)?.contains("system-preview-content"));
    assert!(!serde_json::to_string(&report)?.contains("system-preview-content"));
    records(&env.run(&["apply", "--system", "--yes", "--json"]), 0)?;
    assert_eq!(fs::read(env.system_path(key))?, b"system-preview-content");
    Ok(())
}

fn tty_json(env: &TestEnv, args: &[&str], input: &str) -> R<Output> {
    let script = r"
import json, os, pty, signal, subprocess, sys
master, slave = pty.openpty()
child = None
try:
    child = subprocess.Popen([sys.argv[1]] + json.loads(sys.argv[2]), stdin=slave,
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    os.close(slave)
    slave = None
    os.write(master, sys.argv[3].encode())
    out, err = child.communicate(timeout=30)
    sys.stdout.buffer.write(out)
    sys.stderr.buffer.write(err)
    sys.exit(child.returncode)
finally:
    os.close(master)
    if slave is not None:
        os.close(slave)
    if child is not None and child.poll() is None:
        os.killpg(child.pid, signal.SIGKILL)
        child.wait()
";
    Ok(env
        .process_for(std::path::Path::new("python3"))
        .env("DOTMAN_NON_INTERACTIVE", "")
        .args([
            "-c",
            script,
            env!("CARGO_BIN_EXE_dotman"),
            &serde_json::to_string(args)?,
            input,
        ])
        .output()?)
}

#[test]
fn interactive_diff_child_stdout_is_not_a_json_frame() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", b"base", 0o600);
    env.ok(&["add", ".a"]);
    env.write_repo(".a", b"repository-preview-bytes");
    env.write(".a", b"home-preview-bytes", 0o600);
    let out = tty_json(&env, &["apply", "--json"], "d\nt\n")?;
    let report = records(&out, 0)?;
    assert!(String::from_utf8(out.stderr)?.contains("repository-preview-bytes"));
    assert!(!serde_json::to_string(&report)?.contains("repository-preview-bytes"));
    assert_eq!(env.read(".a"), b"repository-preview-bytes");
    Ok(())
}

#[test]
fn forgetting_a_pending_hook_key_does_not_preview_releasing_it() -> R {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write_repo(".pending", b"base");
    let mut config = env.repo_config();
    config
        .hooks
        .insert(".pending".to_owned(), "echo ran >> marker".to_owned());
    config.save(&env.repo().join(".dotman.toml"), false)?;
    env.ok(&["apply"]);
    fs::remove_file(env.home().join(".pending"))?;
    let before = env.four_trees();
    let report = records(
        &env.run(&[
            "sync",
            "--json",
            "--dry-run",
            "--forget-deleted",
            "--approve-hooks",
        ]),
        0,
    )?;
    assert_eq!(env.four_trees(), before);
    let hooks = find(&report, "hooks")?;
    assert_eq!(hooks["pending_keys"], json!([]));
    assert_eq!(hooks["pending_commands"], json!([]));
    assert_eq!(hooks["commands"], json!([]));
    Ok(())
}

#[test]
fn unmodelled_recovery_runs_the_complete_decoded_recipe_after_manual_resolution() -> R {
    let (env, report, _) = directory_conflict("shape[1]")?;
    let recipe = find(&report, "result")?["recovery"]
        .as_array()
        .ok_or("recipe")?;
    assert!(recipe.iter().any(|step| step["kind"] == "instruction"));
    let mut commands = recipe.iter().filter(|step| step["kind"] == "command");
    let pull = commands.next().ok_or("pull command")?;
    assert_eq!(at(pull, "/argv/2"), "pull");
    assert_eq!(run_argv(&env, pull)?.status.code(), Some(1));
    let (copy, expected) = parked_copy(&env, "shape[1]")?;
    let mut removed = false;
    for step in commands {
        let out = if is_copy_removal(step) {
            assert!(!removed, "duplicate copy removal");
            removed = true;
            run_copy_removal(&env, step, &expected)?
        } else {
            run_argv(&env, step)?
        };
        assert_eq!(
            out.status.code(),
            Some(0),
            "{step}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(removed, "missing copy removal");
    assert!(!env.repo().join(copy).exists());
    assert_eq!(env.read("shape[1]"), b"this-machine-file");
    assert!(!env.repo().join(".git/rebase-merge").exists());
    assert!(!env.repo().join(".git/dotman-pull").exists());
    let status = env.git(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "status",
        "--porcelain",
    ]);
    assert!(status.is_empty(), "{status}");
    let state = dotman::state::State::read(&env.state_dir().join("state.toml"))?.ok_or("state")?;
    assert!(state.hooks.approved.is_none());
    Ok(())
}

#[test]
fn recovery_preserves_non_utf8_path_bytes_in_argv_and_printed_shell_commands() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let name = OsString::from_vec(b"raw-\xff[1]*".to_vec());
    let neighbour = "raw-neighbour";
    let path = env.repo().join(&name);
    for branch in ["base", "side", "main"] {
        if branch == "side" {
            env.ok(&["git", "checkout", "-b", "side"]);
        }
        if branch == "main" {
            env.ok(&["git", "checkout", "main"]);
        }
        fs::write(&path, branch.as_bytes())?;
        env.write_repo(neighbour, b"base");
        env.ok(&["git", "add", "-A"]);
        env.ok(&["git", "commit", "-m", branch]);
    }
    env.write_repo(neighbour, b"dirty-neighbour");
    assert_eq!(env.run(&["git", "merge", "side"]).status.code(), Some(1));
    env.ok(&["git", "merge", "--quit"]);
    let out = env.run(&["apply", "--json"]);
    let report = records(&out, 4)?;
    let recipe = find(&report, "result")?["recovery"]
        .as_array()
        .ok_or("recipe")?;
    let checkout = recipe
        .iter()
        .find(|step| at(step, "/argv/2") == "checkout")
        .ok_or("checkout")?;
    let arg = checkout["argv"]
        .as_array()
        .ok_or("argv")?
        .last()
        .ok_or("path")?;
    let mut expected = b":(literal)".to_vec();
    expected.extend_from_slice(b"raw-\xff[1]*");
    assert_eq!(arg["bytes"], json!(expected));
    assert!(run_argv(&env, checkout)?.status.success());
    assert_eq!(fs::read(&path)?, b"main");
    fs::write(&path, b"manual-resolution")?;
    let printed = String::from_utf8(out.stderr)?;
    let add = printed
        .split('`')
        .find(|text| text.contains("xargs -0 dotman git add --"))
        .ok_or("printed octal add")?;
    assert!(common::paste(&env, add).status.success());
    fs::write(&path, b"later-edit")?;
    let checkout = printed
        .split('`')
        .find(|text| text.contains("xargs -0 dotman git checkout HEAD --"))
        .ok_or("printed octal checkout")?;
    assert!(common::paste(&env, checkout).status.success());
    assert_eq!(fs::read(path)?, b"main");
    assert_eq!(fs::read(env.repo().join(neighbour))?, b"dirty-neighbour");
    Ok(())
}

#[test]
fn outgoing_review_distinguishes_observed_modes_and_fails_closed_on_gitlinks() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".raw", b"before");
    env.ok(&["sync"]);
    env.write_repo(".raw", b"after");
    fs::set_permissions(
        env.repo().join(".raw"),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
    )?;
    let report = records(&env.run(&["sync", "--dry-run", "--json"]), 0)?;
    let raw = report
        .iter()
        .find(|record| at(record, "/path/raw") == ".raw")
        .ok_or("raw")?;
    assert_eq!(at(raw, "/stored/tuple/mode"), "0644");
    assert_eq!(at(raw, "/observed/tuple/mode"), "0600");
    env.ok(&["sync"]);
    let head = env.git(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "rev-parse",
        "HEAD",
    ]);
    env.ok(&[
        "git",
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{},.gitlink", head.trim()),
    ]);
    env.ok(&["git", "commit", "-m", "gitlink"]);
    let changed = env.git(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "rev-parse",
        "HEAD",
    ]);
    env.ok(&[
        "git",
        "update-index",
        "--cacheinfo",
        &format!("160000,{},.gitlink", changed.trim()),
    ]);
    let before = env.four_trees();
    let index = fs::read(env.repo().join(".git/index"))?;
    let report = records(&env.run(&["sync", "--dry-run", "--json"]), 1)?;
    assert_eq!(find(&report, "result")?["error_kind"], "other");
    assert_eq!(env.four_trees(), before);
    assert_eq!(fs::read(env.repo().join(".git/index"))?, index);
    Ok(())
}

#[test]
fn outgoing_review_handles_unborn_tracking_templates_links_and_raw_paths() -> R {
    let env = TestEnv::new();
    env.ok(&["init", "--host", "fixture", "--remote", &env.remote_url()]);
    let before = env.four_trees();
    let unborn = records(&env.run(&["sync", "--json", "--dry-run"]), 0)?;
    assert!(find(&unborn, "outgoing")?["head"].is_null());
    assert_eq!(env.four_trees(), before);
    env.write(".tpl", b"base", 0o600);
    env.ok(&["add", "--template", ".tpl"]);
    env.write("base", b"base", 0o600);
    env.write("next", b"next", 0o600);
    env.symlink(".link", "base");
    env.ok(&["add", ".link"]);
    let raw = OsString::from_vec(b"source-\xff".to_vec());
    fs::write(env.repo().join(&raw), b"raw-base")?;
    env.ok(&["git", "add", "-A"]);
    env.ok(&["sync"]);
    env.write_repo(".tpl{tmpl}", b"template-source-edit");
    env.write_repo("install.sh", b"bootstrap-edit");
    fs::write(env.repo().join(&raw), b"raw-edit")?;
    fs::remove_file(env.home().join(".link"))?;
    std::os::unix::fs::symlink(env.home().join("next"), env.home().join(".link"))?;
    let before = env.four_trees();
    let index = fs::read(env.repo().join(".git/index"))?;
    let report = records(&env.run(&["sync", "--json", "--dry-run"]), 0)?;
    assert_eq!(env.four_trees(), before);
    assert_eq!(fs::read(env.repo().join(".git/index"))?, index);
    let position = find(&report, "outgoing")?;
    assert_eq!(position["tracking_commit"], position["head"]);
    assert!(
        !report
            .iter()
            .any(|record| record["record"] == "ahead_commit")
    );
    let raw_record = report
        .iter()
        .find(|record| at(record, "/path/bytes") == &json!(b"source-\xff"))
        .ok_or("raw outgoing path")?;
    assert_eq!(
        at(raw_record, "/stored/tuple/hash"),
        &json!(hash_hex(b"raw-edit"))
    );
    let template = report
        .iter()
        .find(|record| at(record, "/path/raw") == ".tpl{tmpl}")
        .ok_or("template source")?;
    assert_eq!(
        at(template, "/stored/tuple/hash"),
        &json!(hash_hex(b"template-source-edit"))
    );
    let link = report
        .iter()
        .find(|record| at(record, "/path/raw") == ".link")
        .ok_or("link")?;
    assert_eq!(at(link, "/stored/tuple/kind"), "symlink");
    assert_eq!(at(link, "/stored/tuple/hash"), &json!(hash_hex(b"next")));
    env.ok(&["sync"]);
    assert_committed_review(&env, std::slice::from_ref(link))?;
    assert_eq!(
        fs::read_link(env.repo().join(".link"))?,
        std::path::Path::new("next")
    );
    assert_eq!(env.read(".tpl"), b"template-source-edit");
    let actual = env.git_raw(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "diff-tree",
        "--no-commit-id",
        "--name-only",
        "-r",
        "-z",
        "HEAD",
    ]);
    assert!(actual.status.success());
    let expected = outgoing_byte_paths(&report)?;
    let actual: BTreeSet<_> = actual
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    assert_eq!(expected, actual);
    let subject = env.git(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "log",
        "-1",
        "--format=%s",
    ]);
    assert_eq!(
        at(find(&report, "message")?, "/message/raw"),
        subject.trim()
    );
    Ok(())
}

fn outgoing_byte_paths(report: &[Value]) -> R<BTreeSet<Vec<u8>>> {
    report
        .iter()
        .filter(|record| matches!(record["record"].as_str(), Some("own" | "repo_edit")))
        .map(|record| {
            if let Some(raw) = at(record, "/path/raw").as_str() {
                return Ok(raw.as_bytes().to_vec());
            }
            at(record, "/path/bytes")
                .as_array()
                .ok_or("bytes")?
                .iter()
                .map(|n| u8::try_from(n.as_u64().ok_or("byte")?).map_err(Into::into))
                .collect::<R<Vec<_>>>()
        })
        .collect()
}

#[test]
fn outcome_report_only_reasons_do_not_become_attention_or_second_pass_skips() -> R {
    if !common::age_installed() {
        return Ok(());
    }
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["key", "init"]);
    env.write(".secret", b"fixture-private", 0o600);
    env.ok(&["add", "--encrypt", ".secret"]);
    fs::remove_file(env.xdg_config().join("dotman/age-identity.txt"))?;
    let report = records(&env.run(&["apply", "--dry-run", "--json"]), 0)?;
    let locked = report
        .iter()
        .find(|record| record["reason"] == "locked")
        .ok_or("locked")?;
    assert_eq!(locked["attention"], false);
    assert_eq!(locked["skip_next_pass"], false);
    env.write(".config/new/one", b"one", 0o600);
    let mut config = env.repo_config();
    config.dirs.paths.push(".config/new".to_owned());
    config.save(&env.repo().join(".dotman.toml"), false)?;
    let before = env.four_trees();
    let report = records(&env.run(&["sync", "--dry-run", "--json"]), 3)?;
    assert_eq!(env.four_trees(), before);
    assert!(report.iter().any(|record| record["reason"] == "new_held"
        && record["attention"] == true
        && record["skip_next_pass"] == false));
    let dirs: Vec<_> = report
        .iter()
        .filter(|record| record["reason"] == "directory_approval")
        .collect();
    assert_ne!(dirs, [] as [&Value; 0]);
    assert!(
        dirs.iter()
            .all(|record| record["attention"] == false && record["skip_next_pass"] == false)
    );
    assert!(dirs.iter().any(|record| record["key"].is_null()));
    Ok(())
}

#[test]
fn unstashed_recovery_runs_decoded_commands_and_keeps_saved_edits_until_manual_choice() -> R {
    let a = TestEnv::new();
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write("x[1]", b"base\n", 0o600);
    a.ok(&["add", "x[1]"]);
    a.ok(&["sync"]);
    a.git(&[
        "--git-dir",
        a.remote_path().to_str().ok_or("remote")?,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    let mut b = a.second_device();
    b.ok(&["init", &b.remote_url(), "--take-repo"]);
    a.write("x[1]", b"remote\n", 0o600);
    a.ok(&["sync"]);
    let real_git = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .ok_or("git")?;
    let wrappers = b.root().join("wrapper");
    fs::create_dir(&wrappers)?;
    let armed = b.root().join("armed");
    fs::write(&armed, b"")?;
    common::write_exe(
        &wrappers.join("git"),
        &format!(
            "#!/bin/sh\nfor arg in \"$@\"; do if [ \"$arg\" = pull ] && [ -e {armed} ]; then rm {armed}; printf 'late-edit\\n' > {target}; fi; done\nexec {git} \"$@\"\n",
            armed = shell_words::quote(armed.to_str().ok_or("armed")?),
            target = shell_words::quote(b.repo().join("x[1]").to_str().ok_or("target")?),
            git = shell_words::quote(real_git.to_str().ok_or("git")?),
        ),
    )?;
    b.prepend_path(wrappers);
    let report = records(&b.run(&["sync", "--json"]), 4)?;
    assert_eq!(b.read("x[1]"), b"base\n");
    b.write_repo("x1", b"dirty-neighbour");
    let recipe = find(&report, "result")?["recovery"]
        .as_array()
        .ok_or("recipe")?;
    let mut manual = false;
    for step in recipe {
        if step["kind"] == "instruction" {
            manual = true;
            assert!(
                b.git(&["-C", b.repo().to_str().ok_or("repo")?, "stash", "list"])
                    .contains("autostash")
            );
            b.write_repo("x[1]", b"manual-kept-edit\n");
            continue;
        }
        let out = run_argv(&b, step)?;
        assert!(
            out.status.success(),
            "{step}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        if at(step, "/argv/3") == "show" {
            assert!(String::from_utf8(out.stdout)?.contains("late-edit"));
        }
    }
    assert!(manual);
    assert_eq!(b.read("x[1]"), b"manual-kept-edit\n");
    assert_eq!(fs::read(b.repo().join("x1"))?, b"dirty-neighbour");
    assert_eq!(
        b.git(&["-C", b.repo().to_str().ok_or("repo")?, "stash", "list"]),
        ""
    );
    let state = dotman::state::State::read(&b.state_dir().join("state.toml"))?.ok_or("state")?;
    assert!(state.hooks.approved.is_none());
    Ok(())
}

#[test]
fn remote_dry_run_reports_system_confirmation_and_hooks_for_confirmed_writes() -> R {
    let env = common::system_setup_none(&["/etc"]);
    let key = "/etc/keyd/default.conf";
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write_system(key, b"system-base", 0o600);
    env.ok(&["add", "--system", key]);
    env.ok(&["sync"]);
    env.write_repo(".dotman/system/etc/keyd/default.conf", b"system-incoming");
    let mut config = env.repo_config();
    config
        .hooks
        .insert(key.to_owned(), "echo system-hook >> marker".to_owned());
    config.save(&env.repo().join(".dotman.toml"), false)?;
    let before = env.four_trees();
    let system = common::tree_checksum(&env.system_root());
    let report = records(
        &env.run(&["sync", "--json", "--dry-run", "--system", "--approve-hooks"]),
        3,
    )?;
    let confirmation = report
        .iter()
        .find(|record| record["reason"] == "system_confirmation")
        .ok_or("confirmation")?;
    assert_eq!(at(confirmation, "/key/raw"), key);
    assert_eq!(confirmation["attention"], true);
    assert_eq!(confirmation["skip_next_pass"], true);
    assert_eq!(find(&report, "hooks")?["commands"], json!([]));
    let report = records(
        &env.run(&[
            "sync",
            "--json",
            "--dry-run",
            "--system",
            "--yes",
            "--approve-hooks",
        ]),
        0,
    )?;
    assert_eq!(
        find(&report, "hooks")?["commands"],
        json!([{
            "raw": "echo system-hook >> marker", "shown": "echo system-hook >> marker"
        }])
    );
    assert_eq!(env.four_trees(), before);
    assert_eq!(common::tree_checksum(&env.system_root()), system);
    records(
        &env.run(&["sync", "--json", "--system", "--yes", "--approve-hooks"]),
        0,
    )?;
    assert_eq!(fs::read(env.system_path(key))?, b"system-incoming");
    assert_eq!(env.read("marker"), b"system-hook\n");
    Ok(())
}

#[test]
fn in_progress_rebase_recipe_obeys_stale_lock_and_abort_conditions() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", b"base", 0o600);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    env.ok(&["git", "checkout", "-b", "side"]);
    env.write_repo(".a", b"side");
    env.ok(&["git", "commit", "-am", "side"]);
    env.ok(&["git", "checkout", "main"]);
    env.write_repo(".a", b"mine");
    env.ok(&["git", "commit", "-am", "mine"]);
    let original = env.git(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "rev-parse",
        "HEAD",
    ]);
    assert_eq!(
        env.run(&["git", "rebase", "--merge", "side"]).status.code(),
        Some(1)
    );
    let lock = env.repo().join(".git/index.lock");
    fs::write(&lock, b"fixture-stale-lock")?;
    let report = records(&env.run(&["apply", "--json"]), 4)?;
    let recipe = find(&report, "result")?["recovery"]
        .as_array()
        .ok_or("recipe")?;
    assert!(recipe.iter().any(|step| {
        at(step, "/text/raw")
            .as_str()
            .is_some_and(|text| text.contains("no git process"))
    }));
    assert!(
        recipe
            .iter()
            .any(|step| step["when"] == "only if rebase --abort failed")
    );
    let mut aborted = false;
    for step in recipe.iter().filter(|step| step["kind"] == "command") {
        match step["when"].as_str() {
            Some(
                "only if rebase --abort failed"
                | "after the fallback rebase --quit succeeds"
                | "after the fallback checkout succeeds",
            ) => continue,
            Some("after the repository is recovered") => assert!(aborted),
            Some("after removing the stale locks") => {
                assert!(!lock.exists());
                aborted = true;
            }
            None => {}
            other => return Err(format!("unexpected condition {other:?}").into()),
        }
        let out = run_argv(&env, step)?;
        assert!(
            out.status.success(),
            "{step}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(aborted);
    assert!(!env.repo().join(".git/rebase-merge").exists());
    assert_eq!(
        env.git(&[
            "-C",
            env.repo().to_str().ok_or("repo")?,
            "rev-parse",
            "HEAD"
        ]),
        original
    );
    assert_eq!(env.read(".a"), b"mine");
    Ok(())
}

fn assert_committed_review(env: &TestEnv, report: &[Value]) -> R {
    for record in report
        .iter()
        .filter(|record| matches!(record["record"].as_str(), Some("own" | "repo_edit")))
    {
        let path = at(record, "/path/raw").as_str().ok_or("outgoing path")?;
        let tree = env.git(&[
            "-C",
            env.repo().to_str().ok_or("repo")?,
            "ls-tree",
            "-r",
            "-z",
            "HEAD",
            "--",
            &format!(":(literal){path}"),
        ]);
        let entry = tree
            .split('\0')
            .find(|entry| entry.split_once('\t').is_some_and(|(_, name)| name == path))
            .unwrap_or("");
        if at(record, "/stored/state") == "missing" {
            assert_eq!(entry, "", "{path}");
            continue;
        }
        let mode = entry.split_whitespace().next().ok_or("tree mode")?;
        let expected = match mode {
            "100644" => "0644",
            "100755" => "0755",
            "120000" => "0777",
            _ => return Err(format!("unexpected tree mode {mode}").into()),
        };
        let blob = env.git_raw(&[
            "-C",
            env.repo().to_str().ok_or("repo")?,
            "show",
            &format!("HEAD:{path}"),
        ]);
        assert!(blob.status.success());
        assert_eq!(at(record, "/stored/tuple/mode"), expected, "{path}");
        assert_eq!(
            at(record, "/stored/tuple/hash"),
            &json!(hash_hex(&blob.stdout)),
            "{path}"
        );
    }
    Ok(())
}

#[test]
fn final_index_review_uses_owner_execute_and_effective_filemode_with_index_modes() -> R {
    use std::os::unix::fs::PermissionsExt as _;
    for filemode in [true, false] {
        for index_mode in [None, Some(0o644), Some(0o755)] {
            for physical in [0o654, 0o645, 0o744] {
                let env = TestEnv::new();
                env.ok(&["init"]);
                if let Some(mode) = index_mode {
                    env.write_repo(".mode", b"original");
                    env.ok(&["git", "add", "-A"]);
                    env.ok(&["git", "commit", "-m", "base"]);
                    env.ok(&[
                        "git",
                        "update-index",
                        if mode == 0o755 {
                            "--chmod=+x"
                        } else {
                            "--chmod=-x"
                        },
                        ".mode",
                    ]);
                } else {
                    env.ok(&["sync"]);
                }
                env.ok(&["git", "config", "--global", "core.filemode", "true"]);
                env.ok(&[
                    "git",
                    "config",
                    "--local",
                    "core.filemode",
                    if filemode { "true" } else { "false" },
                ]);
                env.write_repo(".mode", b"edited");
                fs::set_permissions(
                    env.repo().join(".mode"),
                    fs::Permissions::from_mode(physical),
                )?;
                let before = env.four_trees();
                let index = fs::read(env.repo().join(".git/index"))?;
                let objects = common::tree_checksum(&env.repo().join(".git/objects"));
                let report = records(&env.run(&["sync", "--json", "--dry-run"]), 0)?;
                assert_eq!(env.four_trees(), before);
                assert_eq!(fs::read(env.repo().join(".git/index"))?, index);
                assert_eq!(
                    common::tree_checksum(&env.repo().join(".git/objects")),
                    objects
                );
                let edit = report
                    .iter()
                    .find(|record| at(record, "/path/raw") == ".mode")
                    .ok_or("mode edit")?;
                assert_eq!(
                    at(edit, "/observed/tuple/mode"),
                    &json!(format!("{physical:04o}"))
                );
                env.ok(&["sync"]);
                assert_committed_review(&env, &report)?;
                assert_eq!(outgoing_paths(&report)?, committed_paths(&env)?);
            }
        }
    }
    Ok(())
}

#[test]
fn final_index_review_handles_nested_file_directory_replacements() -> R {
    for directory_first in [false, true] {
        let env = TestEnv::new();
        env.ok(&["init"]);
        let original = if directory_first {
            ".p/nested/child"
        } else {
            ".p"
        };
        let replacement = if directory_first {
            ".p"
        } else {
            ".p/nested/child"
        };
        env.write_repo(original, b"original");
        env.ok(&["git", "add", "-A"]);
        env.ok(&["git", "commit", "-m", "base"]);
        if directory_first {
            fs::remove_dir_all(env.repo().join(".p"))?;
        } else {
            fs::remove_file(env.repo().join(".p"))?;
        }
        env.write_repo(replacement, b"replacement");
        let before = env.four_trees();
        let index = fs::read(env.repo().join(".git/index"))?;
        let objects = common::tree_checksum(&env.repo().join(".git/objects"));
        let report = records(&env.run(&["sync", "--json", "--dry-run"]), 0)?;
        assert_eq!(env.four_trees(), before);
        assert_eq!(fs::read(env.repo().join(".git/index"))?, index);
        assert_eq!(
            common::tree_checksum(&env.repo().join(".git/objects")),
            objects
        );
        assert_eq!(
            outgoing_paths(&report)?,
            [original.to_owned(), replacement.to_owned()]
                .into_iter()
                .collect()
        );
        env.ok(&["sync"]);
        assert_committed_review(&env, &report)?;
        assert_eq!(outgoing_paths(&report)?, committed_paths(&env)?);
        let subject = env.git(&[
            "-C",
            env.repo().to_str().ok_or("repo")?,
            "log",
            "-1",
            "--format=%s",
        ]);
        assert_eq!(
            at(find(&report, "message")?, "/message/raw"),
            subject.trim()
        );
    }
    Ok(())
}

#[test]
fn final_index_review_refuses_an_embedded_repository_displacing_a_file() -> R {
    for separate in [false, true] {
        for review in [true, false] {
            let env = TestEnv::new();
            env.ok(&["init"]);
            env.write_repo("p", b"original");
            env.ok(&["sync"]);
            fs::remove_file(env.repo().join("p"))?;
            let embedded = env.repo().join("p");
            let git_dir = env.root().join("embedded.git");
            let embedded_path = embedded.to_str().ok_or("embedded path")?;
            if separate {
                env.git(&[
                    "init",
                    "--template=",
                    "--separate-git-dir",
                    git_dir.to_str().ok_or("git directory")?,
                    embedded_path,
                ]);
            } else {
                env.git(&["init", "--template=", embedded_path]);
            }
            fs::write(embedded.join("content"), b"embedded content")?;
            env.git(&["-C", embedded_path, "add", "content"]);
            env.git(&[
                "-C",
                embedded_path,
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-m",
                "embedded base",
            ]);
            if review {
                let before = common::tree_checksum(env.root());
                let index = fs::read(env.repo().join(".git/index"))?;
                let out = env.run(&["sync", "--json", "--dry-run"]);
                let report = records(&out, 1)?;
                assert_eq!(find(&report, "result")?["error_kind"], "other");
                assert!(
                    String::from_utf8_lossy(&out.stderr).contains("embedded Git repository"),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                assert_eq!(common::tree_checksum(env.root()), before);
                assert_eq!(fs::read(env.repo().join(".git/index"))?, index);
            } else {
                env.ok(&["git", "add", "-A"]);
                let entry = env.git(&[
                    "-C",
                    env.repo().to_str().ok_or("repo")?,
                    "ls-files",
                    "--stage",
                    "--",
                    "p",
                ]);
                assert!(entry.starts_with("160000 "), "{entry}");
            }
        }
    }
    Ok(())
}

#[test]
fn final_index_review_refuses_invalid_or_symlinked_embedded_git_metadata() -> R {
    for shape in ["file", "directory", "symlink"] {
        let env = TestEnv::new();
        env.ok(&["init"]);
        env.write_repo("p", b"original");
        env.ok(&["sync"]);
        fs::remove_file(env.repo().join("p"))?;
        fs::create_dir(env.repo().join("p"))?;
        let marker = env.repo().join("p/.git");
        match shape {
            "file" => fs::write(marker, b"not a gitfile")?,
            "directory" => fs::create_dir(marker)?,
            _ => std::os::unix::fs::symlink(env.root().join("absent"), marker)?,
        }
        let before = common::tree_checksum(env.root());
        let out = env.run(&["sync", "--json", "--dry-run"]);
        records(&out, 1)?;
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("embedded Git repository"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(common::tree_checksum(env.root()), before);
    }
    Ok(())
}

#[test]
fn final_index_review_refuses_a_symlink_displacing_a_parent() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".p/child", b"original");
    env.ok(&["git", "add", "-A"]);
    env.ok(&["git", "commit", "-m", "base"]);
    let outside = env.root().join("outside");
    fs::rename(env.repo().join(".p"), &outside)?;
    std::os::unix::fs::symlink(&outside, env.repo().join(".p"))?;
    let before = env.four_trees();
    let index = fs::read(env.repo().join(".git/index"))?;
    let objects = common::tree_checksum(&env.repo().join(".git/objects"));
    records(&env.run(&["sync", "--json", "--dry-run"]), 1)?;
    assert_eq!(env.four_trees(), before);
    assert_eq!(fs::read(env.repo().join(".git/index"))?, index);
    assert_eq!(
        common::tree_checksum(&env.repo().join(".git/objects")),
        objects
    );
    assert_eq!(fs::read(outside.join("child"))?, b"original");
    Ok(())
}

#[test]
fn post_capture_review_matches_encrypted_tightening_and_hook_execution() -> R {
    if !common::age_installed() {
        return Ok(());
    }
    for remote in [false, true] {
        for loosened in [false, true] {
            for changed in [false, true] {
                let env = TestEnv::new();
                env.ok(&["init"]);
                if remote {
                    env.ok(&["init", "--remote", &env.remote_url()]);
                }
                env.ok(&["key", "init"]);
                env.write(".secret", b"M2-private-original", 0o600);
                env.ok(&["add", "--encrypt", ".secret"]);
                let mut config = env.repo_config();
                config
                    .hooks
                    .insert(".secret".to_owned(), "echo tightened >> marker".to_owned());
                config.save(&env.repo().join(".dotman.toml"), false)?;
                env.ok(&["sync", "--approve-hooks"]);
                assert!(!env.home().join("marker").exists());
                let bytes = if changed {
                    b"M2-private-rotated".as_slice()
                } else {
                    b"M2-private-original".as_slice()
                };
                env.write(".secret", bytes, if loosened { 0o644 } else { 0o600 });
                let before = env.four_trees();
                let state = fs::read(env.state_dir().join("state.toml"))?;
                let index = fs::read(env.repo().join(".git/index"))?;
                let objects = common::tree_checksum(&env.repo().join(".git/objects"));
                let report = records(&env.run(&["sync", "--json", "--dry-run"]), 0)?;
                assert_eq!(env.four_trees(), before);
                assert_eq!(fs::read(env.state_dir().join("state.toml"))?, state);
                assert_eq!(fs::read(env.repo().join(".git/index"))?, index);
                assert_eq!(
                    common::tree_checksum(&env.repo().join(".git/objects")),
                    objects
                );
                assert!(!env.home().join("marker").exists());
                let writes: BTreeSet<_> = report
                    .iter()
                    .filter(|record| {
                        record["record"] == "resolved" && record["effect"] == "written"
                    })
                    .filter_map(|record| at(record, "/key/raw").as_str())
                    .collect();
                let expected: BTreeSet<_> = if loosened {
                    std::iter::once(".secret").collect()
                } else {
                    BTreeSet::new()
                };
                assert_eq!(
                    writes, expected,
                    "remote={remote}, loosened={loosened}, changed={changed}"
                );
                assert_eq!(
                    find(&report, "hooks")?["commands"]
                        .as_array()
                        .ok_or("commands")?
                        .len(),
                    usize::from(loosened)
                );
                assert!(!serde_json::to_string(&report)?.contains("M2-private"));
                let actual = records(&env.run(&["sync", "--json"]), 0)?;
                let ran = actual
                    .iter()
                    .filter(|record| record["record"] == "hook" && record["outcome"] == "succeeded")
                    .count();
                assert_eq!(ran, usize::from(loosened));
                assert_eq!(common::mode_of(&env.home().join(".secret")), 0o600);
                assert_eq!(env.read(".secret"), bytes);
                if loosened {
                    assert_eq!(env.read("marker"), b"tightened\n");
                }
                let all = env.git_raw(&[
                    "-C",
                    env.repo().to_str().ok_or("repo")?,
                    "cat-file",
                    "--batch-all-objects",
                    "--batch",
                ]);
                assert!(all.status.success());
                assert!(
                    !all.stdout
                        .windows(b"M2-private".len())
                        .any(|window| window == b"M2-private")
                );
            }
        }
    }
    Ok(())
}
