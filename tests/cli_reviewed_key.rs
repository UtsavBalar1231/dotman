#![cfg(target_os = "linux")]

mod common;

use common::{R, TestEnv};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Seek, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn replace(value: &mut Value, pointer: &str, replacement: Value) -> R {
    *value.pointer_mut(pointer).ok_or(pointer)? = replacement;
    Ok(())
}

#[test]
fn reviewed_create_refuses_control_directories() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let config_dir = env.xdg_config().join("dotman");
    fs::create_dir_all(&config_dir)?;
    fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))?;
    let config = config_dir.join("config.toml");
    let repo = env.repo();
    let state = env.xdg_state().join("dotman");
    fs::create_dir_all(&state)?;
    fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
    let nested_repo = repo.join("nested");
    let nested_state = state.join("nested");
    let reserved = env.home().join(".dotman");
    let managed = env.home().join("managed");
    let alias = env.root().join("repo-alias");
    std::os::unix::fs::symlink(&repo, &alias)?;
    for dir in [&nested_repo, &nested_state, &reserved, &managed] {
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    fs::write(
        repo.join(".dotman.toml"),
        "[dirs]\npaths = [\"managed/\"]\n",
    )?;
    let id = fs::read_to_string(repo.join(".git/dotman-id"))?;
    for parent in [
        &repo,
        &state,
        &nested_repo,
        &nested_state,
        &reserved,
        &managed,
        &alias,
    ] {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        let identity = parent.join("absent-identity");
        fs::write(
            &config,
            format!("[age]\nidentity = {:?}\n", identity.to_string_lossy()),
        )?;
        let config_meta = fs::metadata(&config)?;
        let parent_meta = fs::metadata(parent)?;
        let request = json!({
            "schema_version": 1, "repo_id": id.trim(),
            "home": {"raw": env.home()}, "repo": {"raw": repo},
            "config": {"raw": config}, "identity": {"raw": identity},
            "config_observation": {
                "dev": config_meta.dev(), "ino": config_meta.ino(), "size": config_meta.size(),
                "mtime": config_meta.mtime(), "mtime_nsec": config_meta.mtime_nsec(),
                "ctime": config_meta.ctime(), "ctime_nsec": config_meta.ctime_nsec()
            },
            "parent_dev": parent_meta.dev(), "parent_ino": parent_meta.ino(),
            "operation": {"kind": "create"}
        });
        let out = env.run_stdin(&[], &serde_json::to_vec(&request)?, &["key", "reviewed"]);
        assert!(!out.status.success(), "{}", common::text(&out));
        assert_eq!(out.stdout, [] as [u8; 0]);
        assert!(!identity.exists());
    }
    Ok(())
}

#[test]
fn reviewed_grant_preserves_original_and_leaves_already_listed_settings_untouched() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let parent = env.xdg_config().join("dotman");
    fs::create_dir_all(&parent)?;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
    let identity = parent.join("age-identity.txt");
    let parent_meta = fs::metadata(&parent)?;
    let id = fs::read_to_string(env.repo().join(".git/dotman-id"))?;
    let mut request = json!({
        "schema_version": 1, "repo_id": id.trim(),
        "home": {"raw": env.home()}, "repo": {"raw": env.repo()},
        "config": {"raw": parent.join("config.toml")}, "identity": {"raw": identity},
        "config_observation": null, "parent_dev": parent_meta.dev(), "parent_ino": parent_meta.ino(),
        "operation": {"kind": "create"}
    });
    let created = env.run_stdin(&[], &serde_json::to_vec(&request)?, &["key", "reviewed"]);
    assert!(created.status.success(), "{}", common::text(&created));
    let receipt: serde_json::Value = serde_json::from_slice(&created.stdout)?;
    let key = fs::read(&identity)?;
    let settings = env.repo().join(".dotman.toml");
    let settings_text = "# preserve header\n[ignore]\npatterns = [\"old/\"] # preserve unrelated\n[age]\nrecipients = [\"age1one\", \"age1two\"] # preserve recipients\n";
    fs::write(&settings, settings_text)?;
    fs::set_permissions(&settings, fs::Permissions::from_mode(0o640))?;
    let mut original_fd = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&settings)?;
    let before = fs::read(&settings)?;
    *request.get_mut("operation").ok_or("missing operation")? = json!({"kind": "grant_preflight"});
    let preflight = env.run_stdin(&[], &serde_json::to_vec(&request)?, &["key", "reviewed"]);
    let state_dir = env.xdg_state().join("dotman");
    let review = verify_grant_preflight(&preflight, &receipt, &state_dir, &key)?;
    assert_eq!(fs::read(&settings)?, before);
    let identity_meta = fs::metadata(&identity)?;
    let settings_meta = fs::metadata(&settings)?;
    let observe = |meta: &fs::Metadata| {
        json!({
            "dev": meta.dev(), "ino": meta.ino(), "size": meta.size(),
            "mtime": meta.mtime(), "mtime_nsec": meta.mtime_nsec(),
            "ctime": meta.ctime(), "ctime_nsec": meta.ctime_nsec()
        })
    };
    let recipients = receipt
        .get("recipients")
        .ok_or("missing recipients")?
        .clone();
    *request.get_mut("operation").ok_or("missing operation")? = json!({
        "kind": "grant", "identity_observation": observe(&identity_meta),
        "repo_config_observation": observe(&settings_meta), "recipients": recipients,
        "repo_dev": review.get("repo_dev"), "repo_ino": review.get("repo_ino"),
        "repo_config_digest": review.get("repo_config_digest"),
        "recovery_parent": {"raw": state_dir},
        "recovery_parent_dev": review.get("recovery_parent_dev"),
        "recovery_parent_ino": review.get("recovery_parent_ino")
    });
    let original = fs::read(&settings)?;
    let fixture = GrantFixture {
        env: &env,
        identity,
        settings,
        state_dir,
        key,
    };
    fixture.reject_changes_to_recovery_parent(&request, &original)?;
    fixture.verify_publication(
        &request,
        &original,
        &settings_meta,
        &mut original_fd,
        &recipients,
    )?;
    fixture.verify_already_listed(request, &recipients)
}

fn verify_grant_preflight(
    preflight: &Output,
    receipt: &Value,
    state_dir: &Path,
    key: &[u8],
) -> R<Value> {
    assert!(preflight.status.success(), "{}", common::text(preflight));
    let review: Value = serde_json::from_slice(&preflight.stdout)?;
    assert_eq!(
        review.get("record").and_then(Value::as_str),
        Some("grant_preflight")
    );
    assert_eq!(review.get("recipients"), receipt.get("recipients"));
    assert_eq!(
        review.get("recipient_present").and_then(Value::as_bool),
        Some(false)
    );
    let state_meta = fs::metadata(state_dir)?;
    assert_eq!(
        review.pointer("/recovery_parent/raw"),
        Some(&json!(state_dir))
    );
    assert_eq!(
        review.get("recovery_parent_dev").and_then(Value::as_u64),
        Some(state_meta.dev())
    );
    assert_eq!(
        review.get("recovery_parent_ino").and_then(Value::as_u64),
        Some(state_meta.ino())
    );
    assert!(
        review
            .get("repo_config_digest")
            .and_then(Value::as_str)
            .is_some()
    );
    assert!(
        !preflight
            .stdout
            .windows(key.len())
            .any(|window| window == key)
    );
    Ok(review)
}

struct GrantFixture<'a> {
    env: &'a TestEnv,
    identity: PathBuf,
    settings: PathBuf,
    state_dir: PathBuf,
    key: Vec<u8>,
}

impl GrantFixture<'_> {
    fn reject_changes_to_recovery_parent(&self, request: &Value, original: &[u8]) -> R {
        let Self {
            env,
            identity,
            settings,
            state_dir,
            key,
        } = self;
        let mut altered = request.clone();
        replace(
            &mut altered,
            "/operation/recovery_parent",
            json!({"raw": env.xdg_state().join("other")}),
        )?;
        let denied = env.run_stdin(&[], &serde_json::to_vec(&altered)?, &["key", "reviewed"]);
        assert!(!denied.status.success());
        altered = request.clone();
        replace(&mut altered, "/operation/recovery_parent_ino", json!(0))?;
        let denied = env.run_stdin(&[], &serde_json::to_vec(&altered)?, &["key", "reviewed"]);
        assert!(!denied.status.success());
        let alternate_state = env.root().join("alternate-state");
        fs::create_dir(&alternate_state)?;
        let denied = env.run_stdin(
            &[(
                "XDG_STATE_HOME",
                alternate_state.to_str().ok_or("invalid path")?,
            )],
            &serde_json::to_vec(request)?,
            &["key", "reviewed"],
        );
        assert!(!denied.status.success());
        assert!(!alternate_state.join("dotman").exists());
        let renamed = env.xdg_state().join("original-state");
        fs::rename(state_dir, &renamed)?;
        fs::create_dir(state_dir)?;
        fs::set_permissions(state_dir, fs::Permissions::from_mode(0o700))?;
        let denied = env.run_stdin(&[], &serde_json::to_vec(request)?, &["key", "reviewed"]);
        assert!(!denied.status.success());
        assert_eq!(fs::read(settings)?, original);
        assert_eq!(&fs::read(identity)?, key);
        fs::remove_dir(state_dir)?;
        fs::rename(&renamed, state_dir)?;
        Ok(())
    }

    fn verify_publication(
        &self,
        request: &Value,
        original: &[u8],
        settings_meta: &fs::Metadata,
        original_fd: &mut fs::File,
        recipients: &Value,
    ) -> R {
        let Self {
            env,
            identity,
            settings,
            key,
            ..
        } = self;
        let granted = env.run_stdin(&[], &serde_json::to_vec(request)?, &["key", "reviewed"]);
        assert!(granted.status.success(), "{}", common::text(&granted));
        let mut records = granted.stdout.split(|b| *b == b'\n');
        let recovery: Value = serde_json::from_slice(records.next().ok_or("missing recovery")?)?;
        assert_eq!(
            recovery.get("record").and_then(Value::as_str),
            Some("settings_recovery")
        );
        assert_eq!(
            recovery.get("phase").and_then(Value::as_str),
            Some("prepared")
        );
        let published: Value =
            serde_json::from_slice(records.next().ok_or("missing publication")?)?;
        assert_eq!(
            published.get("record").and_then(Value::as_str),
            Some("settings_recovery")
        );
        assert_eq!(
            published.get("phase").and_then(Value::as_str),
            Some("published")
        );
        assert_eq!(published.get("directory"), recovery.get("directory"));
        let recovery_dir = recovery
            .pointer("/directory/raw")
            .and_then(Value::as_str)
            .ok_or("missing recovery path")?;
        assert_eq!(
            fs::read(std::path::Path::new(recovery_dir).join("reviewed"))?,
            original
        );
        let displaced = std::path::Path::new(recovery_dir).join("candidate");
        assert_eq!(fs::read(&displaced)?, original);
        assert_eq!(
            fs::metadata(&displaced)?.ino(),
            original_fd.metadata()?.ino()
        );
        assert_eq!(fs::metadata(&displaced)?.ino(), settings_meta.ino());
        assert_ne!(fs::metadata(settings)?.ino(), settings_meta.ino());
        assert_eq!(fs::metadata(settings)?.mode() & 0o777, 0o640);
        let updated = fs::read_to_string(settings)?;
        assert!(updated.contains("# preserve header"));
        assert!(updated.contains("patterns = [\"old/\"] # preserve unrelated"));
        assert!(updated.contains("# preserve recipients"));
        assert_eq!(
            env.repo_config().age.recipients,
            vec![
                "age1one".to_string(),
                "age1two".to_string(),
                recipients
                    .get(0)
                    .and_then(|v| v.as_str())
                    .ok_or("missing recipient")?
                    .to_string()
            ]
        );
        let mut old_bytes = String::new();
        original_fd.read_to_string(&mut old_bytes)?;
        assert_eq!(old_bytes.as_bytes(), original);
        original_fd.rewind()?;
        original_fd.write_all(b"# old fd")?;
        assert!(fs::read(&displaced)?.starts_with(b"# old fd"));
        assert_eq!(
            fs::read(std::path::Path::new(recovery_dir).join("reviewed"))?,
            original
        );
        assert_eq!(fs::read_to_string(settings)?, updated);
        assert_eq!(&fs::read(identity)?, key);
        Ok(())
    }

    fn verify_already_listed(&self, mut request: Value, recipients: &Value) -> R {
        let Self {
            env,
            identity,
            settings,
            state_dir,
            key,
        } = self;
        replace(
            &mut request,
            "/operation",
            json!({"kind": "grant_preflight"}),
        )?;
        let after_grant = env.run_stdin(&[], &serde_json::to_vec(&request)?, &["key", "reviewed"]);
        assert!(
            after_grant.status.success(),
            "{}",
            common::text(&after_grant)
        );
        let after_grant: serde_json::Value = serde_json::from_slice(&after_grant.stdout)?;
        assert_eq!(
            after_grant
                .get("recipient_present")
                .and_then(Value::as_bool),
            Some(true)
        );
        let recipient = recipients
            .get(0)
            .ok_or("missing recipient")?
            .as_str()
            .ok_or("missing recipient")?;
        let text = format!("# keep this comment\n[age]\nrecipients = [\"{recipient}\"] # keep\n");
        fs::write(settings, &text)?;
        fs::set_permissions(settings, fs::Permissions::from_mode(0o640))?;
        let meta = fs::metadata(settings)?;
        let refreshed = env.run_stdin(&[], &serde_json::to_vec(&request)?, &["key", "reviewed"]);
        assert!(refreshed.status.success(), "{}", common::text(&refreshed));
        let refreshed: serde_json::Value = serde_json::from_slice(&refreshed.stdout)?;
        assert_eq!(
            refreshed.get("recipient_present").and_then(Value::as_bool),
            Some(true)
        );
        *request.get_mut("operation").ok_or("missing operation")? = json!({
            "kind": "grant", "identity_observation": refreshed.get("identity_observation"),
            "repo_config_observation": refreshed.get("repo_config_observation"),
            "repo_config_digest": refreshed.get("repo_config_digest"),
            "repo_dev": refreshed.get("repo_dev"), "repo_ino": refreshed.get("repo_ino"),
            "recovery_parent": {"raw": state_dir},
            "recovery_parent_dev": refreshed.get("recovery_parent_dev"),
            "recovery_parent_ino": refreshed.get("recovery_parent_ino"),
            "recipients": recipients
        });
        let mut stale = request.clone();
        *stale
            .get_mut("operation")
            .and_then(|operation| operation.get_mut("repo_config_digest"))
            .ok_or("missing digest")? = json!("00000000000000000000000000000000");
        let denied = env.run_stdin(&[], &serde_json::to_vec(&stale)?, &["key", "reviewed"]);
        assert!(!denied.status.success());
        assert_eq!(fs::read_to_string(settings)?, text);
        assert_eq!(&fs::read(identity)?, key);
        let unchanged = env.run_stdin(&[], &serde_json::to_vec(&request)?, &["key", "reviewed"]);
        assert!(unchanged.status.success(), "{}", common::text(&unchanged));
        assert_eq!(fs::read_to_string(settings)?, text);
        assert_eq!(fs::metadata(settings)?.ino(), meta.ino());
        assert_eq!(fs::metadata(settings)?.mode() & 0o777, 0o640);
        assert_eq!(&fs::read(identity)?, key);
        Ok(())
    }
}

#[test]
fn reviewed_create_publishes_once_without_editing_recipients() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let parent = env.xdg_config().join("dotman");
    fs::create_dir_all(&parent)?;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
    let identity = parent.join("age-identity.txt");
    let config = parent.join("config.toml");
    assert!(!config.exists());
    let meta = fs::metadata(&parent)?;
    let id = fs::read_to_string(env.repo().join(".git/dotman-id"))?;
    let mut request = json!({
        "schema_version": 1,
        "repo_id": id.trim(),
        "home": {"raw": env.home()},
        "repo": {"raw": env.repo()},
        "config": {"raw": config},
        "identity": {"raw": identity},
        "config_observation": null,
        "parent_dev": meta.dev(),
        "parent_ino": meta.ino(),
        "operation": {"kind": "create"}
    });
    let repo_config = fs::read(env.repo().join(".dotman.toml"))?;
    let input = serde_json::to_vec(&request)?;
    let out = env.run_stdin(&[], &input, &["key", "reviewed"]);
    assert!(out.status.success(), "{}", common::text(&out));
    let receipt: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(
        receipt.get("record").and_then(Value::as_str),
        Some("identity")
    );
    let recipients = receipt.get("recipients").ok_or("missing recipients")?;
    assert_eq!(recipients.as_array().map(Vec::len), Some(1));
    let key = fs::read(&identity)?;
    assert_eq!(fs::metadata(&identity)?.mode() & 0o777, 0o600);
    assert!(!out.stdout.windows(key.len()).any(|part| part == key));
    assert!(!String::from_utf8_lossy(&out.stdout).contains(concat!("AGE-", "SECRET-KEY")));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(concat!("AGE-", "SECRET-KEY")));
    assert_eq!(fs::read(env.repo().join(".dotman.toml"))?, repo_config);
    let stale = env.run_stdin(&[], &input, &["key", "reviewed"]);
    assert!(!stale.status.success());
    assert_eq!(stale.stdout, [] as [u8; 0]);
    assert_eq!(fs::read(&identity)?, key);
    assert_eq!(fs::read(env.repo().join(".dotman.toml"))?, repo_config);
    let identity_meta = fs::metadata(&identity)?;
    let repo_meta = fs::metadata(env.repo().join(".dotman.toml"))?;
    replace(
        &mut request,
        "/operation",
        json!({"kind":"grant_preflight"}),
    )?;
    let reviewed = env.run_stdin(&[], &serde_json::to_vec(&request)?, &["key", "reviewed"]);
    assert!(reviewed.status.success(), "{}", common::text(&reviewed));
    let reviewed: serde_json::Value = serde_json::from_slice(&reviewed.stdout)?;
    *request.get_mut("operation").ok_or("missing operation")? = json!({
        "kind": "grant",
        "identity_observation": {
            "dev": identity_meta.dev(), "ino": identity_meta.ino(),
            "size": identity_meta.size(), "mtime": identity_meta.mtime(),
            "mtime_nsec": identity_meta.mtime_nsec(), "ctime": identity_meta.ctime(),
            "ctime_nsec": identity_meta.ctime_nsec()
        },
        "repo_config_observation": {
            "dev": repo_meta.dev(), "ino": repo_meta.ino(),
            "size": repo_meta.size(), "mtime": repo_meta.mtime(),
            "mtime_nsec": repo_meta.mtime_nsec(), "ctime": repo_meta.ctime(),
            "ctime_nsec": repo_meta.ctime_nsec()
        },
        "recipients": [],
        "repo_dev": reviewed.get("repo_dev"),
        "repo_ino": reviewed.get("repo_ino"),
        "recovery_parent": {"raw": env.xdg_state().join("dotman")},
        "recovery_parent_dev": reviewed.get("recovery_parent_dev"),
        "recovery_parent_ino": reviewed.get("recovery_parent_ino"),
        "repo_config_digest": reviewed.get("repo_config_digest")
    });
    let denied = env.run_stdin(&[], &serde_json::to_vec(&request)?, &["key", "reviewed"]);
    assert!(!denied.status.success());
    assert_eq!(fs::read(&identity)?, key);
    assert_eq!(fs::read(env.repo().join(".dotman.toml"))?, repo_config);
    replace(&mut request, "/operation/recipients", recipients.clone())?;
    let granted = env.run_stdin(&[], &serde_json::to_vec(&request)?, &["key", "reviewed"]);
    assert!(granted.status.success(), "{}", common::text(&granted));
    assert_eq!(fs::read(&identity)?, key);
    assert_eq!(
        &serde_json::to_value(env.repo_config().age.recipients)?,
        recipients
    );
    let config_after_grant = fs::read(env.repo().join(".dotman.toml"))?;
    let stale_grant = env.run_stdin(&[], &serde_json::to_vec(&request)?, &["key", "reviewed"]);
    assert!(!stale_grant.status.success());
    assert_eq!(fs::read(&identity)?, key);
    assert_eq!(
        fs::read(env.repo().join(".dotman.toml"))?,
        config_after_grant
    );
    Ok(())
}

#[test]
fn reviewed_import_refuses_stale_source_and_preserves_identity() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let parent = env.xdg_config().join("dotman");
    fs::create_dir_all(&parent)?;
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
    let source = env.root().join("source-key");
    let generated = Command::new("age-keygen").arg("-o").arg(&source).output()?;
    assert!(generated.status.success());
    let source_meta = fs::metadata(&source)?;
    let parent_meta = fs::metadata(&parent)?;
    let id = fs::read_to_string(env.repo().join(".git/dotman-id"))?;
    let identity = parent.join("age-identity.txt");
    let request = json!({
        "schema_version": 1,
        "repo_id": id.trim(),
        "home": {"raw": env.home()},
        "repo": {"raw": env.repo()},
        "config": {"raw": parent.join("config.toml")},
        "identity": {"raw": identity},
        "config_observation": null,
        "parent_dev": parent_meta.dev(),
        "parent_ino": parent_meta.ino(),
        "operation": {
            "kind": "import",
            "source": {"raw": source},
            "source_observation": {
                "dev": source_meta.dev(), "ino": source_meta.ino(),
                "size": source_meta.size(), "mtime": source_meta.mtime(),
                "mtime_nsec": source_meta.mtime_nsec(), "ctime": source_meta.ctime(),
                "ctime_nsec": source_meta.ctime_nsec()
            }
        }
    });
    let input = serde_json::to_vec(&request)?;
    let repo_config = fs::read(env.repo().join(".dotman.toml"))?;
    let mut changed = request.clone();
    replace(&mut changed, "/operation/source_observation/size", json!(0))?;
    let stale_source = env.run_stdin(&[], &serde_json::to_vec(&changed)?, &["key", "reviewed"]);
    assert!(!stale_source.status.success());
    assert!(!identity.exists());
    assert_eq!(fs::read(env.repo().join(".dotman.toml"))?, repo_config);
    let plugin = env.root().join("plugin-key");
    fs::write(&plugin, b"AGE-PLUGIN-EXAMPLE\n")?;
    fs::set_permissions(&plugin, fs::Permissions::from_mode(0o600))?;
    let plugin_meta = fs::metadata(&plugin)?;
    replace(&mut changed, "/operation/source", json!({"raw": plugin}))?;
    replace(
        &mut changed,
        "/operation/source_observation",
        json!({
            "dev": plugin_meta.dev(), "ino": plugin_meta.ino(), "size": plugin_meta.size(),
            "mtime": plugin_meta.mtime(), "mtime_nsec": plugin_meta.mtime_nsec(),
            "ctime": plugin_meta.ctime(), "ctime_nsec": plugin_meta.ctime_nsec()
        }),
    )?;
    let unsupported = env.run_stdin(&[], &serde_json::to_vec(&changed)?, &["key", "reviewed"]);
    assert!(!unsupported.status.success());
    assert!(!identity.exists());
    let link = env.root().join("source-alias");
    std::os::unix::fs::symlink(env.root(), &link)?;
    let source_observation = request
        .get("operation")
        .and_then(|operation| operation.get("source_observation"))
        .ok_or("missing source observation")?
        .clone();
    let operation = changed.get_mut("operation").ok_or("missing operation")?;
    *operation.get_mut("source").ok_or("missing source")? = json!({"raw": link.join("source-key")});
    *operation
        .get_mut("source_observation")
        .ok_or("missing source observation")? = source_observation;
    let symlinked_parent = env.run_stdin(&[], &serde_json::to_vec(&changed)?, &["key", "reviewed"]);
    assert!(!symlinked_parent.status.success());
    assert!(!identity.exists());
    let out = env.run_stdin(&[], &input, &["key", "reviewed"]);
    assert!(out.status.success(), "{}", common::text(&out));
    let key = fs::read(&source)?;
    assert_eq!(fs::read(&identity)?, key);
    assert_eq!(fs::metadata(&identity)?.mode() & 0o777, 0o600);
    assert!(!String::from_utf8_lossy(&out.stdout).contains(concat!("AGE-", "SECRET-KEY")));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(concat!("AGE-", "SECRET-KEY")));
    assert_eq!(fs::read(env.repo().join(".dotman.toml"))?, repo_config);
    fs::write(&source, b"changed")?;
    let stale = env.run_stdin(&[], &input, &["key", "reviewed"]);
    assert!(!stale.status.success());
    assert_eq!(fs::read(&identity)?, key);
    assert_eq!(fs::read(env.repo().join(".dotman.toml"))?, repo_config);
    Ok(())
}
