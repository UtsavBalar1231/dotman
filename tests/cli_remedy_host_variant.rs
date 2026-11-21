mod common;

use common::TestEnv;
use serde_json::{Value, json};

fn request(env: &TestEnv, input: &Value) -> std::process::Output {
    env.run_stdin(
        &[("DOTMAN_HOST", "box")],
        input.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    )
}

fn setup(encrypted: bool) -> (TestEnv, String) {
    let env = TestEnv::new();
    env.ok_with(&[("DOTMAN_HOST", "box")], &["init"]);
    if encrypted {
        env.ok_with(&[("DOTMAN_HOST", "box")], &["key", "init"]);
    }
    env.write(".profile", b"base\n", if encrypted { 0o600 } else { 0o644 });
    let args = if encrypted {
        vec!["add", "--encrypt", ".profile"]
    } else {
        vec!["add", ".profile"]
    };
    env.ok_with(&[("DOTMAN_HOST", "box")], &args);
    let source = if encrypted {
        ".profile{age}"
    } else {
        ".profile"
    }
    .to_owned();
    let repo = env.repo().to_string_lossy().into_owned();
    env.git(&["-C", &repo, "add", "-A"]);
    env.git(&["-C", &repo, "commit", "-qm", "fixture"]);
    env.write(
        ".profile",
        b"local\n",
        if encrypted { 0o600 } else { 0o644 },
    );
    (env, source)
}

#[cfg(target_os = "linux")]
fn review(env: &TestEnv, source: &str) -> anyhow::Result<Value> {
    let occurrence = json!({"scope":"home","source":source,"reason":"host_variant","finding":null});
    let output = request(
        env,
        &json!({"schema_version":1,"operation":"review_host_variant","key":".profile","occurrence":occurrence}),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(
        output
            .stdout
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or_default(),
    )?)
}

#[cfg(target_os = "linux")]
fn execute(env: &TestEnv, source: &str, snapshot: &Value) -> std::process::Output {
    request(
        env,
        &json!({"schema_version":1,"operation":"execute_host_variant","key":".profile",
        "occurrence":{"scope":"home","source":source,"reason":"host_variant","finding":null},"snapshot":snapshot}),
    )
}

#[test]
#[cfg(target_os = "linux")]
fn plain_review_execute_preserves_base_home_config_and_index() {
    let (env, source) = setup(false);
    let repo = env.repo().to_string_lossy().into_owned();
    let index = env.git(&["-C", &repo, "write-tree"]);
    let config = std::fs::read(env.repo().join(".dotman.toml")).unwrap();
    let state_path = env.state_dir().join("state.toml");
    let before: dotman::state::State =
        toml::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    let row = review(&env, &source).unwrap();
    assert_eq!(row.pointer("/host/raw"), Some(&json!("box")));
    assert_eq!(
        row.pointer("/target/raw"),
        Some(&json!(".profile{host=box}"))
    );
    let out = execute(&env, &source, row.get("snapshot").unwrap());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(env.repo().join(".profile")).unwrap(),
        b"base\n"
    );
    assert_eq!(
        std::fs::read(env.repo().join(".profile{host=box}")).unwrap(),
        b"local\n"
    );
    assert_eq!(env.read(".profile"), b"local\n");
    assert_eq!(
        std::fs::read(env.repo().join(".dotman.toml")).unwrap(),
        config
    );
    assert_eq!(env.git(&["-C", &repo, "write-tree"]), index);
    let mut after: dotman::state::State =
        toml::from_str(&std::fs::read_to_string(state_path).unwrap()).unwrap();
    assert_eq!(
        after.files.get(".profile").unwrap().source,
        ".profile{host=box}"
    );
    after.files.insert(
        ".profile".into(),
        before.files.get(".profile").unwrap().clone(),
    );
    assert_eq!(after, before);
}

#[test]
#[cfg(target_os = "linux")]
fn encrypted_review_execute_preserves_ciphertext() {
    if !common::age_installed() {
        return;
    }
    let (env, source) = setup(true);
    let base = std::fs::read(env.repo().join(&source)).unwrap();
    let row = review(&env, &source).unwrap();
    assert_eq!(row.get("encrypted"), Some(&json!(true)));
    let target = row.pointer("/target/raw").and_then(Value::as_str).unwrap();
    let out = execute(&env, &source, row.get("snapshot").unwrap());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(env.repo().join(source)).unwrap(), base);
    let ciphertext = std::fs::read(env.repo().join(target)).unwrap();
    assert!(!ciphertext.windows(6).any(|part| part == b"local\n"));
    let decrypted = std::process::Command::new("age")
        .arg("-d")
        .arg("-i")
        .arg(env.xdg_config().join("dotman/age-identity.txt"))
        .arg(env.repo().join(target))
        .output()
        .unwrap();
    assert!(decrypted.status.success());
    assert_eq!(decrypted.stdout, b"local\n");
    assert_eq!(env.read(".profile"), b"local\n");
}

#[test]
fn unsupported_recipient_is_never_emitted() {
    if !common::age_installed() {
        return;
    }
    let (env, source) = setup(true);
    let marker = concat!("private", "-recipient-marker");
    std::fs::write(
        env.repo().join(".dotman.toml"),
        format!("[age]\nrecipients = [\"{marker}\"]\n"),
    )
    .unwrap();
    std::fs::write(
        env.repo().join(".dotman/age-recipients"),
        format!("{marker}\n"),
    )
    .unwrap();
    let input = json!({"schema_version":1,"operation":"review_host_variant","key":".profile",
        "occurrence":{"scope":"home","source":source,"reason":"host_variant","finding":null}});
    let out = request(&env, &input);
    assert!(!out.status.success());
    let error = serde_json::Deserializer::from_slice(&out.stdout)
        .into_iter::<Value>()
        .map(Result::unwrap)
        .find(|row| row.get("record").and_then(Value::as_str) == Some("remedy_error"))
        .expect("remedy_error record");
    assert_eq!(error.get("code"), Some(&json!("terminal_only")));
    assert_eq!(
        error.get("message").and_then(Value::as_str),
        Some("recipient type requires terminal review")
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains(marker));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(marker));
}

#[test]
#[cfg(target_os = "linux")]
fn changed_or_unsupported_mode_is_refused_without_effects() {
    let (env, source) = setup(false);
    let row = review(&env, &source).unwrap();
    env.write(".profile", b"changed\n", 0o644);
    assert!(
        !execute(&env, &source, row.get("snapshot").unwrap())
            .status
            .success()
    );
    env.write(".profile", b"local\n", 0o600);
    let occurrence = json!({"scope":"home","source":source,"reason":"host_variant","finding":null});
    let out = request(
        &env,
        &json!({"schema_version":1,"operation":"review_host_variant","key":".profile","occurrence":occurrence}),
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("terminal_only"));
    assert!(!env.repo().join(".profile{host=box}").exists());
}

#[test]
fn malformed_execute_is_rejected_before_repository_guard() {
    let (env, source) = setup(false);
    std::fs::create_dir(env.repo().join(".git/rebase-merge")).unwrap();
    let out = request(
        &env,
        &json!({"schema_version":1,"operation":"execute_host_variant","key":".profile",
        "occurrence":{"scope":"home","source":source,"reason":"host_variant","finding":null},"snapshot":{}}),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(stderr.contains("invalid remedy request"));
    assert!(!stderr.contains("rebase"));
}

#[test]
#[cfg(target_os = "linux")]
fn chosen_source_and_snapshot_must_agree() {
    let (env, source) = setup(false);
    let row = review(&env, &source).unwrap();
    let output = execute(&env, "other", row.get("snapshot").unwrap());
    assert!(!output.status.success());
    assert!(!env.repo().join(".profile{host=box}").exists());
}

#[test]
#[cfg(target_os = "linux")]
fn non_utf8_repository_context_round_trips_without_loss() {
    use std::os::unix::ffi::OsStringExt;
    let (env, source) = setup(false);
    let repo = env
        .xdg_data()
        .join(std::ffi::OsString::from_vec(b"repo-\xff".to_vec()));
    std::fs::rename(env.repo(), &repo).unwrap();
    let occurrence = json!({"scope":"home","source":source,"reason":"host_variant","finding":null});
    let input = json!({"schema_version":1,"operation":"review_host_variant","key":".profile","occurrence":occurrence});
    let output = env
        .dot()
        .env("DOTMAN_HOST", "box")
        .env("DOTMAN_REPO_PATH", &repo)
        .args(["remedy", "--from", "-", "--json"])
        .write_stdin(input.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let row: Value =
        serde_json::from_slice(output.stdout.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
    assert!(row.pointer("/repo/bytes").is_some());
    let input = json!({"schema_version":1,"operation":"execute_host_variant","key":".profile","occurrence":occurrence,"snapshot":row.get("snapshot").unwrap()});
    let output = env
        .dot()
        .env("DOTMAN_HOST", "box")
        .env("DOTMAN_REPO_PATH", &repo)
        .args(["remedy", "--from", "-", "--json"])
        .write_stdin(input.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(repo.join(".profile{host=box}")).unwrap(),
        b"local\n"
    );
}
