mod common;

use common::TestEnv;
use serde_json::{Value, json};
use std::fs;

type R<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn at<'a>(value: &'a Value, pointer: &str) -> &'a Value {
    value.pointer(pointer).unwrap_or(&Value::Null)
}

fn put(value: &mut Value, pointer: &str, replacement: Value) -> R {
    let (parent, key) = pointer.rsplit_once('/').ok_or("JSON pointer")?;
    value
        .pointer_mut(parent)
        .and_then(Value::as_object_mut)
        .ok_or("JSON object")?
        .insert(key.to_owned(), replacement);
    Ok(())
}

fn reports(out: &std::process::Output) -> R<Vec<Value>> {
    std::str::from_utf8(&out.stdout)?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

fn record<'a>(reports: &'a [Value], kind: &str) -> R<&'a Value> {
    reports
        .iter()
        .find(|record| record["record"] == kind)
        .ok_or_else(|| format!("missing {kind}").into())
}

fn path(value: &Value) -> Value {
    if value["raw"].is_string() {
        json!({"raw": value["raw"]})
    } else {
        json!({"bytes": value["bytes"]})
    }
}

fn input(env: &TestEnv) -> R<Value> {
    let status = reports(&env.run(&["status", "--json"]))?;
    let context = record(&status, "context")?;
    let mut machine = serde_json::Map::new();
    for field in ["host", "user", "os", "distro", "like", "arch"] {
        machine.insert(
            field.to_owned(),
            at(context, &format!("/machine/{field}/raw")).clone(),
        );
    }
    machine.insert(
        "profiles".to_owned(),
        Value::Array(
            at(context, "/machine/profiles")
                .as_array()
                .ok_or("profiles")?
                .iter()
                .map(|value| value["raw"].clone())
                .collect(),
        ),
    );
    Ok(json!({
        "schema_version": 1,
        "repo_id": at(context, "/repo_id/raw"),
        "home": path(&context["home"]),
        "repo": path(&context["repo"]),
        "machine": machine,
        "files": [],
        "approvals": {}
    }))
}

fn file(env: &TestEnv, key: &str, action: &str) -> R<Value> {
    let status = reports(&env.run(&["status", "--json"]))?;
    let entry = status
        .iter()
        .find(|entry| entry["record"] == "entry" && at(entry, "/key/raw") == key)
        .ok_or("entry")?;
    Ok(json!({
        "key": key,
        "source": at(entry, "/source/raw"),
        "r": entry["r"],
        "source_tuple": entry["source_tuple"],
        "h": entry["h"],
        "raw_h": entry["raw_h"],
        "action": action
    }))
}

fn decided(env: &TestEnv, input: &Value, command: &str, extra: &[&str]) -> std::process::Output {
    let mut args = vec![command, "--decide-from", "-", "--json"];
    args.extend_from_slice(extra);
    env.run_stdin(&[], input.to_string().as_bytes(), &args)
}

fn sync_input(env: &TestEnv) -> R<Value> {
    sync_input_with(env, &[])
}

fn sync_input_with(env: &TestEnv, flags: &[&str]) -> R<Value> {
    let mut input = input(env)?;
    let mut args = vec!["sync", "--dry-run", "--json"];
    args.extend_from_slice(flags);
    let review = reports(&env.run(&args))?;
    let outgoing = record(&review, "outgoing")?;
    put(
        &mut input,
        "/sync",
        json!({
            "head": at(outgoing, "/head/raw"),
            "tracking_ref": at(outgoing, "/tracking_ref/raw"),
            "captures": review.iter().filter(|record| record["record"] == "capture").map(|record| json!({
                "key": at(record, "/key/raw"),
                "source": at(record, "/source/raw"),
                "logical_r": record["logical_r"],
                "raw_h": record["raw_h"],
                "stored_before": record["stored_before"]
            })).collect::<Vec<_>>(),
            "outgoing": review.iter().filter(|record| matches!(record["record"].as_str(), Some("repo_edit" | "own"))).map(|record| json!({
                "path": path(&record["path"]),
                "stored": record["stored"]
            })).collect::<Vec<_>>()
        }),
    )?;
    Ok(input)
}

#[test]
fn decided_sync_preserves_unreviewed_index_and_only_publishes_matching_captures() -> R {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".a", b"original a", 0o600);
    env.write(".b", b"original b", 0o600);
    env.ok(&["add", ".a", ".b"]);
    env.ok(&["sync"]);
    env.write(".a", b"reviewed a", 0o600);
    env.write(".b", b"reviewed b", 0o600);
    let reviewed = sync_input(&env)?;
    env.write(".b", b"later b", 0o600);
    env.write_repo(".unlisted", b"unreviewed staged");
    env.ok(&["git", "add", ".unlisted"]);
    let staged = env.git(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "ls-files",
        "-s",
        "--",
        ".unlisted",
    ]);
    let dry = env.four_trees();
    let out = decided(&env, &reviewed, "sync", &["--dry-run"]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(env.four_trees(), dry);
    let out = decided(&env, &reviewed, "sync", &[]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let remote = env.remote_path();
    assert_eq!(
        env.git(&[
            "--git-dir",
            remote.to_str().ok_or("remote")?,
            "show",
            "main:.a"
        ]),
        "reviewed a"
    );
    assert_eq!(
        env.git(&[
            "--git-dir",
            remote.to_str().ok_or("remote")?,
            "show",
            "main:.b"
        ]),
        "original b"
    );
    assert!(
        !env.git_raw(&[
            "--git-dir",
            remote.to_str().ok_or("remote")?,
            "show",
            "main:.unlisted"
        ])
        .status
        .success()
    );
    assert_eq!(
        env.git(&[
            "-C",
            env.repo().to_str().ok_or("repo")?,
            "ls-files",
            "-s",
            "--",
            ".unlisted"
        ]),
        staged
    );
    assert_eq!(env.read(".b"), b"later b");
    assert_eq!(
        fs::read(env.repo().join(".unlisted"))?,
        b"unreviewed staged"
    );
    Ok(())
}

fn malformed_inputs(env: &TestEnv) -> R<Vec<Vec<u8>>> {
    let base = input(env)?;
    let encoded = base.to_string();
    let mut wrong_version = base.clone();
    put(&mut wrong_version, "/schema_version", json!(2))?;
    let mut unknown = base.clone();
    put(&mut unknown, "/machine/extra", json!(true))?;
    let mut bad_mode = base.clone();
    let mut answer = file(env, ".a", "take_repo")?;
    put(&mut answer, "/r/tuple/mode", json!("644"))?;
    put(&mut bad_mode, "/files", json!([answer]))?;
    let mut duplicate = base.clone();
    let answer = file(env, ".a", "take_repo")?;
    put(&mut duplicate, "/files", json!([answer.clone(), answer]))?;
    let mut inputs = vec![
        b"{".to_vec(),
        format!("{encoded} {{}}").into_bytes(),
        encoded
            .replacen(
                "\"schema_version\":1",
                "\"schema_version\":1,\"schema_version\":1",
                1,
            )
            .into_bytes(),
        wrong_version.to_string().into_bytes(),
        unknown.to_string().into_bytes(),
        bad_mode.to_string().into_bytes(),
        duplicate.to_string().into_bytes(),
    ];
    for (pointer, replacement) in [
        ("/home", json!({"raw": "/", "bytes": null})),
        ("/home", json!({"raw": null})),
        ("/home", json!({"raw": "/", "shown": "/"})),
        ("/repo", json!({"bytes": [0]})),
        (
            "/approvals/hooks",
            json!({"canonical": "{}", "commands": [], "pending_keys": [], "extra": true}),
        ),
        (
            "/approvals/identity",
            json!({"recorded": null, "current": at(&base, "/machine"), "extra": true}),
        ),
    ] {
        let mut invalid = base.clone();
        put(&mut invalid, pointer, replacement)?;
        inputs.push(invalid.to_string().into_bytes());
    }
    let sync = sync_input(env)?;
    for outgoing in [
        json!({"path": {"raw": "a//b"}, "stored": {"state": "missing"}}),
        json!({"path": {"raw": "a/./b"}, "stored": {"state": "missing"}}),
        json!({"path": {"raw": "../b"}, "stored": {"state": "missing"}}),
        json!({"path": {"raw": ".git/config"}, "stored": {"state": "missing"}}),
        json!({"path": {"bytes": [256]}, "stored": {"state": "missing"}}),
        json!({"path": {"raw": ".a"}, "stored": {"state": "missing", "tuple": {}}}),
    ] {
        let mut invalid = sync.clone();
        put(&mut invalid, "/sync/outgoing", json!([outgoing]))?;
        inputs.push(invalid.to_string().into_bytes());
    }
    let answer = file(env, ".a", "take_repo")?;
    for replacement in ["\"raw\":\"/\",\"raw\":\"/\"", "\"raw\":null,\"raw\":\"/\""] {
        inputs.push(
            encoded
                .replace(
                    &format!("\"home\":{}", at(&base, "/home")),
                    &format!("\"home\":{{{replacement}}}"),
                )
                .into_bytes(),
        );
    }
    let mut with_file = base;
    put(&mut with_file, "/files", json!([answer]))?;
    let encoded_file = with_file.to_string();
    inputs.push(
        encoded_file
            .replacen(
                "\"action\":\"take_repo\"",
                "\"action\":\"take_repo\",\"action\":\"take_repo\"",
                1,
            )
            .into_bytes(),
    );
    inputs.push(
        encoded_file
            .replacen(
                "\"mode\":\"0644\"",
                "\"mode\":\"0644\",\"mode\":\"0644\"",
                1,
            )
            .into_bytes(),
    );
    Ok(inputs)
}

#[test]
fn malformed_decisions_are_rejected_before_children_or_writes() -> R {
    use std::os::unix::fs::PermissionsExt as _;
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".a", b"reviewed");
    env.ok(&["sync"]);
    let inputs = malformed_inputs(&env)?;
    let bin = env.root().join("spy-bin");
    fs::create_dir(&bin)?;
    let marker = env.root().join("child-started");
    fs::write(
        bin.join("git"),
        format!(
            "#!/bin/sh\nprintf child > '{}'\nexit 97\n",
            marker.display()
        ),
    )?;
    fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o755))?;
    let before = common::tree_checksum(env.root());
    for bytes in inputs {
        let out = env.run_stdin(
            &[("PATH", bin.to_str().ok_or("spy path")?)],
            &bytes,
            &["apply", "--json", "--decide-from", "-"],
        );
        assert_eq!(
            out.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(record(&reports(&out)?, "result")?["exit_code"], 1);
        assert!(!marker.exists());
        assert_eq!(common::tree_checksum(env.root()), before);
    }
    Ok(())
}

#[test]
fn file_decisions_round_trip_status_and_stale_items_do_not_write() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".a", b"repo a");
    env.write_repo(".b", b"repo b");
    env.ok(&["sync"]);
    env.write(".a", b"local a", 0o600);
    env.write(".b", b"local b", 0o600);
    env.write_repo(".a", b"reviewed a");
    env.write_repo(".b", b"reviewed b");
    let mut reviewed = input(&env)?;
    put(
        &mut reviewed,
        "/files",
        json!([
            file(&env, ".a", "take_repo")?,
            file(&env, ".b", "take_repo")?
        ]),
    )?;
    env.write(".b", b"later b", 0o600);
    let dry = env.four_trees();
    let out = decided(&env, &reviewed, "apply", &["--dry-run"]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(env.four_trees(), dry);
    let out = decided(&env, &reviewed, "apply", &[]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(env.read(".a"), b"reviewed a");
    assert_eq!(env.read(".b"), b"later b");
    Ok(())
}

#[test]
fn late_unmanaged_system_and_own_keys_reject_the_whole_batch_even_outside_filters() -> R {
    for key in [".unmanaged", "/synthetic-system", ".dotman.toml"] {
        let env = TestEnv::new();
        env.ok(&["init"]);
        env.write_repo(".a", b"base");
        env.ok(&["sync"]);
        env.write_repo(".a", b"repo edit");
        let mut reviewed = input(&env)?;
        let first = file(&env, ".a", "take_repo")?;
        let mut last = first.clone();
        put(&mut last, "/key", json!(key))?;
        put(&mut reviewed, "/files", json!([first, last]))?;
        let before = common::tree_checksum(env.root());
        let out = decided(&env, &reviewed, "apply", &[".a"]);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(common::tree_checksum(env.root()), before);
        assert_eq!(env.read(".a"), b"base");
    }
    Ok(())
}

#[test]
fn decide_from_conflicts_with_every_bare_approval_and_accepts_only_stdin() {
    let env = TestEnv::new();
    for command in ["apply", "sync"] {
        let mut flags = vec![
            "--keep-local",
            "--take-repo",
            "--forget-deleted",
            "--restore-deleted",
            "--accept-identity",
            "--approve-hooks",
            "--system",
            "--yes",
        ];
        if command == "sync" {
            flags.extend([
                "--approve-dirs",
                "--drop-unlisted-ignores",
                "--allow-mass-delete",
            ]);
        }
        for flag in flags {
            let out = env.run(&[command, "--decide-from", "-", flag]);
            assert_eq!(out.status.code(), Some(2), "{command} {flag}");
        }
        assert_eq!(
            env.run(&[command, "--decide-from", "decisions.json"])
                .status
                .code(),
            Some(2)
        );
    }
}

#[test]
fn stale_repo_mode_and_raw_symlink_bindings_do_not_authorize_writes() -> R {
    use std::os::unix::fs::PermissionsExt as _;
    for change in ["repo", "mode", "raw_link"] {
        let env = TestEnv::new();
        env.ok(&["init"]);
        env.write(".a", b"base", 0o600);
        env.ok(&["add", ".a"]);
        env.write("base", b"link base", 0o600);
        env.write("repo", b"link repo", 0o600);
        env.symlink(".link", "base");
        env.ok(&["add", ".link"]);
        env.ok(&["sync"]);
        let key = if change == "raw_link" {
            fs::remove_file(env.repo().join(".link"))?;
            std::os::unix::fs::symlink("repo", env.repo().join(".link"))?;
            fs::remove_file(env.home().join(".link"))?;
            std::os::unix::fs::symlink(env.home().join("base"), env.home().join(".link"))?;
            ".link"
        } else {
            env.write_repo(".a", b"reviewed repo");
            ".a"
        };
        let mut reviewed = input(&env)?;
        put(
            &mut reviewed,
            "/files",
            json!([file(&env, key, "take_repo")?]),
        )?;
        match change {
            "repo" => env.write_repo(".a", b"later repo"),
            "mode" => {
                fs::set_permissions(env.home().join(".a"), fs::Permissions::from_mode(0o640))?;
            }
            _ => {
                fs::remove_file(env.home().join(".link"))?;
                std::os::unix::fs::symlink("base", env.home().join(".link"))?;
            }
        }
        let before = env.four_trees();
        let out = decided(&env, &reviewed, "apply", &[]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(env.four_trees()[0], before[0]);
        assert_eq!(env.four_trees()[1], before[1]);
    }
    Ok(())
}

#[test]
fn template_drift_requires_its_current_reviewed_tuple() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".tpl", b"base", 0o600);
    env.ok(&["add", "--template", ".tpl"]);
    env.ok(&["sync"]);
    env.write(".tpl", b"drift", 0o600);
    let mut reviewed = input(&env)?;
    put(
        &mut reviewed,
        "/files",
        json!([file(&env, ".tpl", "take_repo")?]),
    )?;
    env.write_repo(".tpl{tmpl}", b"new template");
    let out = decided(&env, &reviewed, "apply", &[]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(env.read(".tpl"), b"drift");
    let mut refreshed = input(&env)?;
    put(
        &mut refreshed,
        "/files",
        json!([file(&env, ".tpl", "take_repo")?]),
    )?;
    let out = decided(&env, &refreshed, "apply", &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(env.read(".tpl"), b"new template");
    Ok(())
}

fn hook_approval(env: &TestEnv) -> R<Value> {
    let review = reports(&env.run(&["sync", "--dry-run", "--json"]))?;
    let hooks = record(&review, "hooks")?;
    Ok(json!({
        "canonical": at(hooks, "/canonical/raw"),
        "commands": hooks["commands"].as_array().ok_or("commands")?.iter().map(|command| command["raw"].clone()).collect::<Vec<_>>(),
        "pending_keys": hooks["pending_keys"].as_array().ok_or("pending keys")?.iter().map(|key| key["raw"].clone()).collect::<Vec<_>>()
    }))
}

#[test]
fn stale_hook_tokens_cannot_fall_through_to_standing_approval() -> R {
    for changed in ["canonical", "commands", "pending"] {
        let env = TestEnv::new();
        env.ok(&["init"]);
        env.write_repo(".a", b"base");
        env.write_repo(".b", b"base");
        env.ok(&["sync"]);
        let mut config = env.repo_config();
        config
            .hooks
            .insert(".*".to_owned(), "echo ran >> marker".to_owned());
        config.save(&env.repo().join(".dotman.toml"), false)?;
        env.ok(&["apply", "--approve-hooks"]);
        assert!(!env.home().join("marker").exists());
        env.write_repo(".a", b"reviewed");
        let mut reviewed = input(&env)?;
        put(
            &mut reviewed,
            "/files",
            json!([file(&env, ".a", "take_repo")?]),
        )?;
        put(&mut reviewed, "/approvals/hooks", hook_approval(&env)?)?;
        match changed {
            "canonical" => put(&mut reviewed, "/approvals/hooks/canonical", json!("{}"))?,
            "commands" => put(
                &mut reviewed,
                "/approvals/hooks/commands",
                json!(["echo other"]),
            )?,
            _ => put(
                &mut reviewed,
                "/approvals/hooks/pending_keys",
                json!([".b"]),
            )?,
        }
        let state_file = env.state_dir().join("state.toml");
        let before: dotman::state::State = toml::from_str(&fs::read_to_string(&state_file)?)?;
        let out = decided(&env, &reviewed, "apply", &[]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(env.read(".a"), b"reviewed");
        assert!(!env.home().join("marker").exists());
        let after: dotman::state::State = toml::from_str(&fs::read_to_string(&state_file)?)?;
        assert_eq!(after.hooks.approved, before.hooks.approved);
        assert!(after.hooks.pending.contains(".a"));
        let mut refreshed = input(&env)?;
        put(&mut refreshed, "/approvals/hooks", hook_approval(&env)?)?;
        let out = decided(&env, &refreshed, "apply", &[]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(env.read("marker"), b"ran\n");
    }
    Ok(())
}

fn encryption_pair() -> (TestEnv, TestEnv) {
    let a = TestEnv::new();
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.ok(&["key", "init"]);
    a.write(".one", b"review-private-one", 0o600);
    a.write(".two", b"review-private-two", 0o600);
    a.ok(&["add", "--encrypt", ".one", ".two"]);
    a.ok(&["sync"]);
    let b = TestEnv::new();
    b.ok(&["init", &a.remote_url()]);
    b.ok(&["key", "init"]);
    (a, b)
}

#[test]
fn decided_reencryption_stages_only_rewritten_sources_and_record() -> R {
    if !common::age_installed() {
        return Ok(());
    }
    for staged in [false, true] {
        let (a, b) = encryption_pair();
        let reviewed = sync_input(&a)?;
        b.ok(&["sync"]);
        a.write_repo(".unlisted", b"unreviewed-repo-edit");
        if staged {
            a.ok(&["git", "add", ".unlisted"]);
        }
        let repo = a.repo();
        let repo_path = repo.to_str().ok_or("repo")?;
        let index = a.git(&["-C", repo_path, "ls-files", "-s", "--", ".unlisted"]);
        let out = decided(&a, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            a.git(&["-C", repo_path, "ls-files", "-s", "--", ".unlisted"]),
            index
        );
        assert_eq!(fs::read(repo.join(".unlisted"))?, b"unreviewed-repo-edit");
        let remote = a.remote_path();
        let remote_path = remote.to_str().ok_or("remote")?;
        let changed = a.git(&[
            "--git-dir",
            remote_path,
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "main",
        ]);
        let paths: std::collections::BTreeSet<_> = changed.lines().collect();
        assert_eq!(
            paths,
            [".dotman/age-recipients", ".one{age}", ".two{age}"]
                .into_iter()
                .collect()
        );
        for source in [".one{age}", ".two{age}", ".dotman/age-recipients"] {
            let blob = a.git_raw(&["--git-dir", remote_path, "show", &format!("main:{source}")]);
            assert!(blob.status.success());
            assert_eq!(blob.stdout, fs::read(repo.join(source))?);
        }
        let objects = a.git_raw(&[
            "--git-dir",
            remote_path,
            "cat-file",
            "--batch-all-objects",
            "--batch",
        ]);
        assert!(objects.status.success());
        for marker in [
            b"unreviewed-repo-edit".as_slice(),
            b"review-private".as_slice(),
        ] {
            assert!(
                !objects
                    .stdout
                    .windows(marker.len())
                    .any(|bytes| bytes == marker)
            );
        }
        assert!(!String::from_utf8_lossy(&out.stdout).contains("review-private"));
        assert_eq!(a.read(".one"), b"review-private-one");
        assert_eq!(a.read(".two"), b"review-private-two");
    }
    Ok(())
}

#[test]
fn dirty_age_sources_or_recipient_metadata_hold_all_decided_reencryption() -> R {
    if !common::age_installed() {
        return Ok(());
    }
    for dirty in ["age", "record", "config"] {
        let (a, b) = encryption_pair();
        let reviewed = sync_input(&a)?;
        b.ok(&["sync"]);
        match dirty {
            "age" => {
                fs::copy(a.repo().join(".two{age}"), a.repo().join(".one{age}"))?;
            }
            "record" => a.write_repo(".dotman/age-recipients", b"unreviewed-recipient\n"),
            _ => {
                let mut config = a.repo_config();
                config.age.recipients.clear();
                config.save(&a.repo().join(".dotman.toml"), false)?;
            }
        }
        let before = [".one{age}", ".two{age}", ".dotman/age-recipients"]
            .map(|path| fs::read(a.repo().join(path)))
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;
        let out = decided(&a, &reviewed, "sync", &[]);
        assert!(
            matches!(out.status.code(), Some(3 | 4)),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        for (path, expected) in [".one{age}", ".two{age}", ".dotman/age-recipients"]
            .iter()
            .zip(before)
        {
            assert_eq!(fs::read(a.repo().join(path))?, expected, "{dirty}: {path}");
        }
        let remote = a.remote_path();
        let remote_path = remote.to_str().ok_or("remote")?;
        for source in [".one{age}", ".two{age}", ".dotman/age-recipients"] {
            let on_remote =
                a.git_raw(&["--git-dir", remote_path, "show", &format!("main:{source}")]);
            let on_b = b.git_raw(&[
                "-C",
                b.repo().to_str().ok_or("b repo")?,
                "show",
                &format!("HEAD:{source}"),
            ]);
            assert!(on_remote.status.success() && on_b.status.success());
            assert_eq!(on_remote.stdout, on_b.stdout);
        }
        assert!(!String::from_utf8_lossy(&out.stdout).contains("review-private"));
    }
    Ok(())
}

#[test]
fn reviewed_head_and_branch_changes_stop_before_mutation() -> R {
    for moved in ["commit", "unborn", "branch"] {
        let env = TestEnv::new();
        env.ok(&["init", "--remote", &env.remote_url()]);
        if moved != "unborn" {
            env.write_repo(".a", b"base");
            env.ok(&["sync"]);
        }
        let reviewed = sync_input(&env)?;
        if moved == "branch" {
            env.ok(&["git", "checkout", "-b", "different"]);
        } else {
            env.write_repo(".later", b"unreviewed commit");
            env.ok(&["git", "add", "-A"]);
            env.ok(&["git", "commit", "-m", "unreviewed"]);
        }
        let before = common::tree_checksum(env.root());
        let out = decided(&env, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(common::tree_checksum(env.root()), before);
    }
    Ok(())
}

#[test]
fn decided_first_commit_can_review_an_unborn_head_and_own_files() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let reviewed = sync_input(&env)?;
    assert!(at(&reviewed, "/sync/head").is_null());
    let out = decided(&env, &reviewed, "sync", &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let repo = env.repo();
    let entries = env.git(&[
        "-C",
        repo.to_str().ok_or("repo")?,
        "ls-tree",
        "-r",
        "--name-only",
        "HEAD",
    ]);
    assert!(entries.lines().any(|path| path == ".dotman.toml"));
    for entry in at(&reviewed, "/sync/outgoing")
        .as_array()
        .ok_or("outgoing")?
    {
        let name = at(entry, "/path/raw").as_str().ok_or("path")?;
        let blob = env.git_raw(&[
            "-C",
            repo.to_str().ok_or("repo")?,
            "show",
            &format!("HEAD:{name}"),
        ]);
        assert!(blob.status.success());
        assert_eq!(blob.stdout, fs::read(repo.join(name))?);
    }
    Ok(())
}

#[test]
fn push_publishes_the_pinned_reviewed_tip_not_a_raced_branch_head() -> R {
    let mut env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".a", b"base", 0o600);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    env.write(".a", b"reviewed", 0o600);
    let reviewed = sync_input(&env)?;
    let real_git = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .ok_or("git")?;
    let wrapper = env.root().join("wrapper");
    fs::create_dir(&wrapper)?;
    let armed = env.root().join("armed");
    fs::write(&armed, b"")?;
    let repo = env.repo();
    common::write_exe(
        &wrapper.join("git"),
        &format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\nif [ \"$arg\" = push ] && [ -f {armed} ]; then\nrm -- {armed}\nprintf 'raced-branch-edit' > {target}\n{git} -C {repo} -c core.hooksPath=/dev/null add -- .race || exit\n{git} -C {repo} -c core.hooksPath=/dev/null commit -qm raced || exit\nfi\ndone\nexec {git} \"$@\"\n",
            armed = shell_words::quote(armed.to_str().ok_or("armed")?),
            target = shell_words::quote(repo.join(".race").to_str().ok_or("target")?),
            git = shell_words::quote(real_git.to_str().ok_or("git")?),
            repo = shell_words::quote(repo.to_str().ok_or("repo")?),
        ),
    )?;
    env.prepend_path(wrapper);
    let out = decided(&env, &reviewed, "sync", &[]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!armed.exists());
    let remote = env.remote_path();
    assert_eq!(
        env.git(&[
            "--git-dir",
            remote.to_str().ok_or("remote")?,
            "show",
            "main:.a"
        ]),
        "reviewed"
    );
    assert!(
        !env.git_raw(&[
            "--git-dir",
            remote.to_str().ok_or("remote")?,
            "show",
            "main:.race"
        ])
        .status
        .success()
    );
    assert_eq!(fs::read(repo.join(".race"))?, b"raced-branch-edit");
    Ok(())
}

fn saved_state(env: &TestEnv) -> R<dotman::state::State> {
    Ok(toml::from_str(&fs::read_to_string(
        env.state_dir().join("state.toml"),
    )?)?)
}

#[test]
fn directory_and_ignore_approvals_bind_complete_sets() -> R {
    for stale in [false, true] {
        let env = TestEnv::new();
        env.ok(&["init"]);
        let mut config = env.repo_config();
        config.ignore.patterns = vec![".old-ignore".to_owned()];
        config.save(&env.repo().join(".dotman.toml"), false)?;
        env.ok(&["sync"]);
        config.ignore.patterns = vec![".current-ignore".to_owned()];
        config.dirs.paths = vec![".new-dir".to_owned()];
        config.save(&env.repo().join(".dotman.toml"), false)?;
        let mut reviewed = sync_input(&env)?;
        put(&mut reviewed, "/approvals/directories", json!([".new-dir"]))?;
        put(
            &mut reviewed,
            "/approvals/ignores",
            json!({
                "retained": [".old-ignore"], "current": [".current-ignore"]
            }),
        )?;
        if stale {
            put(
                &mut reviewed,
                "/approvals/directories",
                json!([".new-dir", ".extra"]),
            )?;
            put(
                &mut reviewed,
                "/approvals/ignores/current",
                json!([".different"]),
            )?;
        }
        let before = env.four_trees();
        let out = decided(&env, &reviewed, "sync", &["--dry-run"]);
        assert_eq!(
            out.status.code(),
            Some(if stale { 3 } else { 0 }),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(env.four_trees(), before);
        let out = decided(&env, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(if stale { 3 } else { 0 }),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let state = saved_state(&env)?;
        assert_eq!(
            state
                .capture_dirs
                .as_ref()
                .is_some_and(|dirs| dirs.contains(".new-dir")),
            !stale
        );
        assert_eq!(state.ignore_seen.contains(".old-ignore"), stale);
    }
    Ok(())
}

#[test]
fn mass_delete_approval_binds_the_exact_set_and_refusal_keeps_state() -> R {
    for token in ["missing", "stale", "exact"] {
        let env = TestEnv::new();
        env.ok(&["init"]);
        for n in 0..12 {
            env.write_repo(&format!(".f{n}"), b"base");
        }
        env.ok(&["sync"]);
        for n in 0..10 {
            fs::remove_file(env.repo().join(format!(".f{n}")))?;
        }
        let mut reviewed = sync_input(&env)?;
        if token != "missing" {
            let count = if token == "stale" { 9 } else { 10 };
            put(
                &mut reviewed,
                "/approvals/mass_delete",
                json!(
                    (0..count)
                        .map(|n| json!({"raw": format!(".f{n}")}))
                        .collect::<Vec<_>>()
                ),
            )?;
        }
        let before = saved_state(&env)?;
        let out = decided(&env, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(if token == "exact" { 0 } else { 3 }),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let after = saved_state(&env)?;
        for n in 0..10 {
            let key = format!(".f{n}");
            assert_eq!(
                after.files.contains_key(&key),
                token != "exact",
                "{token}: {key}"
            );
            assert_eq!(env.read(&key), b"base");
            if token != "exact" {
                assert_eq!(after.files.get(&key), before.files.get(&key));
                assert!(
                    env.git_raw(&[
                        "-C",
                        env.repo().to_str().ok_or("repo")?,
                        "show",
                        &format!("HEAD:{key}")
                    ])
                    .status
                    .success()
                );
            }
        }
    }
    Ok(())
}

#[test]
fn reviewed_commit_preserves_staged_only_edits_deletions_and_renames() -> R {
    for staged in ["edit", "delete", "rename"] {
        let env = TestEnv::new();
        env.ok(&["init"]);
        env.write_repo(".a", b"a");
        env.write_repo(".b", b"b");
        env.ok(&["sync"]);
        env.write_repo(".a", b"reviewed");
        let reviewed = sync_input(&env)?;
        match staged {
            "edit" => {
                env.write_repo(".b", b"staged-only");
                env.ok(&["git", "add", "--", ".b"]);
            }
            "delete" => {
                env.ok(&["git", "rm", "--", ".b"]);
            }
            _ => {
                env.ok(&["git", "mv", "--", ".b", ".renamed"]);
            }
        }
        env.write_repo(".b", b"b");
        let repo = env.repo();
        let repo = repo.to_str().ok_or("repo")?;
        let before = env
            .git_raw(&["-C", repo, "ls-files", "-s", "-z", "--", ".b", ".renamed"])
            .stdout;
        let out = decided(&env, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            env.git_raw(&["-C", repo, "ls-files", "-s", "-z", "--", ".b", ".renamed"])
                .stdout,
            before
        );
        assert_eq!(env.git(&["-C", repo, "show", "HEAD:.a"]), "reviewed");
        assert_eq!(env.git(&["-C", repo, "show", "HEAD:.b"]), "b");
        assert!(
            !env.git_raw(&["-C", repo, "show", "HEAD:.renamed"])
                .status
                .success()
        );
        assert_eq!(env.read(".a"), b"reviewed");
        assert_eq!(fs::read(env.repo().join(".b"))?, b"b");
    }
    Ok(())
}

#[test]
fn canceled_outgoing_edit_is_stale_and_non_utf8_outgoing_paths_remain_literal() -> R {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".a", b"base");
    let name = OsString::from_vec(b"source-\xff".to_vec());
    fs::write(env.repo().join(&name), b"byte-base")?;
    env.ok(&["git", "add", "-A"]);
    env.ok(&["sync"]);
    env.write_repo(".a", b"reviewed");
    fs::write(env.repo().join(&name), b"byte-path")?;
    let reviewed = sync_input(&env)?;
    env.write_repo(".a", b"base");
    let out = decided(&env, &reviewed, "sync", &[]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        env.git(&["-C", env.repo().to_str().ok_or("repo")?, "show", "HEAD:.a"]),
        "base"
    );
    assert_eq!(env.read(".a"), b"base");
    let listed = env.git_raw(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "ls-tree",
        "-rz",
        "--name-only",
        "HEAD",
    ]);
    assert!(
        listed
            .stdout
            .split(|byte| *byte == 0)
            .any(|path| path == b"source-\xff")
    );
    assert_eq!(fs::read(env.repo().join(name))?, b"byte-path");
    Ok(())
}

#[test]
fn identity_approval_and_selected_source_must_match_the_review() -> R {
    for change in ["none", "recorded_identity", "selected_source"] {
        let env = TestEnv::new();
        env.ok(&["init"]);
        env.write_machine_config("[identity]\nhost = \"before\"\n");
        env.write_repo(".a", b"plain");
        env.write_repo(".a{host=after}", b"after");
        env.ok(&["sync"]);
        let previous = saved_state(&env)?.identity;
        env.write_machine_config("[identity]\nhost = \"after\"\n");
        let mut reviewed = input(&env)?;
        put(
            &mut reviewed,
            "/files",
            json!([file(&env, ".a", "take_repo")?]),
        )?;
        let current = at(&reviewed, "/machine").clone();
        put(
            &mut reviewed,
            "/approvals/identity",
            json!({
                "recorded": previous, "current": current
            }),
        )?;
        if change == "recorded_identity" {
            put(
                &mut reviewed,
                "/approvals/identity/recorded/host",
                json!("wrong"),
            )?;
        } else if change == "selected_source" {
            env.write_repo(".a{host=after+os=linux}", b"later source");
        }
        let out = decided(&env, &reviewed, "apply", &[]);
        assert_eq!(
            out.status.code(),
            Some(if change == "none" { 0 } else { 3 }),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            env.read(".a"),
            if change == "none" {
                b"after".as_slice()
            } else {
                b"plain".as_slice()
            }
        );
        if change == "recorded_identity" {
            assert_eq!(
                saved_state(&env)?
                    .identity
                    .as_ref()
                    .map(|id| id.host.as_str()),
                Some("before")
            );
        }
    }
    Ok(())
}

#[test]
fn reviewed_stdin_on_a_terminal_is_not_prompt_input_or_hook_stdin() -> R {
    use std::io::Write as _;
    use std::path::Path;
    use std::process::Stdio;
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".a", b"base");
    env.ok(&["sync"]);
    env.write_repo(".a", b"reviewed");
    let mut config = env.repo_config();
    config.hooks.insert(
        ".a".to_owned(),
        "if test -t 0; then echo tty > marker; else cat > leaked; echo null > marker; fi"
            .to_owned(),
    );
    config.save(&env.repo().join(".dotman.toml"), false)?;
    let mut reviewed = input(&env)?;
    put(
        &mut reviewed,
        "/files",
        json!([file(&env, ".a", "take_repo")?]),
    )?;
    put(&mut reviewed, "/approvals/hooks", hook_approval(&env)?)?;
    let executable = env.dot_process();
    let executable = executable.get_program().to_str().ok_or("binary path")?;
    let line = format!(
        "{} apply --json --decide-from -",
        shell_words::quote(executable)
    );
    let mut child = env
        .process_for(Path::new("timeout"))
        .env("DOTMAN_NON_INTERACTIVE", "")
        .args(["-k", "2s", "10s", "script", "-qec", &line, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("terminal stdin")?
        .write_all(format!("{reviewed}\n\u{4}").as_bytes())?;
    let out = child.wait_with_output()?;
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"exit_code\":0"));
    assert_eq!(env.read(".a"), b"reviewed");
    assert_eq!(env.read("marker"), b"null\n");
    assert_eq!(env.read("leaked"), b"");
    Ok(())
}

#[test]
fn new_directory_captures_round_trip_missing_logical_sources_and_raw_links() -> R {
    for approved in [false, true] {
        let env = TestEnv::new();
        env.ok(&["init"]);
        env.write(".config/app/anchor", b"anchor", 0o600);
        if approved {
            env.ok(&["add", ".config/app"]);
        } else {
            let mut config = env.repo_config();
            config.dirs.paths.push(".config/app".to_owned());
            config.save(&env.repo().join(".dotman.toml"), false)?;
        }
        let baseline = env.run(&["sync"]);
        assert_eq!(baseline.status.code(), Some(if approved { 0 } else { 3 }));
        env.write(".config/app/new", b"new-file", 0o640);
        env.symlink(".config/app/link", env.home().join(".config/app/anchor"));
        let mut reviewed = sync_input_with(&env, &["--approve-dirs"])?;
        if !approved {
            put(
                &mut reviewed,
                "/approvals/directories",
                json!([".config/app"]),
            )?;
        }
        for capture in at(&reviewed, "/sync/captures")
            .as_array()
            .ok_or("captures")?
        {
            assert_eq!(at(capture, "/logical_r/state"), "missing");
            assert_eq!(at(capture, "/stored_before/state"), "missing");
        }
        assert!(
            at(&reviewed, "/sync/captures")
                .as_array()
                .ok_or("captures")?
                .iter()
                .any(|capture| at(capture, "/key") == ".config/app/link")
        );
        let before = env.four_trees();
        let preview = decided(&env, &reviewed, "sync", &["--dry-run"]);
        assert_eq!(
            preview.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&preview.stderr)
        );
        assert_eq!(env.four_trees(), before);
        if approved {
            env.write(".config/app/unlisted", b"unlisted-sibling", 0o600);
        }
        let out = decided(&env, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(if approved { 3 } else { 0 }),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(fs::read(env.repo().join(".config/app/new"))?, b"new-file");
        assert_eq!(common::mode_of(&env.home().join(".config/app/new")), 0o640);
        assert_eq!(
            fs::read_link(env.repo().join(".config/app/link"))?,
            std::path::Path::new("../../.config/app/anchor")
        );
        assert!(saved_state(&env)?.files.contains_key(".config/app/new"));
        assert!(!env.repo().join(".config/app/unlisted").exists());
        assert!(
            !saved_state(&env)?
                .files
                .contains_key(".config/app/unlisted")
        );
        if approved {
            assert_eq!(env.read(".config/app/unlisted"), b"unlisted-sibling");
        }
    }
    Ok(())
}

#[test]
fn new_capture_bindings_reject_unavailable_or_changed_identity_source_and_home() -> R {
    for changed in [
        "unavailable",
        "blocked",
        "home",
        "raw_link",
        "source",
        "variant",
        "identity",
    ] {
        let env = TestEnv::new();
        env.ok(&["init"]);
        env.write(".config/app/anchor", b"anchor", 0o600);
        env.ok(&["add", ".config/app"]);
        env.ok(&["sync"]);
        env.symlink(".config/app/new", env.home().join(".config/app/anchor"));
        let mut reviewed = sync_input(&env)?;
        match changed {
            "unavailable" => put(
                &mut reviewed,
                "/sync/captures/0/logical_r",
                json!({"state": "unavailable", "reason": "not_observed"}),
            )?,
            "blocked" => put(
                &mut reviewed,
                "/sync/captures/0/logical_r",
                json!({"state": "blocked", "reason": "directory"}),
            )?,
            "home" => {
                fs::remove_file(env.home().join(".config/app/new"))?;
                env.write(".config/app/new", b"later", 0o600);
            }
            "raw_link" => {
                fs::remove_file(env.home().join(".config/app/new"))?;
                env.symlink(".config/app/new", "anchor");
            }
            "source" => {
                std::os::unix::fs::symlink("anchor", env.repo().join(".config/app/new"))?;
            }
            "variant" => env.write_repo(".config/app/new{os=linux}", b"late-variant"),
            _ => env.write_machine_config("[identity]\nhost = \"different\"\n"),
        }
        let before = env.four_trees();
        let out = decided(&env, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "{changed}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(env.four_trees()[0], before[0], "{changed}: home");
        assert_eq!(env.four_trees()[1], before[1], "{changed}: repo");
    }
    Ok(())
}

#[test]
fn a_variant_arriving_during_an_earlier_capture_invalidates_the_selected_source() -> R {
    if !common::age_installed() {
        return Ok(());
    }
    for existing in [false, true] {
        let mut env = TestEnv::new();
        env.ok(&["init"]);
        env.ok(&["key", "init"]);
        env.write(".aaa-secret", b"base", 0o600);
        env.ok(&["add", "--encrypt", ".aaa-secret"]);
        env.write(".config/app/anchor", b"anchor", 0o600);
        if existing {
            env.write(".config/app/new", b"base-source", 0o644);
        }
        env.ok(&["add", ".config/app"]);
        env.ok(&["sync"]);
        let recorded = saved_state(&env)?.files.get(".config/app/new").cloned();
        env.write(".aaa-secret", b"reviewed-private", 0o600);
        env.write(".config/app/new", b"reviewed-new", 0o600);
        let reviewed = sync_input(&env)?;
        assert!(
            at(&reviewed, "/sync/outgoing")
                .as_array()
                .ok_or("outgoing")?
                .iter()
                .any(|entry| at(entry, "/stored/reason") == "hypothetical_ciphertext")
        );
        let real_age = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
            .map(|dir| dir.join("age"))
            .find(|path| path.is_file())
            .ok_or("age")?;
        let wrapper = env.root().join("wrapper");
        fs::create_dir(&wrapper)?;
        let armed = env.root().join("armed");
        fs::write(&armed, b"")?;
        common::write_exe(
            &wrapper.join("age"),
            &format!(
                "#!/bin/sh\nfor arg in \"$@\"; do\nif [ \"$arg\" = -e ] && [ -f {armed} ]; then\nrm -- {armed}\nprintf base-source > {variant}\nchmod 0644 {variant}\nfi\ndone\nexec {age} \"$@\"\n",
                armed = shell_words::quote(armed.to_str().ok_or("armed")?),
                variant = shell_words::quote(
                    env.repo()
                        .join(".config/app/new{os=linux}")
                        .to_str()
                        .ok_or("variant")?
                ),
                age = shell_words::quote(real_age.to_str().ok_or("age")?),
            ),
        )?;
        env.prepend_path(wrapper);
        let out = decided(&env, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!armed.exists());
        if existing {
            assert_eq!(
                fs::read(env.repo().join(".config/app/new"))?,
                b"base-source"
            );
        } else {
            assert!(!env.repo().join(".config/app/new").exists());
        }
        assert_eq!(
            saved_state(&env)?.files.get(".config/app/new"),
            recorded.as_ref()
        );
        assert_eq!(env.read(".config/app/new"), b"reviewed-new");
        assert_eq!(records_captured(&out)?, vec![".aaa-secret".to_owned()]);
        assert!(!records_captured(&out)?.contains(&".config/app/new".to_owned()));
        assert!(!String::from_utf8_lossy(&out.stdout).contains("reviewed-private"));
    }
    Ok(())
}

fn records_captured(out: &std::process::Output) -> R<Vec<String>> {
    reports(out)?
        .iter()
        .filter(|entry| {
            entry["record"] == "resolved"
                && entry["effect"] == "captured"
                && entry["hypothetical"] == false
        })
        .map(|entry| {
            at(entry, "/key/raw")
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "capture key".into())
        })
        .collect()
}

#[test]
fn reviewed_encrypted_capture_projects_without_writes_then_commits_its_ciphertext_only() -> R {
    if !common::age_installed() {
        return Ok(());
    }
    let (mut env, _) = encryption_pair();
    env.write(".one", b"reviewed-capture-plaintext", 0o600);
    let reviewed = sync_input(&env)?;
    assert!(
        at(&reviewed, "/sync/outgoing")
            .as_array()
            .ok_or("outgoing")?
            .iter()
            .any(|entry| at(entry, "/stored/reason") == "hypothetical_ciphertext")
    );
    let age_program = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
        .map(|dir| dir.join("age"))
        .find(|path| path.is_file())
        .ok_or("age")?;
    let produced = env.root().join("produced-ciphertext");
    let wrapper = env.root().join("producer-wrapper");
    fs::create_dir(&wrapper)?;
    common::write_exe(
        &wrapper.join("age"),
        &format!(
            "#!/bin/sh\nset -eu\nfor arg in \"$@\"; do\nif [ \"$arg\" = -e ]; then\n{age} \"$@\" > {produced}\ncat -- {produced}\nexit 0\nfi\ndone\nexec {age} \"$@\"\n",
            age = shell_words::quote(age_program.to_str().ok_or("age")?),
            produced = shell_words::quote(produced.to_str().ok_or("produced")?),
        ),
    )?;
    env.prepend_path(wrapper);
    let before = common::tree_checksum(env.root());
    let preview = decided(&env, &reviewed, "sync", &["--dry-run"]);
    assert_eq!(
        preview.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert_eq!(common::tree_checksum(env.root()), before);
    env.write_repo(".unlisted", b"unreviewed-capture-neighbor");
    let out = decided(&env, &reviewed, "sync", &[]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let remote = env.remote_path();
    let remote = remote.to_str().ok_or("remote")?;
    let blob = env.git_raw(&["--git-dir", remote, "show", "main:.one{age}"]);
    assert!(blob.status.success());
    assert_eq!(blob.stdout, fs::read(produced)?);
    assert_eq!(blob.stdout, fs::read(env.repo().join(".one{age}"))?);
    let changed = env.git(&[
        "--git-dir",
        remote,
        "diff-tree",
        "--no-commit-id",
        "--name-only",
        "-r",
        "main",
    ]);
    assert_eq!(changed, ".one{age}\n");
    let objects = env.git_raw(&[
        "--git-dir",
        remote,
        "cat-file",
        "--batch-all-objects",
        "--batch",
    ]);
    for marker in [
        b"reviewed-capture-plaintext".as_slice(),
        b"unreviewed-capture-neighbor".as_slice(),
    ] {
        assert!(
            !objects
                .stdout
                .windows(marker.len())
                .any(|window| window == marker)
        );
        assert!(
            !out.stdout
                .windows(marker.len())
                .any(|window| window == marker)
        );
    }
    assert_eq!(env.read(".one"), b"reviewed-capture-plaintext");
    Ok(())
}

#[test]
fn hypothetical_ciphertext_requires_a_validated_capture_not_an_unavailable_wildcard() -> R {
    if !common::age_installed() {
        return Ok(());
    }
    for stale in [false, true] {
        let (env, _) = encryption_pair();
        env.write(".one", b"reviewed-capture-plaintext", 0o600);
        let mut reviewed = sync_input(&env)?;
        if stale {
            env.write(".one", b"later-capture-plaintext", 0o600);
        } else {
            put(&mut reviewed, "/sync/captures", json!([]))?;
        }
        let repo = fs::read(env.repo().join(".one{age}"))?;
        let before = common::tree_checksum(env.root());
        let preview = decided(&env, &reviewed, "sync", &["--dry-run"]);
        assert_eq!(
            preview.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&preview.stderr)
        );
        assert_eq!(common::tree_checksum(env.root()), before);
        let out = decided(&env, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(fs::read(env.repo().join(".one{age}"))?, repo);
        let remote = env.remote_path();
        let blob = env.git_raw(&[
            "--git-dir",
            remote.to_str().ok_or("remote")?,
            "show",
            "main:.one{age}",
        ]);
        assert_eq!(blob.stdout, repo);
        assert_eq!(
            env.read(".one"),
            if stale {
                b"later-capture-plaintext".as_slice()
            } else {
                b"reviewed-capture-plaintext".as_slice()
            }
        );
    }
    Ok(())
}

fn after_git(env: &mut TestEnv, trigger: &str, body: &str) -> R {
    let git = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .ok_or("git")?;
    let wrapper = env.root().join("git-wrapper");
    fs::create_dir(&wrapper)?;
    let armed = env.root().join("armed");
    fs::write(&armed, b"")?;
    common::write_exe(
        &wrapper.join("git"),
        &format!(
            "#!/bin/sh\nset -eu\ngit={git}\nrepo={repo}\narmed={armed}\nfor arg in \"$@\"; do\nif [ \"$arg\" = {trigger} ] && [ -f \"$armed\" ]; then\nif \"$git\" \"$@\"; then\nrm -- \"$armed\"\n{body}\nexit 0\nelse exit $?; fi\nfi\ndone\nexec \"$git\" \"$@\"\n",
            git = shell_words::quote(git.to_str().ok_or("git")?),
            repo = shell_words::quote(env.repo().to_str().ok_or("repo")?),
            armed = shell_words::quote(armed.to_str().ok_or("armed")?),
            trigger = shell_words::quote(trigger),
        ),
    )?;
    env.prepend_path(wrapper);
    Ok(())
}

fn pull_pair(kind: &str) -> (TestEnv, TestEnv) {
    let a = TestEnv::new();
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write_repo(".base", b"base");
    a.ok(&["sync"]);
    let b = TestEnv::new();
    b.ok(&["init", &a.remote_url()]);
    if matches!(kind, "diverge" | "empty") {
        let path = if kind == "empty" { ".shared" } else { ".local" };
        b.write_repo(path, b"same-patch");
        b.ok(&["git", "add", "--", path]);
        b.ok(&["git", "commit", "-m", "reviewed local commit"]);
        if kind == "empty" {
            a.write_repo(path, b"same-patch");
            a.ok(&["sync"]);
        }
    }
    if kind != "noop" {
        a.write_repo(".arrived", b"from-remote");
        a.ok(&["sync"]);
    }
    (a, b)
}

fn no_downstream(out: &std::process::Output) -> R {
    for entry in reports(out)? {
        assert!(!matches!(
            at(&entry, "/stage").as_str(),
            Some("apply" | "reencrypt" | "hooks" | "push")
        ));
    }
    Ok(())
}

#[test]
fn post_pull_branch_races_refuse_without_adopting_head_or_losing_staged_entries() -> R {
    for kind in ["fast_forward", "diverge", "noop", "empty"] {
        let (a, mut b) = pull_pair(kind);
        let reviewed = sync_input(&b)?;
        b.write_repo(".unlisted", b"unlisted-staged");
        b.ok(&["git", "add", "--", ".unlisted"]);
        let before = b.git(&[
            "-C",
            b.repo().to_str().ok_or("repo")?,
            "ls-files",
            "-s",
            "--",
            ".unlisted",
        ]);
        after_git(
            &mut b,
            "pull",
            r#"
unset GIT_REFLOG_ACTION
printf external-race > "$repo/.race"
"$git" -C "$repo" -c core.hooksPath=/dev/null add -- .race
"$git" -C "$repo" -c core.hooksPath=/dev/null commit --only -qm external -- .race
"#,
        )?;
        let out = decided(&b, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "{kind}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!b.root().join("armed").exists());
        no_downstream(&out)?;
        assert_eq!(fs::read(b.repo().join(".race"))?, b"external-race");
        assert_eq!(fs::read(b.repo().join(".unlisted"))?, b"unlisted-staged");
        assert_eq!(
            b.git(&[
                "-C",
                b.repo().to_str().ok_or("repo")?,
                "ls-files",
                "-s",
                "--",
                ".unlisted"
            ]),
            before
        );
        let remote = a.remote_path();
        assert!(
            !a.git_raw(&[
                "--git-dir",
                remote.to_str().ok_or("remote")?,
                "show",
                "main:.race"
            ])
            .status
            .success()
        );
        assert!(!b.home().join(".arrived").exists());
        assert!(!b.home().join(".local").exists());
    }
    Ok(())
}

#[test]
fn legitimate_pull_receipts_work_when_reflogs_were_disabled_or_absent() -> R {
    for kind in ["fast_forward", "diverge", "noop", "empty"] {
        let (_a, b) = pull_pair(kind);
        b.ok(&["git", "config", "core.logAllRefUpdates", "false"]);
        fs::remove_file(b.repo().join(".git/logs/refs/heads/main"))?;
        let reviewed = sync_input(&b)?;
        let out = decided(&b, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{kind}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        if kind != "noop" {
            assert_eq!(b.read(".arrived"), b"from-remote");
        }
        if kind == "diverge" {
            assert_eq!(b.read(".local"), b"same-patch");
        }
    }
    Ok(())
}

#[test]
fn missing_or_ambiguous_pull_receipts_refuse_before_apply_and_restore_unlisted_index() -> R {
    for missing in [false, true] {
        let (a, mut b) = pull_pair("fast_forward");
        let reviewed = sync_input(&b)?;
        b.write_repo(".unlisted", b"unlisted-staged");
        b.ok(&["git", "add", "--", ".unlisted"]);
        let before = b.git(&[
            "-C",
            b.repo().to_str().ok_or("repo")?,
            "ls-files",
            "-s",
            "--",
            ".unlisted",
        ]);
        let body = if missing {
            r#"
awk -v action="$GIT_REFLOG_ACTION" '{ at=index($0, action); if (at) print substr($0,1,at-1) "unattributed-pull"; else print }' "$repo/.git/logs/refs/heads/main" > "$repo/.git/receipt-log"
mv -- "$repo/.git/receipt-log" "$repo/.git/logs/refs/heads/main"
"#
        } else {
            r#"
tail -n 1 "$repo/.git/logs/refs/heads/main" > "$repo/.git/receipt-log"
cat "$repo/.git/receipt-log" >> "$repo/.git/logs/refs/heads/main"
rm -- "$repo/.git/receipt-log"
"#
        };
        after_git(&mut b, "pull", body)?;
        let out = decided(&b, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!b.root().join("armed").exists());
        no_downstream(&out)?;
        assert!(!b.home().join(".arrived").exists());
        assert_eq!(
            b.git(&[
                "-C",
                b.repo().to_str().ok_or("repo")?,
                "ls-files",
                "-s",
                "--",
                ".unlisted"
            ]),
            before
        );
        assert_eq!(
            b.git(&["-C", b.repo().to_str().ok_or("repo")?, "rev-parse", "HEAD"]),
            a.git(&[
                "--git-dir",
                a.remote_path().to_str().ok_or("remote")?,
                "rev-parse",
                "main"
            ])
        );
    }
    Ok(())
}

#[test]
fn switching_symbolic_head_at_reviewed_publication_cannot_redirect_the_branch_cas() -> R {
    let mut env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".a", b"base", 0o600);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    env.ok(&["git", "branch", "other"]);
    env.write(".a", b"reviewed", 0o600);
    let reviewed = sync_input(&env)?;
    env.write_repo(".unlisted", b"unlisted-staged");
    env.ok(&["git", "add", "--", ".unlisted"]);
    let index = fs::read(env.repo().join(".git/index"))?;
    let old = env.git(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "rev-parse",
        "HEAD",
    ]);
    let git = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .ok_or("git")?;
    let wrapper = env.root().join("wrapper");
    fs::create_dir(&wrapper)?;
    common::write_exe(
        &wrapper.join("git"),
        &format!(
            "#!/bin/sh\nset -eu\nfor arg in \"$@\"; do\nif [ \"$arg\" = update-ref ]; then\n{git} -C {repo} symbolic-ref HEAD refs/heads/other\nfi\ndone\nexec {git} \"$@\"\n",
            git = shell_words::quote(git.to_str().ok_or("git")?),
            repo = shell_words::quote(env.repo().to_str().ok_or("repo")?),
        ),
    )?;
    env.prepend_path(wrapper);
    let out = decided(&env, &reviewed, "sync", &[]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read(env.repo().join(".git/index"))?, index);
    assert_eq!(
        env.git(&[
            "-C",
            env.repo().to_str().ok_or("repo")?,
            "rev-parse",
            "refs/heads/other"
        ]),
        old
    );
    assert_ne!(
        env.git(&[
            "-C",
            env.repo().to_str().ok_or("repo")?,
            "rev-parse",
            "refs/heads/main"
        ]),
        old
    );
    assert_eq!(
        env.git(&[
            "--git-dir",
            env.remote_path().to_str().ok_or("remote")?,
            "rev-parse",
            "main"
        ]),
        old
    );
    assert_eq!(
        env.git(&[
            "-C",
            env.repo().to_str().ok_or("repo")?,
            "symbolic-ref",
            "HEAD"
        ]),
        "refs/heads/other\n"
    );
    assert!(!env.repo().join(".git/index.lock").exists());
    Ok(())
}

#[test]
fn automatic_replay_uses_its_branch_cas_receipt_not_head_after_reattachment() -> R {
    for raced in [false, true] {
        let (a, mut b) = pull_pair("noop");
        a.write_repo("install.sh", b"origin-installer");
        a.ok(&["sync"]);
        b.write_repo("install.sh", b"local-installer");
        b.ok(&["git", "add", "--", "install.sh"]);
        b.ok(&["git", "commit", "-m", "reviewed installer"]);
        let reviewed = sync_input(&b)?;
        if raced {
            after_git(
                &mut b,
                "dotman: pull",
                r#"
unset GIT_REFLOG_ACTION
tip=$("$git" -C "$repo" rev-parse refs/heads/main)
tree=$("$git" -C "$repo" rev-parse refs/heads/main^{tree})
new=$(printf external-replay-race | "$git" -C "$repo" commit-tree "$tree" -p "$tip")
"$git" -C "$repo" update-ref refs/heads/main "$new" "$tip"
"#,
            )?;
        }
        let out = decided(&b, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(if raced { 3 } else { 0 }),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        if raced {
            assert!(!b.root().join("armed").exists());
            no_downstream(&out)?;
            let last = b.git(&[
                "-C",
                b.repo().to_str().ok_or("repo")?,
                "log",
                "-1",
                "--format=%s",
            ]);
            assert_eq!(last, "external-replay-race\n");
            assert!(
                !a.git(&[
                    "--git-dir",
                    a.remote_path().to_str().ok_or("remote")?,
                    "log",
                    "-1",
                    "--format=%s",
                    "main"
                ])
                .contains("external-replay-race")
            );
        }
        assert_eq!(fs::read(b.repo().join("install.sh"))?, b"origin-installer");
    }
    Ok(())
}

#[test]
fn capture_mode_settings_must_be_reviewed_before_source_or_state_changes() -> R {
    for missing in [false, true] {
        let env = TestEnv::new();
        env.ok(&["init"]);
        env.write(".a", b"base", 0o644);
        env.ok(&["add", ".a"]);
        env.ok(&["sync"]);
        env.write(".a", b"reviewed", 0o600);
        let mut reviewed = sync_input(&env)?;
        let outgoing = reviewed
            .pointer_mut("/sync/outgoing")
            .and_then(Value::as_array_mut)
            .ok_or("outgoing")?;
        let settings = outgoing
            .iter()
            .position(|entry| at(entry, "/path/raw") == ".dotman.toml")
            .ok_or("coupled settings")?;
        if missing {
            outgoing.remove(settings);
        } else {
            let entry = outgoing.get_mut(settings).ok_or("settings")?;
            put(
                entry,
                "/stored/tuple/hash",
                json!("00000000000000000000000000000000"),
            )?;
        }
        let state = saved_state(&env)?.files.get(".a").cloned();
        let repo_before = env.four_trees()[1].clone();
        let out = decided(&env, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(env.four_trees()[1], repo_before);
        assert_eq!(saved_state(&env)?.files.get(".a"), state.as_ref());
        assert_eq!(env.read(".a"), b"reviewed");
        assert_eq!(common::mode_of(&env.home().join(".a")), 0o600);
    }
    Ok(())
}

#[test]
fn encrypted_capture_rechecks_its_source_after_the_encrypting_child() -> R {
    if !common::age_installed() {
        return Ok(());
    }
    let (mut env, _) = encryption_pair();
    let original = fs::read(env.repo().join(".one{age}"))?;
    let replacement = fs::read(env.repo().join(".two{age}"))?;
    env.write(".one", b"reviewed-capture-plaintext", 0o600);
    let reviewed = sync_input(&env)?;
    let git_head = env.git(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "rev-parse",
        "HEAD",
    ]);
    let age_program = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
        .map(|dir| dir.join("age"))
        .find(|path| path.is_file())
        .ok_or("age")?;
    let wrapper = env.root().join("age-wrapper");
    fs::create_dir(&wrapper)?;
    let armed = env.root().join("armed");
    fs::write(&armed, b"")?;
    common::write_exe(
        &wrapper.join("age"),
        &format!(
            "#!/bin/sh\nset -eu\nfor arg in \"$@\"; do\nif [ \"$arg\" = -e ] && [ -f {armed} ]; then\nrm -- {armed}\ncp -- {other} {source}\nfi\ndone\nexec {age} \"$@\"\n",
            armed = shell_words::quote(armed.to_str().ok_or("armed")?),
            other = shell_words::quote(env.repo().join(".two{age}").to_str().ok_or("other")?),
            source = shell_words::quote(env.repo().join(".one{age}").to_str().ok_or("source")?),
            age = shell_words::quote(age_program.to_str().ok_or("age")?),
        ),
    )?;
    env.prepend_path(wrapper);
    let out = decided(&env, &reviewed, "sync", &[]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!armed.exists());
    assert_eq!(fs::read(env.repo().join(".one{age}"))?, replacement);
    assert_eq!(env.read(".one"), b"reviewed-capture-plaintext");
    assert_eq!(
        env.git(&[
            "-C",
            env.repo().to_str().ok_or("repo")?,
            "rev-parse",
            "HEAD"
        ]),
        git_head
    );
    let remote = env.remote_path();
    assert_eq!(
        env.git_raw(&[
            "--git-dir",
            remote.to_str().ok_or("remote")?,
            "show",
            "main:.one{age}"
        ])
        .stdout,
        original
    );
    Ok(())
}

#[test]
fn reviewed_ignore_release_captures_only_after_the_exact_approval() -> R {
    for changed in ["none", "approval", "held"] {
        let env = TestEnv::new();
        env.ok(&["init"]);
        env.write(".config/app/anchor", b"anchor", 0o644);
        env.ok(&["add", ".config/app"]);
        env.ok(&["sync"]);
        let mut config = env.repo_config();
        config.ignore.patterns.push(".config/app/new".to_owned());
        config.save(&env.repo().join(".dotman.toml"), false)?;
        env.ok(&["sync"]);
        env.write(".config/app/new", b"released-file", 0o600);
        config
            .ignore
            .patterns
            .retain(|pattern| pattern != ".config/app/new");
        config.save(&env.repo().join(".dotman.toml"), false)?;
        let mut reviewed = sync_input_with(&env, &["--drop-unlisted-ignores"])?;
        put(
            &mut reviewed,
            "/approvals/ignores",
            json!({
                "retained": [".config/app/new"], "current": config.ignore.patterns
            }),
        )?;
        if changed == "approval" {
            put(
                &mut reviewed,
                "/approvals/ignores/current",
                json!(["stale"]),
            )?;
        } else if changed == "held" {
            env.write(".config/app/new", b"binary\0content", 0o600);
        }
        let before = common::tree_checksum(env.root());
        let preview = decided(&env, &reviewed, "sync", &["--dry-run"]);
        let expected = if changed == "none" { 0 } else { 3 };
        assert_eq!(
            preview.status.code(),
            Some(expected),
            "{changed}: {}",
            String::from_utf8_lossy(&preview.stderr)
        );
        assert_eq!(common::tree_checksum(env.root()), before);
        let out = decided(&env, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(expected),
            "{changed}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let state = saved_state(&env)?;
        assert_eq!(
            env.repo().join(".config/app/new").exists(),
            changed == "none"
        );
        assert_eq!(
            state.files.contains_key(".config/app/new"),
            changed == "none"
        );
        assert_eq!(
            state.ignore_seen.contains(".config/app/new"),
            changed == "approval"
        );
        if changed == "none" {
            assert_eq!(
                fs::read(env.repo().join(".config/app/new"))?,
                b"released-file"
            );
        }
    }
    Ok(())
}

#[test]
fn new_directory_scope_validation_does_not_grant_capture_approval() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["sync"]);
    let mut config = env.repo_config();
    config.dirs.paths.push(".config/new".to_owned());
    config.save(&env.repo().join(".dotman.toml"), false)?;
    env.write(".config/new/file", b"needs-directory-approval", 0o600);
    let reviewed = sync_input_with(&env, &["--approve-dirs"])?;
    assert!(
        at(&reviewed, "/sync/captures")
            .as_array()
            .ok_or("captures")?
            .iter()
            .any(|capture| at(capture, "/key") == ".config/new/file")
    );
    let before = env.four_trees();
    let out = decided(&env, &reviewed, "sync", &[]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(env.four_trees()[0], before[0]);
    assert_eq!(env.four_trees()[1], before[1]);
    let state = saved_state(&env)?;
    assert!(!state.files.contains_key(".config/new/file"));
    assert!(
        !state
            .capture_dirs
            .as_ref()
            .is_some_and(|dirs| dirs.contains(".config/new"))
    );
    Ok(())
}

#[test]
fn detached_replay_edits_before_publication_are_not_operation_receipts() -> R {
    let (a, mut b) = pull_pair("noop");
    a.write_repo("install.sh", b"origin-installer");
    a.ok(&["sync"]);
    b.write_repo("install.sh", b"local-installer");
    b.ok(&["git", "add", "--", "install.sh"]);
    b.ok(&["git", "commit", "-m", "reviewed installer"]);
    let reviewed = sync_input(&b)?;
    let old = b.git(&["-C", b.repo().to_str().ok_or("repo")?, "rev-parse", "HEAD"]);
    after_git(
        &mut b,
        "--is-ancestor",
        r#"
test -f "$repo/.git/dotman-pull"
! "$git" -C "$repo" symbolic-ref -q HEAD
unset GIT_REFLOG_ACTION
printf staged-neighbour > "$repo/.unlisted"
"$git" -C "$repo" add -- .unlisted
tip=$("$git" -C "$repo" rev-parse HEAD)
tree=$("$git" -C "$repo" rev-parse HEAD^{tree})
new=$(printf external-detached-edit | "$git" -C "$repo" commit-tree "$tree" -p "$tip")
"$git" -C "$repo" update-ref --no-deref HEAD "$new" "$tip"
"#,
    )?;
    let out = decided(&b, &reviewed, "sync", &[]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!b.root().join("armed").exists());
    no_downstream(&out)?;
    assert_eq!(
        b.git(&[
            "-C",
            b.repo().to_str().ok_or("repo")?,
            "rev-parse",
            "refs/heads/main"
        ]),
        old
    );
    assert_eq!(
        b.git(&[
            "-C",
            b.repo().to_str().ok_or("repo")?,
            "log",
            "-1",
            "--format=%s"
        ]),
        "external-detached-edit\n"
    );
    assert_eq!(
        b.git_raw(&["-C", b.repo().to_str().ok_or("repo")?, "show", ":.unlisted"])
            .stdout,
        b"staged-neighbour"
    );
    assert_eq!(
        a.git(&[
            "--git-dir",
            a.remote_path().to_str().ok_or("remote")?,
            "log",
            "-1",
            "--format=%s",
            "main"
        ]),
        a.git(&[
            "-C",
            a.repo().to_str().ok_or("repo")?,
            "log",
            "-1",
            "--format=%s"
        ])
    );
    Ok(())
}

#[test]
fn symbolic_branch_refs_cannot_redirect_reviewed_commit_or_replay() -> R {
    for replay in [false, true] {
        let (a, mut b) = pull_pair("noop");
        if replay {
            a.write_repo("install.sh", b"origin-installer");
            a.ok(&["sync"]);
            b.write_repo("install.sh", b"local-installer");
            b.ok(&["git", "add", "--", "install.sh"]);
            b.ok(&["git", "commit", "-m", "reviewed installer"]);
        } else {
            b.write(".a", b"base", 0o600);
            b.ok(&["add", ".a"]);
            b.ok(&["sync"]);
            b.write(".a", b"reviewed", 0o600);
        }
        b.ok(&["git", "branch", "other"]);
        let reviewed = sync_input(&b)?;
        if !replay {
            b.write_repo(".unlisted", b"staged-neighbour");
            b.ok(&["git", "add", "--", ".unlisted"]);
        }
        let index = fs::read(b.repo().join(".git/index"))?;
        let old = b.git(&[
            "-C",
            b.repo().to_str().ok_or("repo")?,
            "rev-parse",
            "refs/heads/other",
        ]);
        let git = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
            .map(|dir| dir.join("git"))
            .find(|path| path.is_file())
            .ok_or("git")?;
        let wrapper = b.root().join("branch-wrapper");
        fs::create_dir(&wrapper)?;
        common::write_exe(
            &wrapper.join("git"),
            &format!(
                "#!/bin/sh\nset -eu\nfor arg in \"$@\"; do\nif [ \"$arg\" = update-ref ]; then\nif [ -f {repo}/.git/dotman-pull ]; then\nprintf staged-neighbour > {repo}/.unlisted\n{git} -C {repo} add -- .unlisted\nfi\n{git} -C {repo} symbolic-ref refs/heads/main refs/heads/other\nfi\ndone\nexec {git} \"$@\"\n",
                git = shell_words::quote(git.to_str().ok_or("git")?),
                repo = shell_words::quote(b.repo().to_str().ok_or("repo")?)
            ),
        )?;
        b.prepend_path(wrapper);
        let remote = fs::read(a.remote_path().join("refs/heads/main"))?;
        let out = decided(&b, &reviewed, "sync", &[]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "{replay}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            b.git(&[
                "-C",
                b.repo().to_str().ok_or("repo")?,
                "rev-parse",
                "refs/heads/other"
            ]),
            old
        );
        if replay {
            assert_eq!(
                b.git_raw(&["-C", b.repo().to_str().ok_or("repo")?, "show", ":.unlisted"])
                    .stdout,
                b"staged-neighbour"
            );
        } else {
            assert_eq!(fs::read(b.repo().join(".git/index"))?, index);
        }
        assert_eq!(
            b.git(&[
                "-C",
                b.repo().to_str().ok_or("repo")?,
                "symbolic-ref",
                "refs/heads/main"
            ]),
            "refs/heads/other\n"
        );
        assert!(!b.repo().join(".git/index.lock").exists());
        assert!(!b.repo().join(".git/refs/heads/main.lock").exists());
        assert_eq!(fs::read(a.remote_path().join("refs/heads/main"))?, remote);
    }
    Ok(())
}

#[test]
fn unavailable_prepared_transactions_refuse_reviewed_publication() -> R {
    let mut env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.ok(&["sync"]);
    env.write_repo(".reviewed", b"reviewed");
    let reviewed = sync_input(&env)?;
    let head = env.git(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "rev-parse",
        "HEAD",
    ]);
    let index = fs::read(env.repo().join(".git/index"))?;
    let git = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .ok_or("git")?;
    let wrapper = env.root().join("unsupported-wrapper");
    fs::create_dir(&wrapper)?;
    common::write_exe(
        &wrapper.join("git"),
        &format!(
            "#!/bin/sh\nset -eu\nfor arg in \"$@\"; do\nif [ \"$arg\" = update-ref ]; then\nread -r line\nprintf '%s\\n' \"fatal: unknown command: $line\" >&2\nexit 128\nfi\ndone\nexec {git} \"$@\"\n",
            git = shell_words::quote(git.to_str().ok_or("git")?)
        ),
    )?;
    env.prepend_path(wrapper);
    let out = decided(&env, &reviewed, "sync", &[]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        env.git(&[
            "-C",
            env.repo().to_str().ok_or("repo")?,
            "rev-parse",
            "HEAD"
        ]),
        head
    );
    assert_eq!(fs::read(env.repo().join(".git/index"))?, index);
    assert_eq!(
        env.git(&[
            "--git-dir",
            env.remote_path().to_str().ok_or("remote")?,
            "rev-parse",
            "main"
        ]),
        head
    );
    assert!(!env.repo().join(".git/index.lock").exists());
    Ok(())
}

#[test]
fn oversized_transaction_errors_are_drained_and_reaped() -> R {
    use std::io::Write as _;
    use std::process::Stdio;
    let mut env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.ok(&["sync"]);
    env.write_repo(".reviewed", b"reviewed");
    let reviewed = sync_input(&env)?;
    let head = env.git(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "rev-parse",
        "HEAD",
    ]);
    let index = fs::read(env.repo().join(".git/index"))?;
    let git = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .ok_or("git")?;
    let wrapper = env.root().join("noisy-wrapper");
    fs::create_dir(&wrapper)?;
    let pid = env.root().join("transaction-pid");
    common::write_exe(
        &wrapper.join("git"),
        &format!(
            "#!/bin/sh\nset -eu\nfor arg in \"$@\"; do\nif [ \"$arg\" = update-ref ]; then\nprintf '%s\\n' \"$$\" > {pid}\nhead -c 1048576 /dev/zero | tr '\\000' x >&2\nexit 128\nfi\ndone\nexec {git} \"$@\"\n",
            pid = shell_words::quote(pid.to_str().ok_or("pid")?),
            git = shell_words::quote(git.to_str().ok_or("git")?)
        ),
    )?;
    env.prepend_path(wrapper);
    let mut command = env.process_for(std::path::Path::new("timeout"));
    command
        .args([
            "--signal=TERM",
            "--kill-after=2s",
            "8s",
            env!("CARGO_BIN_EXE_dotman"),
            "sync",
            "--decide-from",
            "-",
            "--json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    child
        .stdin
        .take()
        .ok_or("stdin")?
        .write_all(reviewed.to_string().as_bytes())?;
    let out = child.wait_with_output()?;
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pid = fs::read_to_string(pid)?;
    assert!(!std::path::Path::new("/proc").join(pid.trim()).exists());
    assert_eq!(fs::read(env.repo().join(".git/index"))?, index);
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
        env.git(&[
            "--git-dir",
            env.remote_path().to_str().ok_or("remote")?,
            "rev-parse",
            "main"
        ]),
        head
    );
    assert!(!env.repo().join(".git/index.lock").exists());
    assert!(!env.repo().join(".git/refs/heads/main.lock").exists());
    Ok(())
}

#[test]
fn reencryption_decrypts_the_frozen_ciphertext_not_a_restored_source() -> R {
    if !common::age_installed() {
        return Ok(());
    }
    let (mut a, b) = encryption_pair();
    let reviewed = sync_input(&a)?;
    b.ok(&["sync"]);
    let age_program = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
        .map(|dir| dir.join("age"))
        .find(|path| path.is_file())
        .ok_or("age")?;
    let wrapper = a.root().join("substitute-wrapper");
    fs::create_dir(&wrapper)?;
    let backup = a.root().join("original-ciphertext");
    fs::copy(a.repo().join(".two{age}"), &backup)?;
    common::write_exe(
        &wrapper.join("age"),
        &format!(
            "#!/bin/sh\nset -eu\nfor arg in \"$@\"; do\nif [ \"$arg\" = -e ]; then\ntouch {ready}\nfi\nif [ \"$arg\" = -d ] && [ -f {ready} ] && [ ! -f {done} ]; then\nfor input in \"$@\"; do :; done\nprintf '%s\\n' \"$input\" > {snapshot}\ncp -- {other} {source}\nset +e\n{age_program} \"$@\"\nrc=$?\nset -e\ncp -- {backup} {source}\ntouch {done}\nexit \"$rc\"\nfi\ndone\nexec {age_program} \"$@\"\n",
            ready = shell_words::quote(a.root().join("encrypted").to_str().ok_or("ready")?),
            done = shell_words::quote(a.root().join("substituted").to_str().ok_or("done")?),
            other = shell_words::quote(a.repo().join(".one{age}").to_str().ok_or("other")?),
            source = shell_words::quote(a.repo().join(".two{age}").to_str().ok_or("source")?),
            backup = shell_words::quote(backup.to_str().ok_or("backup")?),
            snapshot =
                shell_words::quote(a.root().join("snapshot-path").to_str().ok_or("snapshot")?),
            age_program = shell_words::quote(age_program.to_str().ok_or("age")?)
        ),
    )?;
    a.prepend_path(wrapper);
    let out = decided(&a, &reviewed, "sync", &[]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(a.root().join("substituted").exists());
    let snapshot = fs::read_to_string(a.root().join("snapshot-path"))?;
    assert!(!std::path::Path::new(snapshot.trim()).exists());
    let mut decrypt = a.process_for(&age_program);
    decrypt
        .args(["-d", "-i"])
        .arg(a.xdg_config().join("dotman/age-identity.txt"))
        .arg("--")
        .arg(a.repo().join(".two{age}"));
    let plain = decrypt.output()?;
    assert!(plain.status.success());
    assert_eq!(plain.stdout, b"review-private-two");
    Ok(())
}

#[test]
fn missing_detached_replay_attribution_stops_before_publication() -> R {
    let (a, mut b) = pull_pair("noop");
    a.write_repo("install.sh", b"origin-installer");
    a.ok(&["sync"]);
    b.write_repo("install.sh", b"local-installer");
    b.ok(&["git", "add", "--", "install.sh"]);
    b.ok(&["git", "commit", "-m", "reviewed installer"]);
    let reviewed = sync_input(&b)?;
    let old = b.git(&["-C", b.repo().to_str().ok_or("repo")?, "rev-parse", "HEAD"]);
    let git = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .ok_or("git")?;
    let wrapper = b.root().join("unattributed-wrapper");
    fs::create_dir(&wrapper)?;
    common::write_exe(
        &wrapper.join("git"),
        &format!(
            "#!/bin/sh\nset -eu\nif [ -f {repo}/.git/dotman-pull ]; then\nfor arg in \"$@\"; do\nif [ \"$arg\" = rebase ]; then\nunset GIT_REFLOG_ACTION\nfi\ndone\nfi\nexec {git} \"$@\"\n",
            repo = shell_words::quote(b.repo().to_str().ok_or("repo")?),
            git = shell_words::quote(git.to_str().ok_or("git")?)
        ),
    )?;
    b.prepend_path(wrapper);
    let out = decided(&b, &reviewed, "sync", &[]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    no_downstream(&out)?;
    assert_eq!(
        b.git(&[
            "-C",
            b.repo().to_str().ok_or("repo")?,
            "rev-parse",
            "refs/heads/main"
        ]),
        old
    );
    assert!(b.repo().join(".git/dotman-pull").exists());
    assert_eq!(
        a.git(&[
            "--git-dir",
            a.remote_path().to_str().ok_or("remote")?,
            "show",
            "main:install.sh"
        ]),
        "origin-installer"
    );
    Ok(())
}

#[test]
fn reviewed_sync_does_not_automatically_capture_system_edits() -> R {
    let mut env = TestEnv::new();
    env.use_system_root();
    env.write_machine_config("[system]\nroots = [\"/etc\"]\n");
    env.write_system("/etc/keyd/default.conf", b"system-base", 0o644);
    env.ok(&["init"]);
    env.ok(&["add", "--system", "/etc/keyd/default.conf"]);
    env.write(".home", b"base", 0o600);
    env.ok(&["add", ".home"]);
    env.ok(&["sync"]);
    env.write(".home", b"reviewed-home", 0o600);
    let reviewed = sync_input(&env)?;
    let system = saved_state(&env)?.system;
    let settings = fs::read(env.repo().join(".dotman.toml"))?;
    env.write_system("/etc/keyd/default.conf", b"late-system", 0o600);
    let out = decided(&env, &reviewed, "sync", &[]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read(env.repo().join(".dotman/system/etc/keyd/default.conf"))?,
        b"system-base"
    );
    assert_eq!(saved_state(&env)?.system, system);
    assert_eq!(fs::read(env.repo().join(".dotman.toml"))?, settings);
    assert_eq!(fs::read(env.repo().join(".home"))?, b"reviewed-home");
    Ok(())
}

fn reencrypt_status_race(env: &mut TestEnv, path: &str, produced: bool) -> R {
    let find = |name| -> R<std::path::PathBuf> {
        std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
            .map(|dir| dir.join(name))
            .find(|path| path.is_file())
            .ok_or_else(|| "program".into())
    };
    let wrapper = env.root().join("reencrypt-wrapper");
    fs::create_dir(&wrapper)?;
    let ready = env.root().join("encrypted");
    let done = env.root().join("injected");
    let record = env.repo().join(".dotman/age-recipients");
    let original = env.root().join("original-record");
    fs::copy(&record, &original)?;
    common::write_exe(
        &wrapper.join("age"),
        &format!(
            "#!/bin/sh\nset -eu\nfor arg in \"$@\"; do\nif [ \"$arg\" = -e ]; then\ntouch {ready}\nfi\ndone\nexec {age_program} \"$@\"\n",
            ready = shell_words::quote(ready.to_str().ok_or("ready")?),
            age_program = shell_words::quote(find("age")?.to_str().ok_or("age")?)
        ),
    )?;
    common::write_exe(
        &wrapper.join("git"),
        &format!(
            "#!/bin/sh\nset -eu\nfor arg in \"$@\"; do\nif [ \"$arg\" = status ] && [ -f {ready} ] && [ ! -f {done} ] {condition}; then\n{git} \"$@\"\n{edit}\ntouch {done}\nexit 0\nfi\ndone\nexec {git} \"$@\"\n",
            ready = shell_words::quote(ready.to_str().ok_or("ready")?),
            done = shell_words::quote(done.to_str().ok_or("done")?),
            condition = if produced {
                format!(
                    "&& ! cmp -s -- {} {}",
                    shell_words::quote(record.to_str().ok_or("record")?),
                    shell_words::quote(original.to_str().ok_or("original")?)
                )
            } else {
                String::new()
            },
            git = shell_words::quote(find("git")?.to_str().ok_or("git")?),
            edit = if matches!(path, ".one{age}" | ".late{age}" | ".two{host=late+age}") {
                format!(
                    "cp -- {} {}",
                    shell_words::quote(env.repo().join(".two{age}").to_str().ok_or("other")?),
                    shell_words::quote(env.repo().join(path).to_str().ok_or("source")?)
                )
            } else if matches!(path, ".index-only{age}" | ".staged-existing") {
                format!(
                    "oid=$({git} -C {repo} hash-object -w --stdin < {other})\n{git} -C {repo} update-index --add --cacheinfo \"100644,$oid,{target}\"",
                    git = shell_words::quote(find("git")?.to_str().ok_or("git")?),
                    repo = shell_words::quote(env.repo().to_str().ok_or("repo")?),
                    other =
                        shell_words::quote(env.repo().join(".two{age}").to_str().ok_or("other")?),
                    target = if path == ".staged-existing" {
                        ".one{age}"
                    } else {
                        path
                    }
                )
            } else {
                format!(
                    "printf '\\n# external-late-edit\\n' >> {}",
                    shell_words::quote(env.repo().join(path).to_str().ok_or("path")?)
                )
            }
        ),
    )?;
    env.prepend_path(wrapper);
    Ok(())
}

#[test]
fn late_age_inventory_and_index_entries_refuse_the_whole_reencryption() -> R {
    if !common::age_installed() {
        return Ok(());
    }
    for produced in [false, true] {
        for path in [
            ".late{age}",
            ".two{host=late+age}",
            ".index-only{age}",
            ".staged-existing",
        ] {
            let (mut a, b) = encryption_pair();
            let reviewed = sync_input(&a)?;
            b.ok(&["sync"]);
            let remote = b.git(&["-C", b.repo().to_str().ok_or("repo")?, "rev-parse", "HEAD"]);
            let before = [".one{age}", ".two{age}", ".dotman/age-recipients"]
                .map(|path| fs::read(a.repo().join(path)))
                .into_iter()
                .collect::<Result<Vec<_>, _>>()?;
            a.write_repo(".unlisted", b"unrelated-staged");
            a.ok(&["git", "add", "--", ".unlisted"]);
            let stage = a.git(&[
                "-C",
                a.repo().to_str().ok_or("repo")?,
                "ls-files",
                "-s",
                "--",
                ".unlisted",
            ]);
            reencrypt_status_race(&mut a, path, produced)?;
            let out = decided(&a, &reviewed, "sync", &[]);
            assert_eq!(
                out.status.code(),
                Some(3),
                "{produced}/{path}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(a.root().join("injected").exists());
            assert_eq!(
                a.git(&["-C", a.repo().to_str().ok_or("repo")?, "rev-parse", "HEAD"]),
                remote
            );
            assert_eq!(
                a.git(&[
                    "--git-dir",
                    a.remote_path().to_str().ok_or("remote")?,
                    "rev-parse",
                    "main"
                ]),
                remote
            );
            assert_eq!(
                a.git(&[
                    "-C",
                    a.repo().to_str().ok_or("repo")?,
                    "ls-files",
                    "-s",
                    "--",
                    ".unlisted"
                ]),
                stage
            );
            let late = if matches!(path, ".late{age}" | ".two{host=late+age}") {
                fs::read(a.repo().join(path))?
            } else {
                let target = if path == ".staged-existing" {
                    ".one{age}"
                } else {
                    path
                };
                a.git_raw(&[
                    "-C",
                    a.repo().to_str().ok_or("repo")?,
                    "show",
                    &format!(":{target}"),
                ])
                .stdout
            };
            assert_eq!(late, fs::read(a.repo().join(".two{age}"))?);
            if !produced {
                for (path, bytes) in [".one{age}", ".two{age}", ".dotman/age-recipients"]
                    .iter()
                    .zip(before)
                {
                    assert_eq!(fs::read(a.repo().join(path))?, bytes);
                }
            }
        }
    }
    Ok(())
}

#[test]
fn reencrypt_freezes_inputs_and_couples_the_produced_commit() -> R {
    if !common::age_installed() {
        return Ok(());
    }
    for produced in [false, true] {
        for path in [".one{age}", ".dotman.toml", ".dotman/age-recipients"] {
            let (mut a, b) = encryption_pair();
            let reviewed = sync_input(&a)?;
            b.ok(&["sync"]);
            let remote = b.git(&["-C", b.repo().to_str().ok_or("repo")?, "rev-parse", "HEAD"]);
            let record = fs::read(a.repo().join(".dotman/age-recipients"))?;
            let old_source = fs::read(a.repo().join(".one{age}"))?;
            let other_source = fs::read(a.repo().join(".two{age}"))?;
            a.write_repo(".unlisted", b"staged-neighbour");
            a.ok(&["git", "add", "--", ".unlisted"]);
            let index = a.git(&[
                "-C",
                a.repo().to_str().ok_or("repo")?,
                "ls-files",
                "-s",
                "--",
                ".unlisted",
            ]);
            reencrypt_status_race(&mut a, path, produced)?;
            let out = decided(&a, &reviewed, "sync", &[]);
            assert_eq!(
                out.status.code(),
                Some(3),
                "{produced}/{path}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(a.root().join("injected").exists(), "{produced}/{path}");
            if path == ".one{age}" {
                assert_eq!(
                    fs::read(a.repo().join(path))?,
                    fs::read(a.repo().join(".two{age}"))?
                );
            } else {
                assert!(fs::read(a.repo().join(path))?.ends_with(b"\n# external-late-edit\n"));
            }
            if !produced {
                assert_eq!(fs::read(a.repo().join(".two{age}"))?, other_source);
                if path != ".dotman/age-recipients" {
                    assert_eq!(fs::read(a.repo().join(".dotman/age-recipients"))?, record);
                }
                if path != ".one{age}" {
                    assert_eq!(fs::read(a.repo().join(".one{age}"))?, old_source);
                }
            }
            assert_eq!(
                a.git(&["-C", a.repo().to_str().ok_or("repo")?, "rev-parse", "HEAD"]),
                remote
            );
            assert_eq!(
                a.git(&[
                    "--git-dir",
                    a.remote_path().to_str().ok_or("remote")?,
                    "rev-parse",
                    "main"
                ]),
                remote
            );
            assert_eq!(
                a.git(&[
                    "-C",
                    a.repo().to_str().ok_or("repo")?,
                    "ls-files",
                    "-s",
                    "--",
                    ".unlisted"
                ]),
                index
            );
            let objects = a.git_raw(&[
                "--git-dir",
                a.remote_path().to_str().ok_or("remote")?,
                "cat-file",
                "--batch-all-objects",
                "--batch",
            ]);
            assert!(objects.status.success());
            assert!(
                !objects
                    .stdout
                    .windows(b"review-private".len())
                    .any(|bytes| bytes == b"review-private")
            );
        }
    }
    Ok(())
}
