use super::{R, TestEnv, expect_prepared_publication, input, records};
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn fixture(repo: &[u8], home: &[u8]) -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", repo, 0o600);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    env.write(".a", home, 0o600);
    env
}

#[test]
fn dry_run_matches_then_real_sync_leaves_unselected_home_lines() -> R {
    let repo = b"header\r\nobsolete\r\nmiddle\r\nanchor\r\nend";
    let home = b"header\r\nmiddle\r\ninserted\r\nanchor\r\nEND";
    let env = fixture(repo, home);
    let review = input(&env, &[1, 2])?.to_string();
    let before = env.four_trees();
    let run = |args: &[&str]| env.run_stdin(&[], review.as_bytes(), args);
    let dry = run(&[
        "sync",
        "--dry-run",
        "--decide-from",
        "-",
        "--no-hooks",
        "--json",
    ]);
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    assert_eq!(env.four_trees(), before);
    let Some(real) = expect_prepared_publication(&env, || {
        run(&["sync", "--decide-from", "-", "--no-hooks", "--json"])
    })?
    else {
        env.ok(&["sync", "--no-hooks"]);
        assert_eq!(fs::read(env.repo().join(".a"))?, home);
        return Ok(());
    };
    assert!(
        real.status.success(),
        "{}",
        String::from_utf8_lossy(&real.stderr)
    );
    assert_eq!(fs::read(env.home().join(".a"))?, home);
    assert_eq!(
        fs::read(env.repo().join(".a"))?,
        b"header\r\nmiddle\r\ninserted\r\nanchor\r\nend"
    );
    let status = records(&env.run(&["status", "--json"]))?;
    assert!(status.iter().any(
        |v| v.pointer("/key/raw").and_then(serde_json::Value::as_str) == Some(".a")
            && v.get("status").and_then(serde_json::Value::as_str) == Some("changed-locally")
    ));
    env.ok(&["sync", "--no-hooks"]);
    assert_eq!(fs::read(env.repo().join(".a"))?, home);
    Ok(())
}

#[test]
fn changed_permissions_after_selection_refuse_before_writing() -> R {
    let env = fixture(b"old\n", b"new\n");
    let review = input(&env, &[1])?.to_string();
    env.write(".a", b"new\n", 0o644);
    let before = env.four_trees();
    let out = env.run_stdin(
        &[],
        review.as_bytes(),
        &["sync", "--decide-from", "-", "--no-hooks"],
    );
    assert!(!out.status.success());
    assert_eq!(env.four_trees(), before);
    Ok(())
}

#[test]
fn private_capture_binds_physical_source_and_preserves_modes() -> R {
    let env = fixture(b"one\ntwo\nthree\n", b"ONE\ntwo\nTHREE\n");
    fs::set_permissions(env.repo().join(".a"), fs::Permissions::from_mode(0o644))?;
    let status = records(&env.run(&["status", "--json"]))?;
    let entry = status
        .iter()
        .find(|v| v.pointer("/key/raw").and_then(serde_json::Value::as_str) == Some(".a"))
        .ok_or("entry")?;
    assert_eq!(
        entry.pointer("/r/tuple/mode").ok_or("logical repo mode")?,
        "0600"
    );
    assert_eq!(
        entry
            .pointer("/physical_plain/tuple/mode")
            .ok_or("physical repo mode")?,
        "0644"
    );
    let id = fs::read_to_string(env.repo().join(".git/dotman-id"))?;
    let exact = format!("--exact-repo={}", id.trim());
    let path = env.home().join(".a");
    let path = path.to_str().ok_or("path")?;
    let diff = records(&env.run(&["diff", "--json", &exact, "--", path]))?;
    let diff = diff.iter().find(|v| v["record"] == "diff").ok_or("diff")?;
    assert_eq!(diff["physical_plain"], entry["physical_plain"]);
    assert_eq!(diff["line_edits"].as_array().ok_or("line edits")?.len(), 2);
    let review = input(&env, &[1])?.to_string();
    fs::set_permissions(env.repo().join(".a"), fs::Permissions::from_mode(0o600))?;
    let before = env.four_trees();
    let stale = env.run_stdin(
        &[],
        review.as_bytes(),
        &["sync", "--decide-from", "-", "--no-hooks", "--json"],
    );
    assert!(!stale.status.success());
    assert_eq!(env.four_trees(), before);
    fs::set_permissions(env.repo().join(".a"), fs::Permissions::from_mode(0o644))?;
    let Some(real) = expect_prepared_publication(&env, || {
        env.run_stdin(
            &[],
            review.as_bytes(),
            &["sync", "--decide-from", "-", "--no-hooks", "--json"],
        )
    })?
    else {
        return Ok(());
    };
    assert!(
        real.status.success(),
        "{}",
        String::from_utf8_lossy(&real.stderr)
    );
    assert_eq!(fs::read(env.repo().join(".a"))?, b"ONE\ntwo\nthree\n");
    assert_eq!(
        env.git(&["-C", env.repo().to_str().ok_or("repo")?, "show", "HEAD:.a"]),
        "ONE\ntwo\nthree\n"
    );
    assert_eq!(fs::read(env.home().join(".a"))?, b"ONE\ntwo\nTHREE\n");
    assert_eq!(
        fs::metadata(env.repo().join(".a"))?.permissions().mode() & 0o777,
        0o644
    );
    assert_eq!(
        fs::metadata(env.home().join(".a"))?.permissions().mode() & 0o777,
        0o600
    );
    let status = records(&env.run(&["status", "--json"]))?;
    let entry = status
        .iter()
        .find(|v| v.pointer("/key/raw").and_then(serde_json::Value::as_str) == Some(".a"))
        .ok_or("entry")?;
    assert_eq!(
        entry.pointer("/s/tuple/mode").ok_or("recorded mode")?,
        "0600"
    );
    env.write(".a", b"ONE\ntwo\nTHREE\n", 0o644);
    let diff = records(&env.run(&["diff", "--json", &exact, "--", path]))?;
    let diff = diff.iter().find(|v| v["record"] == "diff").ok_or("diff")?;
    assert!(diff.get("line_edits").is_none());
    Ok(())
}

#[test]
fn projection_that_trips_the_guard_is_not_committed() -> R {
    let old = concat!("-----BEGIN RSA PRIVATE", " KEY-----\nnot-key-material\n");
    let new = format!("test banner\n{}\n", "A".repeat(64));
    let env = fixture(old.as_bytes(), new.as_bytes());
    let review = input(&env, &[2])?.to_string();
    let before = env.four_trees();
    let out = env.run_stdin(
        &[],
        review.as_bytes(),
        &["sync", "--decide-from", "-", "--no-hooks"],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("held"));
    assert_eq!(env.four_trees(), before);
    Ok(())
}

#[test]
fn incomplete_selected_requests_cannot_capture_the_whole_file() -> R {
    let env = fixture(b"one\ntwo\n", b"ONE\nTWO\n");
    for (lines, target) in [
        (None, Some(serde_json::json!("local"))),
        (Some(serde_json::Value::Null), None),
        (
            Some(serde_json::json!({".a":[1]})),
            Some(serde_json::Value::Null),
        ),
    ] {
        let mut document = input(&env, &[1])?;
        let sync = document
            .get_mut("sync")
            .and_then(serde_json::Value::as_object_mut)
            .ok_or("sync")?;
        sync.remove("selected_lines");
        if let Some(value) = lines {
            sync.insert("selected_lines".into(), value);
        }
        if let Some(value) = target {
            sync.insert("selected_target".into(), value);
        }
        let before = env.four_trees();
        let out = env.run_stdin(
            &[],
            document.to_string().as_bytes(),
            &["sync", "--decide-from", "-", "--no-hooks", "--json"],
        );
        assert!(!out.status.success());
        assert_eq!(env.four_trees(), before);
    }
    Ok(())
}
