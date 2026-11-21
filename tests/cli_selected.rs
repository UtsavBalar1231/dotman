mod common;
#[path = "cli_selected/edges.rs"]
mod edges;
#[path = "cli_selected/transport.rs"]
mod transport;

use common::{TestEnv, expect_prepared_publication};
use serde_json::{Value, json};
use std::fs;

type R = Result<(), Box<dyn std::error::Error>>;

fn records(output: &std::process::Output) -> Result<Vec<Value>, Box<dyn std::error::Error>> {
    std::str::from_utf8(&output.stdout)?
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn input(env: &TestEnv, ids: &[u32]) -> Result<Value, Box<dyn std::error::Error>> {
    input_with(env, ids, &[])
}

fn input_with(
    env: &TestEnv,
    ids: &[u32],
    vars: &[(&str, &str)],
) -> Result<Value, Box<dyn std::error::Error>> {
    let status = records(&env.run_with(vars, &["status", "--json"]))?;
    let context = status
        .iter()
        .find(|v| v["record"] == "context")
        .ok_or("context")?;
    let review = records(&env.run_with(vars, &["sync", "--dry-run", "--json"]))?;
    let outgoing = review
        .iter()
        .find(|v| v["record"] == "outgoing")
        .ok_or("outgoing")?;
    let capture = review
        .iter()
        .find(|v| v["record"] == "capture" && v.pointer("/key/raw") == Some(&json!(".a")))
        .ok_or("capture")?;
    let mut machine = serde_json::Map::new();
    for key in ["host", "user", "os", "distro", "like", "arch"] {
        machine.insert(
            key.to_owned(),
            context
                .pointer(&format!("/machine/{key}/raw"))
                .ok_or("machine field")?
                .clone(),
        );
    }
    machine.insert(
        "profiles".to_owned(),
        Value::Array(
            context
                .pointer("/machine/profiles")
                .ok_or("profiles")?
                .as_array()
                .ok_or("profiles")?
                .iter()
                .map(|v| v["raw"].clone())
                .collect(),
        ),
    );
    Ok(json!({
        "schema_version": 1, "repo_id": context.pointer("/repo_id/raw").ok_or("repo id")?,
        "home": {"raw": context.pointer("/home/raw").ok_or("home")?}, "repo": {"raw": context.pointer("/repo/raw").ok_or("repo")?},
        "machine": machine, "files": [], "approvals": {},
        "sync": {
            "head": outgoing.pointer("/head/raw"), "tracking_ref": outgoing.pointer("/tracking_ref/raw"),
            "captures": [{"key": ".a", "source": capture.pointer("/source/raw").ok_or("source")?,
                "logical_r": capture["logical_r"], "raw_h": capture["raw_h"], "stored_before": capture["stored_before"]}],
            "outgoing": [], "selected_lines": {".a": ids}
        }
    }))
}

fn bound_input(env: &TestEnv) -> RValue {
    bound_input_with(env, &[])
}

fn bound_input_with(env: &TestEnv, vars: &[(&str, &str)]) -> RValue {
    let mut decision = input_with(env, &[1], vars)?;
    let review = records(&env.run_with(vars, &["sync", "--dry-run", "--json"]))?;
    let outgoing = review
        .iter()
        .find(|v| v["record"] == "outgoing")
        .ok_or("outgoing")?;
    let target = outgoing["selected_target"]
        .as_str()
        .ok_or("selected target")?;
    assert!(!target.contains(&env.remote_url()));
    decision
        .get_mut("sync")
        .and_then(Value::as_object_mut)
        .ok_or("sync")?
        .insert("selected_target".to_owned(), json!(target));
    Ok(decision)
}

type RValue = Result<Value, Box<dyn std::error::Error>>;

#[test]
fn selected_unsupported_transaction_leaves_interrupted_pull_untouched() -> R {
    let mut env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", b"one\ntwo\nthree\n", 0o600);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    env.write(".a", b"ONE\ntwo\nTHREE\n", 0o600);
    let decision = input(&env, &[1])?;
    let head = env.git(&[
        "-C",
        env.repo().to_str().ok_or("repo")?,
        "rev-parse",
        "HEAD",
    ]);
    let head = head.trim();
    fs::write(
        env.repo().join(".git/dotman-pull"),
        format!("dotman-pull 1\0main\0{head}\0{head}\0"),
    )?;
    let run = |env: &TestEnv| {
        env.run_stdin(
            &[],
            decision.to_string().as_bytes(),
            &["sync", "--decide-from", "-", "--no-hooks", "--json"],
        )
    };
    if !common::prepared_transactions_supported()? {
        common::assert_prepared_refusal(&env, || run(&env));
    }
    let git = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .ok_or("git")?;
    let wrapper = env.root().join("no-prepared");
    fs::create_dir(&wrapper)?;
    common::write_exe(
        &wrapper.join("git"),
        &format!(
            "#!/bin/sh\nfor arg do\nif [ \"$arg\" = update-ref ]; then\ncat >/dev/null\nprintf '%s\\n' 'fatal: unknown command: start' >&2\nexit 128\nfi\ndone\nexec {} \"$@\"\n",
            shell_words::quote(git.to_str().ok_or("git")?)
        ),
    )?;
    env.prepend_path(wrapper);
    let before = env.four_trees();
    let out = run(&env);
    assert_eq!(
        out.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(env.four_trees(), before);
    Ok(())
}

#[test]
fn selected_preflight_refuses_before_any_capture_or_backup() -> R {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Stdio;

    for (error, expected) in [
        ("fatal: unknown command: start", 3),
        ("fatal: disk error", 1),
    ] {
        let mut env = TestEnv::new();
        env.ok(&["init", "--remote", &env.remote_url()]);
        env.write(".a", b"one\ntwo\nthree\n", 0o600);
        env.ok(&["add", ".a"]);
        env.ok(&["sync"]);
        env.write(".a", b"ONE\ntwo\nTHREE\n", 0o600);
        let decision = bound_input(&env)?;
        let repo = env.repo();
        let repo_s = repo.to_str().ok_or("repo")?;
        let remote = env.remote_path();
        let home_before = common::tree_checksum(&env.home());
        let state_before = common::tree_checksum(&env.state_dir());
        let remote_before = common::tree_checksum(&remote);
        let refs_before = env.git(&[
            "-C",
            repo_s,
            "for-each-ref",
            "--format=%(refname) %(objectname) %(symref)",
        ]);
        let head_before = env.git(&["-C", repo_s, "rev-parse", "HEAD"]);
        let index_before = fs::read(repo.join(".git/index"))?;
        let file_before = fs::read(repo.join(".a"))?;
        let mode_before = fs::metadata(repo.join(".a"))?.permissions().mode();
        let git = std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?)
            .map(|dir| dir.join("git"))
            .find(|path| path.is_file())
            .ok_or("git")?;
        let wrapper = env.root().join("preflight-wrapper");
        fs::create_dir(&wrapper)?;
        common::write_exe(
            &wrapper.join("git"),
            &format!(
                "#!/bin/sh\nfor arg do\nif [ \"$arg\" = update-ref ]; then\ncat >/dev/null\nprintf '%s\\n' '{}' >&2\nexit 128\nfi\ndone\nexec {} \"$@\"\n",
                error,
                shell_words::quote(git.to_str().ok_or("git")?)
            ),
        )?;
        env.prepend_path(wrapper);
        let mut command = env.process_for(std::path::Path::new("timeout"));
        command
            .args([
                "--kill-after=2s",
                "8s",
                env!("CARGO_BIN_EXE_dotman"),
                "sync",
                "--decide-from",
                "-",
                "--no-hooks",
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
            .write_all(decision.to_string().as_bytes())?;
        let out = child.wait_with_output()?;
        assert_eq!(
            out.status.code(),
            Some(expected),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            records(&out)?
                .iter()
                .any(|v| v["record"] == "result" && v["exit_code"] == expected)
        );
        assert_eq!(common::tree_checksum(&env.home()), home_before);
        assert_eq!(common::tree_checksum(&env.state_dir()), state_before);
        assert_eq!(common::tree_checksum(&remote), remote_before);
        assert_eq!(
            env.git(&[
                "-C",
                repo_s,
                "for-each-ref",
                "--format=%(refname) %(objectname) %(symref)"
            ]),
            refs_before
        );
        assert_eq!(env.git(&["-C", repo_s, "rev-parse", "HEAD"]), head_before);
        assert_eq!(fs::read(repo.join(".git/index"))?, index_before);
        assert_eq!(fs::read(repo.join(".a"))?, file_before);
        assert_eq!(
            fs::metadata(repo.join(".a"))?.permissions().mode(),
            mode_before
        );
        assert!(!repo.join(".git/index.lock").exists());
    }
    Ok(())
}

fn assert_stale_push_target(env: &TestEnv, review: &Value, other_url: &str) -> R {
    let refused = env.run_stdin(
        &[],
        review.to_string().as_bytes(),
        &["sync", "--decide-from", "-", "--no-hooks", "--json"],
    );
    assert!(!refused.status.success());
    assert!(!String::from_utf8_lossy(&refused.stderr).contains(other_url));
    assert_eq!(fs::read(env.repo().join(".a"))?, b"one\ntwo\nthree\n");
    let before = env.four_trees();
    let stale_preview = env.run_stdin(
        &[],
        review.to_string().as_bytes(),
        &[
            "sync",
            "--decide-from",
            "-",
            "--no-hooks",
            "--json",
            "--dry-run",
        ],
    );
    assert!(
        !stale_preview.status.success()
            && common::text(&stale_preview)
                .contains("selected push target changed or was not reviewed"),
        "{}",
        common::text(&stale_preview)
    );
    assert_eq!(env.four_trees(), before);
    Ok(())
}

#[test]
fn selected_push_is_bound_and_pushes_only_the_reviewed_branch() -> R {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".a", b"one\ntwo\nthree\n", 0o600);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    let repo = env.repo();
    let repo = repo.to_str().ok_or("repo")?;
    env.git(&[
        "-C",
        repo,
        "tag",
        "-a",
        "not-selected",
        "-m",
        "not selected",
    ]);
    env.git(&["-C", repo, "config", "push.followTags", "true"]);
    let other = env.root().join("other.git");
    env.git(&["init", "--bare", other.to_str().ok_or("other")?]);
    let other_url = format!("file://{}", other.display());
    env.git(&[
        "-C",
        repo,
        "remote",
        "set-url",
        "--push",
        "origin",
        &env.remote_url(),
    ]);
    env.git(&["-C", repo, "remote", "set-url", "origin", &other_url]);
    env.write(".a", b"ONE\ntwo\nTHREE\n", 0o600);
    let review = bound_input(&env)?;
    env.git(&[
        "-C", repo, "remote", "set-url", "--push", "origin", &other_url,
    ]);
    assert_stale_push_target(&env, &review, &other_url)?;
    env.git(&[
        "-C",
        repo,
        "remote",
        "set-url",
        "--push",
        "origin",
        &env.remote_url(),
    ]);
    let Some(out) = expect_prepared_publication(&env, || {
        env.run_stdin(
            &[],
            review.to_string().as_bytes(),
            &["sync", "--decide-from", "-", "--no-hooks", "--json"],
        )
    })?
    else {
        return Ok(());
    };
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let events = records(&out)?;
    assert!(
        events
            .iter()
            .any(|v| v["record"] == "stage" && v["stage"] == "push")
    );
    let remote = env.remote_path();
    let remote = remote.to_str().ok_or("remote")?;
    assert_eq!(
        env.git(&["--git-dir", remote, "show", "main:.a"]),
        "ONE\ntwo\nthree\n"
    );
    assert_eq!(env.git(&["--git-dir", remote, "tag"]), "");
    assert_eq!(fs::read(env.home().join(".a"))?, b"ONE\ntwo\nTHREE\n");
    assert_eq!(
        env.git_raw(&["--git-dir", other.to_str().ok_or("other")?, "show-ref"])
            .stdout,
        Vec::<u8>::new()
    );
    Ok(())
}

#[test]
fn selected_push_rejects_diverged_remote_and_retains_commit() -> R {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".a", b"one\ntwo\nthree\n", 0o600);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    env.write(".a", b"ONE\ntwo\nTHREE\n", 0o600);
    let review = bound_input(&env)?;
    let other = env.root().join("other-worktree");
    let other = other.to_str().ok_or("other")?;
    env.git(&["clone", &env.remote_url(), other]);
    env.git(&["-C", other, "checkout", "main"]);
    fs::write(
        std::path::Path::new(other).join(".a"),
        b"one\nREMOTE\nthree\n",
    )?;
    env.git(&["-C", other, "add", ".a"]);
    env.git(&["-C", other, "commit", "-m", "remote change"]);
    env.git(&["-C", other, "push", "origin", "HEAD:refs/heads/main"]);
    let remote = env.remote_path();
    let remote = remote.to_str().ok_or("remote")?;
    let before = env.git(&["--git-dir", remote, "rev-parse", "refs/heads/main"]);
    let Some(out) = expect_prepared_publication(&env, || {
        env.run_stdin(
            &[],
            review.to_string().as_bytes(),
            &["sync", "--decide-from", "-", "--no-hooks", "--json"],
        )
    })?
    else {
        return Ok(());
    };
    assert!(!out.status.success());
    assert_eq!(
        env.git(&["--git-dir", remote, "rev-parse", "refs/heads/main"]),
        before
    );
    assert_eq!(fs::read(env.repo().join(".a"))?, b"ONE\ntwo\nthree\n");
    assert_eq!(fs::read(env.home().join(".a"))?, b"ONE\ntwo\nTHREE\n");
    assert!(String::from_utf8_lossy(&out.stderr).contains("local commit"));
    Ok(())
}

#[test]
fn selected_capture_commits_only_selected_edits() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", b"one\ntwo\nthree\n", 0o600);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    env.write(".a", b"ONE\ntwo\nTHREE\n", 0o600);
    let review = input(&env, &[1])?;
    let Some(out) = expect_prepared_publication(&env, || {
        env.run_stdin(
            &[],
            review.to_string().as_bytes(),
            &["sync", "--decide-from", "-", "--no-hooks"],
        )
    })?
    else {
        return Ok(());
    };
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(fs::read(env.repo().join(".a"))?, b"ONE\ntwo\nthree\n");
    assert_eq!(fs::read(env.home().join(".a"))?, b"ONE\ntwo\nTHREE\n");
    let commit = env.git(&["-C", env.repo().to_str().ok_or("repo")?, "show", "HEAD:.a"]);
    assert_eq!(commit, "ONE\ntwo\nthree\n");
    let state =
        dotman::state::State::read(&env.xdg_state().join("dotman/state.toml"))?.ok_or("state")?;
    assert_eq!(
        state.files.get(".a").ok_or("entry")?.hash,
        format!("{:032x}", xxhash_rust::xxh3::xxh3_128(b"ONE\ntwo\nthree\n"))
    );
    Ok(())
}

#[test]
fn selected_capture_rejects_stale_or_unrelated_repo_edits() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", b"one\ntwo\n", 0o600);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    env.write(".a", b"ONE\ntwo\n", 0o600);
    let review = input(&env, &[1])?;
    env.write(".a", b"later\ntwo\n", 0o600);
    let before = env.four_trees();
    let out = env.run_stdin(
        &[],
        review.to_string().as_bytes(),
        &["sync", "--decide-from", "-", "--no-hooks"],
    );
    assert!(!out.status.success());
    assert!(common::text(&out).contains("selected capture changed since review"));
    assert_eq!(env.four_trees(), before);
    assert_eq!(fs::read(env.repo().join(".a"))?, b"one\ntwo\n");
    env.write(".a", b"ONE\ntwo\n", 0o600);
    env.write_repo(".unrelated", b"keep");
    let before = env.four_trees();
    let out = env.run_stdin(
        &[],
        review.to_string().as_bytes(),
        &["sync", "--decide-from", "-", "--no-hooks"],
    );
    assert!(!out.status.success());
    assert!(common::text(&out).contains("selected capture requires a clean repository and index"));
    assert_eq!(env.four_trees(), before);
    assert_eq!(fs::read(env.repo().join(".unrelated"))?, b"keep");
    assert_eq!(fs::read(env.repo().join(".a"))?, b"one\ntwo\n");
    Ok(())
}

#[test]
fn selected_capture_rejects_invalid_ids_and_remote_without_writing() -> R {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".a", b"one\n", 0o600);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    env.write(".a", b"ONE\n", 0o600);
    let before = env.four_trees();
    let review = input(&env, &[1])?;
    let out = env.run_stdin(
        &[],
        review.to_string().as_bytes(),
        &["sync", "--decide-from", "-", "--no-hooks"],
    );
    assert!(!out.status.success());
    assert_eq!(env.four_trees(), before);
    let invalid = input(&env, &[0])?;
    let out = env.run_stdin(
        &[],
        invalid.to_string().as_bytes(),
        &["sync", "--decide-from", "-", "--no-hooks"],
    );
    assert!(!out.status.success());
    assert_eq!(env.four_trees(), before);
    Ok(())
}
