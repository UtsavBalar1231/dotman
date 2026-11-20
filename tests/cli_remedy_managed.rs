mod common;

use common::TestEnv;
use serde_json::{Value, json};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn request(env: &TestEnv, input: &Value) -> std::process::Output {
    env.run_stdin(
        &[],
        input.to_string().as_bytes(),
        &["remedy", "--from", "-", "--json"],
    )
}

fn setup() -> anyhow::Result<(TestEnv, String)> {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".profile", b"safe base\n", 0o644);
    env.ok(&["add", ".profile"]);
    let repo = env.repo().to_string_lossy().into_owned();
    env.git(&["-C", &repo, "add", ".profile"]);
    env.git(&["-C", &repo, "commit", "-m", "fixture"]);
    env.ok(&["key", "init"]);
    let backups = env.state_dir().join("backups");
    std::fs::create_dir_all(&backups)?;
    std::fs::set_permissions(&backups, std::fs::Permissions::from_mode(0o700))?;
    let secret = format!(
        "token={}\n",
        concat!("gh", "p_", "a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2")
    );
    env.write(".profile", secret.as_bytes(), 0o600);
    Ok((env, secret))
}

fn occurrence() -> Value {
    json!({"scope":"home","source":".profile","reason":"content",
        "finding":{"rule":"github-token","place":"content","line":1}})
}

#[test]
fn managed_home_hold_encrypts_and_preserves_the_original_inode() {
    if !common::age_installed() {
        return;
    }
    let (env, secret) = setup().unwrap();
    let inode = std::fs::metadata(env.repo().join(".profile"))
        .unwrap()
        .ino();
    let config = std::fs::read(env.repo().join(".dotman.toml")).unwrap();
    let input = json!({"schema_version":1,"operation":"review_encrypt","key":".profile",
        "occurrence":occurrence()});
    let out = request(&env, &input);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains(&secret));
    let row: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(
        row.pointer("/snapshot/review/kind"),
        Some(&json!("managed"))
    );
    assert_eq!(
        row.pointer("/managed_consequences/mode_change"),
        Some(&json!("none"))
    );
    assert_eq!(
        row.pointer("/managed_consequences/creates_recipient_record"),
        Some(&json!(true))
    );
    assert_eq!(
        row.get("recipients")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1)
    );
    let input = json!({"schema_version":1,"operation":"execute_encrypt","key":".profile",
        "occurrence":occurrence(),"snapshot":row.get("snapshot").unwrap()});
    let out = request(&env, &input);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    let run = result
        .pointer("/private_recovery/state_backup_run")
        .and_then(Value::as_str)
        .unwrap();
    let backup = env.state_dir().join("backups").join(run).join("source");
    assert_eq!(std::fs::metadata(&backup).unwrap().ino(), inode);
    assert_eq!(std::fs::read(backup).unwrap(), b"safe base\n");
    assert_eq!(
        std::fs::read(env.home().join(".profile")).unwrap(),
        secret.as_bytes()
    );
    assert!(!env.repo().join(".profile").exists());
    assert!(env.repo().join(".profile{age}").is_file());
    assert!(env.repo().join(".dotman/age-recipients").is_file());
    assert_eq!(
        std::fs::read(env.repo().join(".dotman.toml")).unwrap(),
        config
    );
    let state = dotman::state::State::read(&env.state_dir().join("state.toml"))
        .unwrap()
        .unwrap();
    assert_eq!(
        state
            .files
            .get(".profile")
            .map(|entry| entry.source.as_str()),
        Some(".profile{age}")
    );
}

#[test]
fn managed_review_refuses_an_unreviewed_source_mode_change() {
    if !common::age_installed() {
        return;
    }
    let (env, _) = setup().unwrap();
    let config_path = env.repo().join(".dotman.toml");
    let mut config = dotman::repo_config::RepoConfig::load(&config_path).unwrap();
    config
        .modes
        .insert(".profile".to_owned(), "0600".to_owned());
    config.save(&config_path, false).unwrap();
    let before = std::fs::read(&config_path).unwrap();
    let input = json!({"schema_version":1,"operation":"review_encrypt","key":".profile",
        "occurrence":occurrence()});
    let out = request(&env, &input);
    assert!(!out.status.success());
    assert_eq!(std::fs::read(config_path).unwrap(), before);
    assert_eq!(
        std::fs::read(env.repo().join(".profile")).unwrap(),
        b"safe base\n"
    );
    assert!(!env.repo().join(".profile{age}").exists());
}

#[test]
fn managed_review_refuses_changed_home_or_new_variant() {
    if !common::age_installed() {
        return;
    }
    for changed in ["home", "variant", "recipients"] {
        let (env, _) = setup().unwrap();
        let input = json!({"schema_version":1,"operation":"review_encrypt","key":".profile",
            "occurrence":occurrence()});
        let out = request(&env, &input);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let row: Value =
            serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
        if changed == "home" {
            env.write(".profile", b"changed\n", 0o600);
        } else if changed == "variant" {
            env.write_repo(".profile{host=other}", b"variant");
        } else {
            let config_path = env.repo().join(".dotman.toml");
            let mut config = dotman::repo_config::RepoConfig::load(&config_path).unwrap();
            config.age.recipients.push("age1invalid".to_owned());
            config.save(&config_path, false).unwrap();
        }
        let input = json!({"schema_version":1,"operation":"execute_encrypt","key":".profile",
            "occurrence":occurrence(),"snapshot":row.get("snapshot").unwrap()});
        let out = request(&env, &input);
        assert!(!out.status.success());
        assert_eq!(
            std::fs::read(env.repo().join(".profile")).unwrap(),
            b"safe base\n"
        );
        assert!(!env.repo().join(".profile{age}").exists());
    }
}

#[test]
fn repo_held_conversion_requires_the_same_physical_bytes() {
    if !common::age_installed() {
        return;
    }
    let (env, secret) = setup().unwrap();
    env.write_repo(".profile", secret.as_bytes());
    let state_path = env.state_dir().join("state.toml");
    let mut state = dotman::state::State::read(&state_path).unwrap().unwrap();
    state.files.get_mut(".profile").unwrap().hash =
        format!("{:032x}", xxhash_rust::xxh3::xxh3_128(secret.as_bytes()));
    state.save(&state_path, false).unwrap();
    let occurrence = json!({"scope":"repo","source":".profile","reason":"content",
        "finding":{"rule":"github-token","place":"content","line":1}});
    let input = json!({"schema_version":1,"operation":"review_encrypt","key":".profile",
        "occurrence":occurrence});
    let out = request(&env, &input);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    env.write(".profile", format!("{secret}changed\n").as_bytes(), 0o600);
    assert!(!request(&env, &input).status.success());
    let input = json!({"schema_version":1,"operation":"execute_encrypt","key":".profile",
        "occurrence":occurrence,"snapshot":row.get("snapshot").unwrap()});
    let out = request(&env, &input);
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read(env.repo().join(".profile")).unwrap(),
        secret.as_bytes()
    );
    assert!(!env.repo().join(".profile{age}").exists());
}

#[test]
fn managed_0644_home_is_preserved_and_future_mode_is_disclosed() {
    if !common::age_installed() {
        return;
    }
    let (env, secret) = setup().unwrap();
    env.write(".profile", secret.as_bytes(), 0o644);
    let input = json!({"schema_version":1,"operation":"review_encrypt","key":".profile",
        "occurrence":occurrence()});
    let out = request(&env, &input);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert!(
        row.pointer("/managed_consequences/mode_consequence")
            .and_then(Value::as_str)
            .unwrap()
            .contains("0600")
    );
    let input = json!({"schema_version":1,"operation":"execute_encrypt","key":".profile",
        "occurrence":occurrence(),"snapshot":row.get("snapshot").unwrap()});
    let out = request(&env, &input);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(env.home().join(".profile")).unwrap(),
        secret.as_bytes()
    );
    assert_eq!(
        std::fs::metadata(env.home().join(".profile"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644
    );
}

#[test]
fn recovery_directory_is_created_only_on_confirmed_execution() {
    if !common::age_installed() {
        return;
    }
    let (env, _) = setup().unwrap();
    let backups = env.state_dir().join("backups");
    std::fs::remove_dir(&backups).unwrap();
    let input = json!({"schema_version":1,"operation":"review_encrypt","key":".profile",
        "occurrence":occurrence()});
    let out = request(&env, &input);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(
        row.pointer("/managed_consequences/creates_backup_directory"),
        Some(&json!(true))
    );
    assert!(!backups.exists());
    let input = json!({"schema_version":1,"operation":"execute_encrypt","key":".profile",
        "occurrence":occurrence(),"snapshot":row.get("snapshot").unwrap()});
    let out = request(&env, &input);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row: Value =
        serde_json::from_slice(out.stdout.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(row.get("backup_dir_created"), Some(&json!(true)));
    assert!(backups.is_dir());
}
