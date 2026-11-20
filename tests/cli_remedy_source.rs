mod common;

use common::TestEnv;
use serde_json::{Value, json};
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, UNIX_EPOCH};

fn request(env: &TestEnv, source: &str, rule: &str) -> std::process::Output {
    let input = json!({
        "schema_version": 1, "operation": "review_forget", "key": ".profile",
        "occurrence": {"scope": "repo", "source": source, "reason": "content",
            "finding": {"rule": rule, "place": "content", "line": 1}}
    });
    env.run_stdin(
        &[],
        input.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    )
}

#[test]
fn repo_occurrence_uses_only_the_selected_sources_finding() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".profile", b"safe\n", 0o600);
    env.ok(&["add", ".profile"]);
    env.write_repo(".profile{host=other}", b"safe variant\n");
    let repo = env.repo().to_string_lossy().into_owned();
    env.git(&["-C", &repo, "add", ".profile", ".profile{host=other}"]);
    let token = format!(
        "token={}\n",
        concat!("gh", "p_", "a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2")
    );
    let age = format!(
        "{}{}\n",
        concat!("AGE-", "SECRET-", "KEY-1"),
        "Q".repeat(58)
    );
    env.write_repo(".profile", token.as_bytes());
    env.write_repo(".profile{host=other}", age.as_bytes());
    assert!(!request(&env, ".profile", "age-secret-key").status.success());
    let out = request(&env, ".profile", "github-token");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(row.get("source_count"), Some(&json!(2)));
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&token));
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&age));
}

#[test]
fn repo_occurrence_does_not_scan_unrelated_staged_content() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".profile", b"safe\n", 0o600);
    env.ok(&["add", ".profile"]);
    env.write_repo(".unrelated", b"staged unrelated\n");
    let repo = env.repo().to_string_lossy().into_owned();
    env.git(&["-C", &repo, "config", "core.trustctime", "false"]);
    env.git(&["-C", &repo, "add", ".profile", ".unrelated"]);
    let token = format!(
        "token={}\n",
        concat!("gh", "p_", "a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2")
    );
    env.write_repo(".profile", token.as_bytes());
    let path = env.repo().join(".unrelated");
    let file = std::fs::File::open(&path).unwrap();
    let old = UNIX_EPOCH + Duration::from_secs(10);
    file.set_times(std::fs::FileTimes::new().set_accessed(old))
        .unwrap();
    let _bytes = std::fs::read(&path).unwrap();
    let calibrated = std::fs::metadata(&path).unwrap();
    if calibrated.atime() == 10 {
        eprintln!("filesystem does not report access-time updates; observation unavailable");
        return;
    }
    file.set_times(std::fs::FileTimes::new().set_accessed(old))
        .unwrap();
    let out = request(&env, ".profile", "github-token");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::metadata(&path).unwrap().atime(), 10);
}
