mod common;

use common::TestEnv;
use serde_json::{Value, json};

#[cfg(target_os = "linux")]
fn new_name_occurrence() -> Value {
    json!({"scope":"new","source":null,"reason":"secret_name","finding":null})
}

#[cfg(target_os = "linux")]
fn content_occurrence(scope: &str, source: Option<&str>) -> Value {
    json!({"scope":scope,"source":source,"reason":"content",
        "finding":{"rule":"github-token","place":"content","line":1}})
}

fn review(env: &TestEnv, key: &str) -> std::process::Output {
    let request = json!({"schema_version": 1, "operation": "review", "key": key});
    env.run_stdin(
        &[],
        request.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    )
}

#[test]
fn reviews_a_binary_leaf_without_changing_it() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/app/seed", b"seed", 0o644);
    env.ok(&["add", ".config/app"]);
    env.write(".config/app/data", b"binary\0data", 0o600);
    let out = review(&env, ".config/app/data");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(record.get("record"), Some(&json!("remedy_review")));
    assert_eq!(record.get("reason"), Some(&json!("binary")));
    assert_eq!(record.get("executable"), Some(&json!(false)));
    assert_eq!(record.pointer("/home/mode"), Some(&json!("0600")));
    assert!(!env.repo().join(".config/app/data").exists());
    assert_eq!(
        std::fs::read(env.home().join(".config/app/data")).unwrap(),
        b"binary\0data"
    );
}

#[test]
fn refuses_nonheld_and_directory_inputs() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/app/seed", b"seed", 0o644);
    env.ok(&["add", ".config/app"]);
    env.write(".config/app/plain", b"plain", 0o644);
    for key in [".config/app", ".config/app/plain", ".config/app/seed"] {
        assert!(!review(&env, key).status.success(), "{key}");
    }
}

#[test]
fn reviews_large_and_rejects_a_content_finding() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/app/seed", b"seed", 0o644);
    env.ok(&["add", ".config/app"]);
    let large = vec![b'x'; 1_048_577];
    env.write(".config/app/large", &large, 0o600);
    let out = review(&env, ".config/app/large");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(record.get("reason"), Some(&json!("large")));
    let mut secret = large;
    secret.extend_from_slice(format!("\ntoken = {}{}\n", "ghp_", "a1B2".repeat(9)).as_bytes());
    env.write(".config/app/large", &secret, 0o600);
    assert!(!review(&env, ".config/app/large").status.success());
    assert!(!env.repo().join(".config/app/large").exists());
}

#[test]
fn execution_without_snapshot_is_rejected() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let input = br#"{"schema_version":1,"operation":"execute","key":"x"}"#;
    let out = env.run_stdin(&[], input, &["remedy", "--from", "-", "--json"]);
    assert!(!out.status.success());
}

#[test]
fn invalid_requests_are_rejected_before_the_repository_guard() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    std::fs::create_dir(env.repo().join(".git/rebase-merge")).unwrap();
    let malformed = env.run_stdin(&[], b"{", &["remedy", "--from", "-", "--json"]);
    assert!(!malformed.status.success());
    let stderr = String::from_utf8_lossy(&malformed.stderr);
    assert!(stderr.contains("invalid remedy request"));
    assert!(!stderr.contains("rebase"));
    let oversized = vec![b' '; 65_537];
    let out = env.run_stdin(&[], &oversized, &["remedy", "--from", "-", "--json"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("request is too large"));
}

#[cfg(target_os = "linux")]
fn snapshot(env: &TestEnv, key: &str) -> anyhow::Result<Value> {
    let out = review(env, key);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap_or_default())?;
    assert_eq!(record.get("executable"), Some(&json!(true)));
    record
        .get("snapshot")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing snapshot"))
}

#[cfg(target_os = "linux")]
fn execute(env: &TestEnv, key: &str, snapshot: &Value) -> std::process::Output {
    let request =
        json!({"schema_version": 1, "operation": "execute", "key": key, "snapshot": snapshot});
    env.run_stdin(
        &[],
        request.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    )
}

fn fixture() -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/app/seed", b"seed", 0o644);
    env.ok(&["add", ".config/app"]);
    env.write(".config/app/data", b"binary\0data", 0o644);
    env
}

#[test]
#[cfg(not(target_os = "linux"))]
fn reviewed_capture_is_not_executable_off_linux() {
    let env = fixture();
    let before = env.four_trees();
    let out = review(&env, ".config/app/data");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(record.get("executable"), Some(&json!(false)));
    assert!(record.get("snapshot").is_none_or(Value::is_null));
    assert_eq!(env.four_trees(), before);
    assert!(!env.repo().join(".config/app/data").exists());
}

#[test]
#[cfg(target_os = "linux")]
fn captures_reviewed_leaf_without_touching_siblings() {
    let env = fixture();
    let key = ".config/app/data";
    let snap = snapshot(&env, key).unwrap();
    let sibling = std::fs::read(env.repo().join(".config/app/seed")).unwrap();
    let out = execute(&env, key, &snap);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(env.repo().join(key)).unwrap(),
        b"binary\0data"
    );
    assert_eq!(
        std::fs::read(env.repo().join(".config/app/seed")).unwrap(),
        sibling
    );
    assert_eq!(
        std::fs::read(env.home().join(key)).unwrap(),
        b"binary\0data"
    );
    assert!(
        env.porcelain("status")
            .contains(&("clean".to_owned(), key.to_owned()))
    );
}

#[test]
#[cfg(target_os = "linux")]
fn nested_held_capture_binds_missing_directories_to_the_review() {
    let env = fixture();
    let key = ".config/app/assets/nested/blank.png";
    env.write(key, b"image\0data", 0o644);
    let assets = env.repo().join(".config/app/assets");
    let snap = snapshot(&env, key).unwrap();
    assert!(!assets.exists());
    std::fs::create_dir(&assets).unwrap();
    assert!(!execute(&env, key, &snap).status.success());
    assert!(!assets.join("nested").exists());
    let snap = snapshot(&env, key).unwrap();
    assert!(execute(&env, key, &snap).status.success());
    assert_eq!(std::fs::read(env.repo().join(key)).unwrap(), b"image\0data");
}

#[test]
#[cfg(target_os = "linux")]
fn refuses_stale_content_and_occupied_destination() {
    for occupied in [false, true] {
        let env = fixture();
        let key = ".config/app/data";
        let snap = snapshot(&env, key).unwrap();
        let state = std::fs::read(env.state_dir().join("state.toml")).unwrap();
        let config = std::fs::read(env.repo().join(".dotman.toml")).unwrap();
        if occupied {
            std::fs::write(env.repo().join(key), b"other").unwrap();
        } else {
            env.write(key, b"changed\0data", 0o644);
        }
        let out = execute(&env, key, &snap);
        assert!(!out.status.success());
        assert_eq!(
            std::fs::read(env.state_dir().join("state.toml")).unwrap(),
            state
        );
        assert_eq!(
            std::fs::read(env.repo().join(".dotman.toml")).unwrap(),
            config
        );
        if occupied {
            assert_eq!(std::fs::read(env.repo().join(key)).unwrap(), b"other");
        } else {
            assert!(!env.repo().join(key).exists());
        }
    }
}

#[test]
#[cfg(target_os = "linux")]
fn captures_preconfigured_private_mode() {
    use std::os::unix::fs::PermissionsExt;
    let env = fixture();
    let key = ".config/app/data";
    env.write(key, b"private\0data", 0o600);
    let config_path = env.repo().join(".dotman.toml");
    let mut config = dotman::repo_config::RepoConfig::load(&config_path).unwrap();
    config.modes.insert(key.to_owned(), "0600".to_owned());
    config.save(&config_path, false).unwrap();
    let before = std::fs::read(&config_path).unwrap();
    let out = execute(&env, key, &snapshot(&env, key).unwrap());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::metadata(env.repo().join(key))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(std::fs::read(&config_path).unwrap(), before);
    assert!(
        env.porcelain("status")
            .contains(&("clean".to_owned(), key.to_owned()))
    );
}

#[test]
#[cfg(target_os = "linux")]
fn refuses_changed_mode_directory_or_new_variant() {
    for change in ["mode", "directory", "variant"] {
        let env = fixture();
        let key = ".config/app/data";
        let snap = snapshot(&env, key).unwrap();
        let state = std::fs::read(env.state_dir().join("state.toml")).unwrap();
        match change {
            "mode" => env.write(key, b"binary\0data", 0o600),
            "directory" => {
                std::fs::remove_file(env.home().join(key)).unwrap();
                std::fs::create_dir(env.home().join(key)).unwrap();
            }
            "variant" => {
                std::fs::write(env.repo().join(".config/app/data{host=other}"), b"x").unwrap();
            }
            _ => unreachable!(),
        }
        assert!(!execute(&env, key, &snap).status.success(), "{change}");
        assert!(!env.repo().join(key).exists(), "{change}");
        assert_eq!(
            std::fs::read(env.state_dir().join("state.toml")).unwrap(),
            state
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn leaves_an_unrelated_staged_edit_alone() {
    let env = fixture();
    let key = ".config/app/data";
    std::fs::write(env.repo().join(".config/app/seed"), b"staged edit").unwrap();
    let repo = env.repo().to_string_lossy().into_owned();
    env.git(&["-C", &repo, "add", ".config/app/seed"]);
    let before = env.git(&["-C", &repo, "diff", "--cached", "--binary"]);
    let out = execute(&env, key, &snapshot(&env, key).unwrap());
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        env.git(&["-C", &repo, "diff", "--cached", "--binary"]),
        before
    );
}

#[test]
#[cfg(target_os = "linux")]
fn encrypts_one_held_leaf_without_a_private_identity() {
    if !common::age_installed() {
        return;
    }
    let env = fixture();
    env.ok(&["key", "init"]);
    env.write(".config/app/bootstrap", b"bootstrap", 0o600);
    env.ok(&["add", "--encrypt", ".config/app/bootstrap"]);
    std::fs::remove_file(env.xdg_config().join("dotman/age-identity.txt")).unwrap();
    let key = ".config/app/.env";
    let secret = format!(
        "token={}\n",
        concat!("gh", "p_", "a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2")
    );
    env.write(key, secret.as_bytes(), 0o600);
    let occurrence = content_occurrence("new", None);
    let request = json!({"schema_version": 1, "operation": "review_encrypt", "key": key, "occurrence": occurrence});
    let out = env.run_stdin(
        &[],
        request.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&secret));
    let record: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    let request = json!({"schema_version": 1, "operation": "execute_encrypt", "key": key, "occurrence": record.get("occurrence").unwrap(), "snapshot": record.get("snapshot").unwrap()});
    let out = env.run_stdin(
        &[],
        request.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ciphertext = std::fs::read(env.repo().join(".config/app/.env{age}")).unwrap();
    assert!(
        !ciphertext
            .windows(secret.len())
            .any(|part| part == secret.as_bytes())
    );
    assert_eq!(
        std::fs::read(env.home().join(key)).unwrap(),
        secret.as_bytes()
    );
    assert!(!env.repo().join(key).exists());
}

#[test]
#[cfg(target_os = "linux")]
fn encrypted_review_refuses_changed_home() {
    if !common::age_installed() {
        return;
    }
    let env = fixture();
    env.ok(&["key", "init"]);
    env.write(".config/app/bootstrap", b"bootstrap", 0o600);
    env.ok(&["add", "--encrypt", ".config/app/bootstrap"]);
    let key = ".config/app/.env";
    env.write(key, b"SECRET=before\n", 0o600);
    let occurrence = new_name_occurrence();
    let input = json!({"schema_version": 1, "operation": "review_encrypt", "key": key, "occurrence": occurrence});
    let out = env.run_stdin(
        &[],
        input.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    let state = std::fs::read(env.state_dir().join("state.toml")).unwrap();
    env.write(key, b"SECRET=after\n", 0o600);
    let input = json!({"schema_version": 1, "operation": "execute_encrypt", "key": key, "occurrence": record.get("occurrence").unwrap(), "snapshot": record.get("snapshot").unwrap()});
    let out = env.run_stdin(
        &[],
        input.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    );
    assert!(!out.status.success());
    assert!(!env.repo().join(".config/app/.env{age}").exists());
    assert_eq!(
        std::fs::read(env.state_dir().join("state.toml")).unwrap(),
        state
    );
}

#[test]
#[cfg(target_os = "linux")]
fn first_held_secret_is_encrypted_with_a_new_recipient_record() {
    if !common::age_installed() {
        return;
    }
    let env = fixture();
    env.ok(&["key", "init"]);
    let key = ".config/app/.env";
    let secret = format!(
        "token={}\n",
        concat!("gh", "p_", "a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2")
    );
    env.write(key, secret.as_bytes(), 0o644);
    let recipient_record = env.repo().join(".dotman/age-recipients");
    assert!(!recipient_record.exists());
    let occurrence = json!({"scope":"new","source":null,"reason":"content",
        "finding":{"rule":"github-token","place":"content","line":1}});
    let request = json!({"schema_version": 1, "operation": "review_encrypt", "key": key, "occurrence": occurrence});
    let out = env.run_stdin(
        &[],
        request.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&secret));
    let record: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(
        record.pointer("/snapshot/review/review/creates_recipient_record"),
        Some(&json!(true))
    );
    assert!(
        record
            .pointer("/snapshot/review/review/mode_consequence")
            .and_then(Value::as_str)
            .unwrap()
            .contains("0600")
    );
    let request = json!({"schema_version": 1, "operation": "execute_encrypt", "key": key, "occurrence": occurrence, "snapshot": record.get("snapshot").unwrap()});
    let out = env.run_stdin(
        &[],
        request.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let encrypted = env.repo().join(".config/app/.env{age}");
    let result = std::process::Command::new("age")
        .arg("-d")
        .arg("-i")
        .arg(env.xdg_config().join("dotman/age-identity.txt"))
        .arg(&encrypted)
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(result.stdout, secret.as_bytes());
    assert!(
        !std::fs::read(recipient_record)
            .unwrap()
            .windows(secret.len())
            .any(|part| part == secret.as_bytes())
    );
    assert!(!env.repo().join(key).exists());
    assert_eq!(
        std::fs::read(env.home().join(key)).unwrap(),
        secret.as_bytes()
    );
}

#[test]
#[cfg(target_os = "linux")]
fn rejects_a_snapshot_with_unknown_fields() {
    let env = fixture();
    let key = ".config/app/data";
    let mut snap = snapshot(&env, key).unwrap();
    snap.as_object_mut()
        .unwrap()
        .insert("extra".to_owned(), json!("unexpected"));
    let out = execute(&env, key, &snap);
    assert!(!out.status.success());
    assert!(!env.repo().join(key).exists());
}

fn action_request(
    env: &TestEnv,
    operation: &str,
    key: &str,
    occurrence: &Value,
    snapshot: Option<&Value>,
) -> std::process::Output {
    let input = json!({"schema_version": 1, "operation": operation, "key": key, "occurrence": occurrence, "snapshot": snapshot});
    env.run_stdin(
        &[],
        input.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    )
}

#[test]
#[cfg(target_os = "linux")]
fn allows_only_name_held_text_without_creating_a_grant() {
    for mode in [0o644, 0o600] {
        let env = fixture();
        let key = ".config/app/.env";
        env.write(key, b"API_URL=https://example.invalid\n", mode);
        let config_path = env.repo().join(".dotman.toml");
        let mut config = std::fs::read_to_string(&config_path).unwrap();
        config.push_str("\n# keep this comment\n");
        std::fs::write(&config_path, config).unwrap();
        let machine = std::fs::read(env.xdg_config().join("dotman/config.toml")).ok();
        let head = env.run(&["git", "rev-parse", "--verify", "HEAD"]);
        let out = action_request(&env, "review_plain", key, &new_name_occurrence(), None);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let record: Value =
            serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
        assert_eq!(record.get("reason"), Some(&json!("secret_name")));
        assert_eq!(record.get("plaintext_may_be_published"), Some(&json!(true)));
        assert_eq!(record.get("prepare_mode"), Some(&json!(mode == 0o600)));
        let out = action_request(
            &env,
            "execute_plain",
            key,
            record.get("occurrence").unwrap(),
            record.get("snapshot"),
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read(env.repo().join(key)).unwrap(),
            b"API_URL=https://example.invalid\n"
        );
        assert!(
            std::fs::read_to_string(config_path)
                .unwrap()
                .contains("# keep this comment")
        );
        assert_eq!(
            std::fs::read(env.xdg_config().join("dotman/config.toml")).ok(),
            machine
        );
        let after = env.run(&["git", "rev-parse", "--verify", "HEAD"]);
        assert_eq!(after.status.code(), head.status.code());
        assert_eq!(after.stdout, head.stdout);
        assert!(
            env.porcelain("status")
                .contains(&("clean".to_owned(), key.to_owned()))
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn plain_review_rejects_new_credentials_and_cross_action_snapshots() {
    let env = fixture();
    let key = ".config/app/.env";
    env.write(key, b"API_URL=https://example.invalid\n", 0o644);
    let out = action_request(&env, "review_plain", key, &new_name_occurrence(), None);
    assert!(out.status.success());
    let record: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    let snap = record.get("snapshot").unwrap().clone();
    let state = std::fs::read(env.state_dir().join("state.toml")).unwrap();
    let config = std::fs::read(env.repo().join(".dotman.toml")).unwrap();
    assert!(!execute(&env, key, &snap).status.success());
    env.write(
        key,
        format!(
            "token={}\n",
            concat!("gh", "p_", "a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2")
        )
        .as_bytes(),
        0o644,
    );
    let out = action_request(
        &env,
        "execute_plain",
        key,
        record.get("occurrence").unwrap(),
        Some(&snap),
    );
    assert!(!out.status.success());
    let first: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(first.get("record"), Some(&json!("remedy_error")));
    assert_eq!(first.get("code"), Some(&json!("refused")));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(key));
    assert!(!env.repo().join(key).exists());
    assert_eq!(
        std::fs::read(env.state_dir().join("state.toml")).unwrap(),
        state
    );
    assert_eq!(
        std::fs::read(env.repo().join(".dotman.toml")).unwrap(),
        config
    );
}

#[test]
fn plain_review_refuses_binary_secret_names() {
    let env = fixture();
    let key = ".config/app/.env";
    env.write(key, b"binary\0value", 0o600);
    assert!(
        !action_request(
            &env,
            "review_plain",
            key,
            &json!({"scope":"new","source":null,"reason":"secret_binary","finding":null}),
            None
        )
        .status
        .success()
    );
}

#[cfg(target_os = "linux")]
fn held_managed() -> (TestEnv, String) {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".profile", b"original\n", 0o600);
    env.ok(&["add", ".profile"]);
    let secret = format!(
        "token={}\n",
        concat!("gh", "p_", "a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2")
    );
    env.write(".profile", secret.as_bytes(), 0o600);
    (env, secret)
}

#[test]
#[cfg(target_os = "linux")]
fn forgets_one_held_managed_key_with_private_backup() {
    use std::os::unix::fs::PermissionsExt;
    let (env, secret) = held_managed();
    let out = action_request(
        &env,
        "review_forget",
        ".profile",
        &content_occurrence("home", Some(".profile")),
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&secret));
    let record: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(
        record.get("occurrence"),
        Some(&content_occurrence("home", Some(".profile")))
    );
    assert_eq!(record.get("source_count"), Some(&json!(1)));
    assert_eq!(record.get("home_preserved"), Some(&json!(true)));
    let out = action_request(
        &env,
        "execute_forget",
        ".profile",
        record.get("occurrence").unwrap(),
        record.get("snapshot"),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&secret));
    assert_eq!(
        std::fs::read(env.home().join(".profile")).unwrap(),
        secret.as_bytes()
    );
    assert!(!env.repo().join(".profile").exists());
    let backups = env.backups(".dotman/repo/.profile");
    assert_eq!(backups.len(), 1);
    assert_eq!(backups.first().unwrap().0, b"original\n");
    assert_eq!(backups.first().unwrap().1 & 0o777, 0o600);
    assert_eq!(
        std::fs::metadata(env.home().join(".profile"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
#[cfg(target_os = "linux")]
fn forget_refuses_nonheld_and_a_new_variant() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".profile", b"original\n", 0o600);
    env.ok(&["add", ".profile"]);
    assert!(
        !action_request(
            &env,
            "review_forget",
            ".profile",
            &content_occurrence("home", Some(".profile")),
            None
        )
        .status
        .success()
    );
    let (env, _) = held_managed();
    let out = action_request(
        &env,
        "review_forget",
        ".profile",
        &content_occurrence("home", Some(".profile")),
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    env.write_repo(".profile{host=other}", b"variant");
    let state = std::fs::read(env.state_dir().join("state.toml")).unwrap();
    let out = action_request(
        &env,
        "execute_forget",
        ".profile",
        record.get("occurrence").unwrap(),
        record.get("snapshot"),
    );
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read(env.repo().join(".profile")).unwrap(),
        b"original\n"
    );
    assert_eq!(
        std::fs::read(env.repo().join(".profile{host=other}")).unwrap(),
        b"variant"
    );
    assert_eq!(env.backups(".dotman/repo/.profile"), Vec::new());
    assert_eq!(
        std::fs::read(env.state_dir().join("state.toml")).unwrap(),
        state
    );
}

#[test]
#[cfg(target_os = "linux")]
fn forget_saves_a_repo_only_held_edit() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".profile", b"original\n", 0o600);
    env.ok(&["add", ".profile"]);
    let secret = format!(
        "token={}\n",
        concat!("gh", "p_", "a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2")
    );
    let repo = env.repo().to_string_lossy().into_owned();
    env.git(&["-C", &repo, "add", ".profile"]);
    env.write_repo(".profile", secret.as_bytes());
    let out = action_request(
        &env,
        "review_forget",
        ".profile",
        &content_occurrence("repo", Some(".profile")),
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&secret));
    let record: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(
        record.get("occurrence"),
        Some(&content_occurrence("repo", Some(".profile")))
    );
    let out = action_request(
        &env,
        "execute_forget",
        ".profile",
        record.get("occurrence").unwrap(),
        record.get("snapshot"),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(env.home().join(".profile")).unwrap(),
        b"original\n"
    );
    assert_eq!(
        env.backups(".dotman/repo/.profile").first().unwrap().0,
        secret.as_bytes()
    );
}

#[test]
#[cfg(target_os = "linux")]
fn forget_review_requires_the_exact_home_occurrence() {
    let (env, _) = held_managed();
    let occurrence = json!({"scope":"home","source":".profile","reason":"content",
        "finding":{"rule":"github-token","place":"content","line":1}});
    let request = |occurrence: &Value| {
        let input = json!({"schema_version":1,"operation":"review_forget","key":".profile",
            "occurrence": occurrence});
        env.run_stdin(
            &[],
            input.to_string().as_bytes(),
            &["remedy", "--from", "-", "--json"],
        )
    };
    let out = request(&occurrence);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: Value =
        serde_json::from_slice(out.stdout.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
    assert_eq!(record.get("occurrence"), Some(&occurrence));
    let wrong = json!({"scope":"home","source":".other","reason":"content",
        "finding":{"rule":"github-token","place":"content","line":1}});
    assert!(!request(&wrong).status.success());
    let wrong_execute = json!({"schema_version":1,"operation":"execute_forget","key":".profile",
        "occurrence":wrong,"snapshot":record.get("snapshot")});
    assert!(
        !env.run_stdin(
            &[],
            wrong_execute.to_string().as_bytes(),
            &["remedy", "--from", "-", "--json"]
        )
        .status
        .success()
    );
    assert_eq!(
        std::fs::read(env.repo().join(".profile")).unwrap(),
        b"original\n"
    );
    let input = json!({"schema_version":1,"operation":"execute_forget","key":".profile",
        "occurrence":occurrence,"snapshot":record.get("snapshot")});
    let out = env.run_stdin(
        &[],
        input.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let receipt: Value =
        serde_json::from_slice(out.stdout.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
    assert!(
        receipt
            .pointer("/private_recovery/state_backup_run")
            .is_some()
    );
    assert!(!env.repo().join(".profile").exists());
    assert_eq!(
        env.backups(".dotman/repo/.profile").first().unwrap().0,
        b"original\n"
    );
    let repeated = env.run_stdin(
        &[],
        input.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    );
    assert!(!repeated.status.success());
    assert_eq!(env.backups(".dotman/repo/.profile").len(), 1);
}

#[test]
#[cfg(target_os = "linux")]
fn plain_review_requires_the_exact_new_occurrence() {
    let env = fixture();
    let key = ".config/app/.env";
    env.write(key, b"API_URL=https://example.invalid\n", 0o644);
    let occurrence = json!({"scope":"new","source":null,"reason":"secret_name","finding":null});
    let input = json!({"schema_version":1,"operation":"review_plain","key":key,
        "occurrence":occurrence});
    let out = env.run_stdin(
        &[],
        input.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: Value =
        serde_json::from_slice(out.stdout.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
    assert_eq!(record.get("occurrence"), Some(&occurrence));
    let input = json!({"schema_version":1,"operation":"execute_plain","key":key,
        "occurrence":occurrence,"snapshot":record.get("snapshot")});
    let out = env.run_stdin(
        &[],
        input.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(env.repo().join(key)).unwrap(),
        b"API_URL=https://example.invalid\n"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn encrypt_review_requires_the_exact_new_occurrence() {
    if !common::age_installed() {
        return;
    }
    let env = fixture();
    env.ok(&["key", "init"]);
    env.write(".config/app/bootstrap", b"bootstrap", 0o600);
    env.ok(&["add", "--encrypt", ".config/app/bootstrap"]);
    let key = ".config/app/.env";
    env.write(key, b"API_URL=https://example.invalid\n", 0o600);
    let occurrence = json!({"scope":"new","source":null,"reason":"secret_name","finding":null});
    let input = json!({"schema_version":1,"operation":"review_encrypt","key":key,
        "occurrence":occurrence});
    let out = env.run_stdin(
        &[],
        input.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let record: Value =
        serde_json::from_slice(out.stdout.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
    assert_eq!(record.get("occurrence"), Some(&occurrence));
    assert_eq!(
        record.get("destination"),
        Some(&json!(".config/app/.env{age}"))
    );
    let input = json!({"schema_version":1,"operation":"execute_encrypt","key":key,
        "occurrence":occurrence,"snapshot":record.get("snapshot")});
    let out = env.run_stdin(
        &[],
        input.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(env.repo().join(".config/app/.env{age}").is_file());
}

#[test]
#[cfg(target_os = "linux")]
fn forget_review_checks_the_exact_tracked_repo_occurrence() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".profile", b"original\n", 0o600);
    env.ok(&["add", ".profile"]);
    let repo = env.repo().to_string_lossy().into_owned();
    env.git(&["-C", &repo, "add", ".profile"]);
    let secret = format!(
        "token={}\n",
        concat!("gh", "p_", "a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2")
    );
    env.write_repo(".profile", secret.as_bytes());
    let occurrence = json!({"scope":"repo","source":".profile","reason":"content",
        "finding":{"rule":"github-token","place":"content","line":1}});
    let input = json!({"schema_version":1,"operation":"review_forget","key":".profile",
        "occurrence":occurrence});
    let out = env.run_stdin(
        &[],
        input.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&secret));
    let record: Value =
        serde_json::from_slice(out.stdout.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
    assert_eq!(record.get("occurrence"), Some(&occurrence));
    let wrong = json!({"scope":"repo","source":".other","reason":"content",
        "finding":{"rule":"github-token","place":"content","line":1}});
    let input = json!({"schema_version":1,"operation":"review_forget","key":".profile",
        "occurrence":wrong});
    assert!(
        !env.run_stdin(
            &[],
            input.to_string().as_bytes(),
            &["remedy", "--from", "-", "--json"]
        )
        .status
        .success()
    );
    assert_eq!(
        std::fs::read(env.repo().join(".profile")).unwrap(),
        secret.as_bytes()
    );
}

fn first_tracking_record(output: &std::process::Output) -> serde_json::Result<Value> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(
        output
            .stdout
            .split(|b| *b == b'\n')
            .next()
            .unwrap_or_default(),
    )
}

fn tracking_request(
    env: &TestEnv,
    repo_id: &str,
    operation: &str,
    paths: &[&str],
    expected: Option<&str>,
) -> std::process::Output {
    let input = json!({"schema_version":1, "operation":operation, "paths":paths, "repo_id":repo_id.trim(),"expected":expected});
    env.run_stdin(
        &[],
        input.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    )
}

#[test]
fn tracking_multiple_files_reports_holds_and_redacts_sensitive_names() {
    let parsed_tracking = |output: &std::process::Output| first_tracking_record(output).unwrap();
    let env = TestEnv::new();
    env.ok(&["init"]);
    let repo_id = std::fs::read_to_string(env.repo().join(".git/dotman-id")).unwrap();
    env.write(".config/app/.env", b"example=1", 0o600);
    env.write(".editorrc", b"editor", 0o644);
    env.write(".shellrc", b"shell", 0o600);
    let selection = [".editorrc", ".shellrc", ".config/app/.env"];
    let multi = parsed_tracking(&tracking_request(
        &env,
        &repo_id,
        "track_files",
        &selection,
        None,
    ));
    assert_eq!(multi.get("files"), Some(&json!([".editorrc", ".shellrc"])));
    assert!(multi.pointer("/held/.config~1app~1.env").is_some());
    parsed_tracking(&tracking_request(
        &env,
        &repo_id,
        "track_files",
        &selection,
        multi.get("token").and_then(Value::as_str),
    ));
    assert_eq!(
        std::fs::read(env.repo().join(".shellrc")).unwrap(),
        b"shell"
    );
    assert_eq!(
        std::fs::read(env.repo().join(".editorrc")).unwrap(),
        b"editor"
    );
    let repeat = parsed_tracking(&tracking_request(
        &env,
        &repo_id,
        "track_files",
        &selection,
        None,
    ));
    assert_eq!(
        repeat.get("existing"),
        Some(&json!([".editorrc", ".shellrc"]))
    );
    let protected = format!("ghp_{}", "a1B2".repeat(9));
    env.write(&protected, b"fixture", 0o600);
    let review = parsed_tracking(&tracking_request(
        &env,
        &repo_id,
        "track_files",
        &[protected.as_str()],
        None,
    ));
    assert!(!review.to_string().contains(&protected));
    assert_eq!(
        review.pointer("/held/~1[protected filename 1]"),
        Some(&json!("Protected filename"))
    );
}

#[test]
fn partial_tracking_keeps_omitted_home_files_out_of_sync() {
    for (already_captured, keep_new_rule) in [(false, false), (true, false), (false, true)] {
        let env = TestEnv::new();
        env.ok(&["init", "--no-remote"]);
        env.write("widgets/base", b"base", 0o644);
        if already_captured {
            env.ok(&["add", "widgets"]);
        }
        let keep = "widgets/eww/main.yuck";
        let omit = "widgets/eww/debug[1].log";
        env.write(keep, b"widget", 0o644);
        env.write(omit, b"local", 0o644);
        let id = std::fs::read_to_string(env.repo().join(".git/dotman-id")).unwrap();
        let folder = ["widgets/eww"];
        let files = [keep, omit];
        let (action, paths) = if already_captured {
            ("track_files", files.as_slice())
        } else {
            ("track_folder", folder.as_slice())
        };
        let read =
            || first_tracking_record(&tracking_request(&env, &id, action, paths, None)).unwrap();
        let apply = |preview: &Value, excluded: &[usize]| {
            let input = json!({"schema_version":1,"repo_id":id.trim(),"operation":action,
                "paths":paths,"expected":preview.get("token"),"excluded":excluded,"keep_folder_rule":keep_new_rule});
            env.run_stdin(
                &[],
                input.to_string().as_bytes(),
                &["remedy", "--from", "-", "--json"],
            )
        };
        let preview = read();
        assert_eq!(preview.get("files"), Some(&json!([omit, keep])));
        assert!(!apply(&preview, &[0, 1]).status.success());
        assert!(!apply(&preview, &[2]).status.success());
        env.write(omit, b"local change", 0o644);
        assert_eq!(apply(&preview, &[0]).status.code(), Some(3));
        assert!(!env.repo().join(keep).exists());
        let done = first_tracking_record(&apply(&read(), &[0])).unwrap();
        assert_eq!(done.get("files"), Some(&json!([keep])));
        let rules = if keep_new_rule {
            folder.as_slice()
        } else {
            &[]
        };
        assert_eq!(done.get("folders"), Some(&json!(rules)));
        env.write("widgets/eww/debug1.log", b"another file", 0o644);
        env.ok(&["sync"]);
        assert_eq!(std::fs::read(env.repo().join(keep)).unwrap(), b"widget");
        assert!(!env.repo().join(omit).exists());
        assert_eq!(
            std::fs::read(env.home().join(omit)).unwrap(),
            b"local change"
        );
        assert_eq!(
            env.repo().join("widgets/eww/debug1.log").exists(),
            already_captured || keep_new_rule
        );
    }
}

#[test]
fn tracking_binds_the_folder_inventory_and_keeps_local_files_on_forget() {
    let parsed_tracking = |output: &std::process::Output| first_tracking_record(output).unwrap();
    let env = TestEnv::new();
    env.ok(&["init"]);
    let repo_id = std::fs::read_to_string(env.repo().join(".git/dotman-id")).unwrap();
    env.write(".config/app/config.toml", b"mode=1", 0o600);
    env.write(".config/app/.env", b"example=1", 0o600);
    let folder = [".config/app"];
    let first = parsed_tracking(&tracking_request(
        &env,
        &repo_id,
        "track_folder",
        &folder,
        None,
    ));
    assert_eq!(
        first.get("files"),
        Some(&json!([".config/app/config.toml"]))
    );
    assert!(first.pointer("/held/.config~1app~1.env").is_some());
    env.write(".config/app/later", b"later", 0o644);
    assert_eq!(
        tracking_request(
            &env,
            &repo_id,
            "track_folder",
            &folder,
            first.get("token").and_then(Value::as_str)
        )
        .status
        .code(),
        Some(3)
    );
    assert!(!env.repo().join(".config/app/config.toml").exists());
    let current = parsed_tracking(&tracking_request(
        &env,
        &repo_id,
        "track_folder",
        &folder,
        None,
    ));
    let receipt = parsed_tracking(&tracking_request(
        &env,
        &repo_id,
        "track_folder",
        &folder,
        current.get("token").and_then(Value::as_str),
    ));
    assert_eq!(receipt.get("record"), Some(&json!("tracking_done")));
    assert!(!env.repo().join(".config/app/.env").exists());

    let file = [".config/app/config.toml"];
    let stop = parsed_tracking(&tracking_request(
        &env,
        &repo_id,
        "stop_tracking",
        &file,
        None,
    ));
    std::fs::write(env.repo().join(".config/app/config.toml"), b"repo edit").unwrap();
    assert_eq!(
        tracking_request(
            &env,
            &repo_id,
            "stop_tracking",
            &file,
            stop.get("token").and_then(Value::as_str)
        )
        .status
        .code(),
        Some(3)
    );
    let stop = parsed_tracking(&tracking_request(
        &env,
        &repo_id,
        "stop_tracking",
        &file,
        None,
    ));
    parsed_tracking(&tracking_request(
        &env,
        &repo_id,
        "stop_tracking",
        &file,
        stop.get("token").and_then(Value::as_str),
    ));
    assert_eq!(
        std::fs::read(env.home().join(".config/app/config.toml")).unwrap(),
        b"mode=1"
    );
    assert!(!env.repo().join(".config/app/config.toml").exists());
    assert!(
        env.repo_config()
            .ignore
            .patterns
            .iter()
            .any(|p| p == ".config/app/config.toml")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn desktop_origin_reads_effective_routes_and_reviews_local_config() {
    let env = TestEnv::new();
    env.ok(&["init", "--no-remote"]);
    let id = std::fs::read_to_string(env.repo().join(".git/dotman-id")).unwrap();
    let request = |change: Value, expected: Value| {
        let data = json!({"schema_version":1,"operation":"sync_destination","repo_id":id.trim(),"change":change,"expected":expected});
        env.run_stdin(
            &[],
            data.to_string().as_bytes(),
            &["remedy", "--from", "-", "--json"],
        )
    };
    let view = |change, token| {
        let result = request(change, token);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        serde_json::from_slice::<Value>(result.stdout.split(|b| *b == b'\n').next().unwrap())
            .unwrap()
    };
    let git = |args: &[&str]| {
        let path = env.repo();
        let mut in_repo = vec!["-C", path.to_str().unwrap()];
        in_repo.extend(args);
        env.git(&in_repo).trim_end().to_owned()
    };
    let none = Value::Null;
    let initial = view(none.clone(), none.clone());
    assert_eq!(initial.get("push"), Some(&json!([])));
    let change = json!({"fetch":"ssh://git@github.com/example/fetch.git","push":"git@github.com:example/push.git"});
    let config = env.repo().join(".git/config");
    let original = std::fs::read(&config).unwrap();
    let preview = view(change.clone(), none.clone());
    assert_eq!(original, std::fs::read(&config).unwrap());
    assert_eq!(preview, initial);
    let applied = view(change.clone(), initial.get("token").unwrap().clone());
    assert_eq!(
        applied.get("push"),
        Some(&json!(["git@github.com:example/push.git"]))
    );
    assert_eq!(
        git(&["remote", "get-url", "origin"]),
        "ssh://git@github.com/example/fetch.git"
    );
    assert_eq!(
        git(&["remote", "get-url", "--push", "origin"]),
        "git@github.com:example/push.git"
    );
    assert!(
        !request(change.clone(), initial.get("token").unwrap().clone())
            .status
            .success()
    );
    let shared = view(
        json!({"fetch":"git@github.com:example/shared.git","push":null}),
        applied.get("token").unwrap().clone(),
    );
    assert_eq!(shared.get("separate_push"), Some(&json!(false)));
    assert_eq!(shared.get("fetch"), shared.get("push"));
    git(&[
        "config",
        "--add",
        "remote.origin.pushurl",
        "git@github.com:example/one",
    ]);
    git(&[
        "config",
        "--add",
        "remote.origin.pushurl",
        concat!("https://user:", "private", "@example.invalid/two"),
    ]);
    let multiple = view(none.clone(), none);
    assert_eq!(multiple.get("editable"), Some(&json!(false)));
    assert_eq!(
        multiple.get("push"),
        Some(&json!(["git@github.com:example/one", null]))
    );
    assert!(!multiple.to_string().contains("private@"));
    assert!(
        !request(change, multiple.get("token").unwrap().clone())
            .status
            .success()
    );
}

#[test]
fn folder_review_and_later_sync_respect_local_discovery_rules() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let id = std::fs::read_to_string(env.repo().join(".git/dotman-id")).unwrap();
    let folder = [".config/eww"];
    let write = |key: &str, data: &[u8]| env.write(format!(".config/eww/{key}"), data, 0o644);
    for key in [
        "eww.yuck",
        ".omca/rules/keep.md",
        ".claude/settings.json",
        "README.md",
        ".ruff_cache/CACHEDIR.TAG",
        "scripts/.ruff_cache/.gitignore",
        ".omca/work/local.json",
        ".claude/agent-memory/a.md",
        "CLAUDE.md",
        "AGENTS.md",
    ] {
        write(key, b"fixture");
    }
    write(".omca/.gitignore", b"*\n!/rules/\n!/rules/**\n");
    let review = |expected| tracking_request(&env, &id, "track_folder", &folder, expected);
    let before = first_tracking_record(&review(None)).unwrap();
    assert_eq!(
        before.get("files"),
        Some(&json!([
            ".config/eww/.claude/settings.json",
            ".config/eww/.omca/rules/keep.md",
            ".config/eww/README.md",
            ".config/eww/eww.yuck"
        ]))
    );
    for held in [
        ".ruff_cache",
        "scripts/.ruff_cache",
        ".claude/agent-memory",
        ".omca/work",
        "CLAUDE.md",
        "AGENTS.md",
    ] {
        assert!(
            before
                .get("held")
                .and_then(|paths| paths.get(format!(".config/eww/{held}")))
                .is_some(),
            "{held}"
        );
    }
    write(".omca/.gitignore", b"*\n");
    assert_eq!(
        review(before.get("token").and_then(Value::as_str))
            .status
            .code(),
        Some(3)
    );
    let current = first_tracking_record(&review(None)).unwrap();
    write(".ruff_cache/local", b"new scratch");
    first_tracking_record(&review(current.get("token").and_then(Value::as_str))).unwrap();
    write("scripts/.ruff_cache/later", b"new cache");
    write(".omca/work/later", b"new agent state");
    write("later.yuck", b"source");
    env.ok(&["sync"]);
    assert!(env.repo().join(".config/eww/later.yuck").exists());
    for excluded in [
        ".ruff_cache",
        "scripts/.ruff_cache",
        ".omca/work",
        "AGENTS.md",
        "CLAUDE.md",
    ] {
        assert!(!env.repo().join(".config/eww").join(excluded).exists());
    }
    env.ok(&["add", ".config/eww/AGENTS.md"]);
    write("AGENTS.md", b"changed instructions");
    env.ok(&["sync"]);
    assert_eq!(
        std::fs::read(env.repo().join(".config/eww/AGENTS.md")).unwrap(),
        b"changed instructions"
    );
}

#[test]
fn kept_local_survives_content_changes_and_sync_until_released() -> anyhow::Result<()> {
    let env = fixture();
    let key = ".config/app/data";
    let state = env.state_dir().join("state.toml");
    let id = std::fs::read_to_string(env.repo().join(".git/dotman-id"))?;
    let change = |operation: &str, key: &str, repo_id: &str| {
        let request = json!({"schema_version":1, "operation":operation, "key":key,
            "repo_id":repo_id,"repo":env.repo(),"home":env.home(),"state":state});
        env.run_stdin(
            &[],
            request.to_string().as_bytes(),
            &["remedy", "--from", "-", "--json"],
        )
    };
    assert!(
        !change("keep_local", key, "another-repository")
            .status
            .success()
    );
    assert!(change("keep_local", key, id.trim()).status.success());
    let private = ".config/app/_tokens.scss";
    env.write(private, b"local palette", 0o644);
    assert!(change("keep_local", private, id.trim()).status.success());
    assert!(!std::fs::read_to_string(&state)?.contains(private));
    env.write(key, b"now eligible text", 0o644);
    env.write(".config/app/other", b"sibling", 0o644);
    env.ok(&["sync"]);
    env.ok(&["sync"]);
    assert!(!env.repo().join(key).exists());
    assert!(!env.repo().join(private).exists());
    assert_eq!(
        std::fs::read(env.repo().join(".config/app/other"))?,
        b"sibling"
    );
    let output = env.run(&["status", "--json"]);
    assert!(output.status.success());
    let records = std::str::from_utf8(&output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<Vec<Value>, _>>()?;
    let saved = records
        .iter()
        .find_map(|r| r.get("kept_local"))
        .and_then(Value::as_object)
        .unwrap();
    let (digest, _) = saved
        .iter()
        .find(|(_, name)| name.as_str() == Some(key))
        .unwrap();
    assert!(change("review_again", digest, id.trim()).status.success());
    env.ok(&["sync"]);
    assert_eq!(std::fs::read(env.repo().join(key))?, b"now eligible text");
    assert!(!env.repo().join(private).exists());
    Ok(())
}
