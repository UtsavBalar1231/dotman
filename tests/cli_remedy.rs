mod common;

use common::TestEnv;
use serde_json::{Value, json};

fn new_name_occurrence() -> Value {
    json!({"scope":"new","source":null,"reason":"secret_name","finding":null})
}

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
