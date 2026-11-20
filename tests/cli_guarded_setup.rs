#![cfg(target_os = "linux")]

mod common;

use common::{R, TestEnv, tree_checksum};
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

fn request(env: &TestEnv) -> R<Value> {
    let parent = env
        .repo()
        .parent()
        .ok_or("repo has no parent")?
        .to_path_buf();
    let stat = fs::metadata(parent)?;
    Ok(json!({
        "schema_version": 1,
        "home": {"raw": env.home()},
        "repo": {"raw": env.repo()},
        "parent_dev": stat.dev(),
        "parent_ino": stat.ino(),
        "operation": {"kind": "fresh"},
    }))
}

fn run(env: &TestEnv, input: &[u8], args: &[&str]) -> Output {
    env.run_stdin(&[], input, args)
}

fn setup(env: &TestEnv, value: &Value) -> R<Output> {
    Ok(run(
        env,
        &serde_json::to_vec(value)?,
        &["init", "--setup-from", "-", "--json"],
    ))
}

fn records(out: &Output) -> R<Vec<Value>> {
    String::from_utf8(out.stdout.clone())?
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn refused(out: &Output) -> R {
    assert!(!out.status.success(), "unexpected success");
    let records = records(out)?;
    assert!(!records.iter().any(|v| v["record"] == "setup"));
    let last = records.last().ok_or("missing result record")?;
    assert_eq!(last["record"], "result");
    let exit_code = out.status.code().ok_or("process did not exit normally")?;
    assert_eq!(last["exit_code"].as_i64(), Some(i64::from(exit_code)));
    Ok(())
}

fn assert_only_lock(env: &TestEnv) {
    let state = tree_checksum(&env.xdg_state());
    for key in state.keys() {
        assert!(key == "dotman" || key == "dotman/lock", "{key}");
    }
}

fn assert_no_setup_stage(env: &TestEnv) -> R {
    let parent = env.repo().parent().ok_or("missing parent")?.to_path_buf();
    for entry in fs::read_dir(parent)? {
        assert!(
            !entry?
                .file_name()
                .to_string_lossy()
                .starts_with(".dotman-setup-")
        );
    }
    Ok(())
}

#[test]
fn static_import_previews_and_publishes_only_the_selected_working_tree() -> R {
    let env = TestEnv::new();
    let source = env.root().join("selected");
    fs::create_dir_all(source.join(".config/app"))?;
    fs::set_permissions(source.join(".config"), fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(
        source.join(".config/app"),
        fs::Permissions::from_mode(0o700),
    )?;
    fs::write(source.join(".config/app/settings"), b"theme=dark\n")?;
    fs::set_permissions(
        source.join(".config/app/settings"),
        fs::Permissions::from_mode(0o600),
    )?;
    fs::write(source.join(".normal"), b"normal\n")?;
    fs::set_permissions(source.join(".normal"), fs::Permissions::from_mode(0o664))?;
    fs::create_dir(source.join(".git"))?;
    fs::write(source.join(".git/config"), b"not consulted")?;
    let home = tree_checksum(&env.home());
    let config = tree_checksum(&env.xdg_config());
    let mut value = request(&env)?;
    *value.get_mut("operation").ok_or("operation")? =
        json!({"kind":"import_preview","source":{"raw":source}});
    let preview = setup(&env, &value)?;
    assert!(preview.status.success(), "{}", common::text(&preview));
    let rows = records(&preview)?;
    let row = rows
        .iter()
        .find(|r| r["record"] == "import_preview")
        .ok_or("missing preview")?;
    assert_eq!(row["kind"], "static_tree");
    assert_eq!(row["count"], 2);
    assert_eq!(
        row.pointer("/review/files/0/path").and_then(Value::as_str),
        Some(".config/app/settings")
    );
    assert!(!env.repo().exists());
    *value.get_mut("operation").ok_or("operation")? =
        json!({"kind":"import","source":{"raw":source},"review":row["review"]});
    let result = setup(&env, &value)?;
    assert!(result.status.success(), "{}", common::text(&result));
    let rows = records(&result)?;
    let receipt = rows
        .iter()
        .find(|r| r["record"] == "setup" && r["operation"] == "import")
        .ok_or("missing receipt")?;
    assert_eq!(
        fs::read(env.repo().join(".config/app/settings"))?,
        b"theme=dark\n"
    );
    assert_eq!(
        fs::metadata(env.repo().join(".config/app/settings"))?.mode() & 0o777,
        0o600
    );
    let config_text = fs::read_to_string(env.repo().join(".dotman.toml"))?;
    let repo_config: toml::Value = toml::from_str(&config_text)?;
    assert_eq!(
        repo_config
            .get("modes")
            .and_then(|m| m.get(".config/app/settings"))
            .and_then(toml::Value::as_str),
        Some("0600")
    );
    assert_eq!(
        repo_config
            .get("modes")
            .and_then(|m| m.get(".config/app/"))
            .and_then(toml::Value::as_str),
        Some("0700")
    );
    assert!(
        repo_config
            .get("modes")
            .and_then(|m| m.get(".normal"))
            .is_none()
    );
    assert_eq!(
        fs::metadata(env.repo().join(".normal"))?.mode() & 0o777,
        0o644
    );
    assert_eq!(
        common::git_in(&env, &env.repo(), &["rev-list", "--count", "HEAD"]).trim(),
        "1"
    );
    assert_eq!(common::git_in(&env, &env.repo(), &["remote"]).trim(), "");
    let status = env.run(&["status", "--json"]);
    assert!(status.status.success(), "{}", common::text(&status));
    let status_rows = records(&status)?;
    assert!(
        status_rows
            .iter()
            .any(|r| r["record"] == "context" && r["repo_id"] == receipt["repo_id"])
    );
    assert_eq!(tree_checksum(&env.home()), home);
    assert_eq!(tree_checksum(&env.xdg_config()), config);
    assert_only_lock(&env);
    assert_no_setup_stage(&env)?;
    Ok(())
}

#[test]
fn static_import_refuses_changed_source_and_destination() -> R {
    for change_source in [true, false] {
        let env = TestEnv::new();
        let source = env.root().join("selected");
        fs::create_dir(&source)?;
        fs::write(source.join(".sample"), b"before\n")?;
        let home = tree_checksum(&env.home());
        let mut value = request(&env)?;
        *value.get_mut("operation").ok_or("operation")? =
            json!({"kind":"import_preview","source":{"raw":source}});
        let out = setup(&env, &value)?;
        assert!(out.status.success(), "{}", common::text(&out));
        let rows = records(&out)?;
        let review = rows
            .iter()
            .find(|r| r["record"] == "import_preview")
            .ok_or("missing preview")?["review"]
            .clone();
        if change_source {
            fs::write(source.join(".sample"), b"after\n")?;
        } else {
            fs::create_dir(env.repo())?;
        }
        *value.get_mut("operation").ok_or("operation")? =
            json!({"kind":"import","source":{"raw":source},"review":review});
        refused(&setup(&env, &value)?)?;
        if change_source {
            assert!(!env.repo().exists());
        } else {
            assert!(env.repo().is_dir());
        }
        assert_eq!(tree_checksum(&env.home()), home);
        assert_only_lock(&env);
        assert_no_setup_stage(&env)?;
    }
    Ok(())
}

#[test]
fn static_import_refuses_directory_mode_changed_after_review() -> R {
    let env = TestEnv::new();
    let source = env.root().join("selected");
    fs::create_dir_all(source.join(".config"))?;
    fs::set_permissions(source.join(".config"), fs::Permissions::from_mode(0o700))?;
    fs::write(source.join(".config/settings"), b"safe\n")?;
    let mut value = request(&env)?;
    *value.get_mut("operation").ok_or("operation")? =
        json!({"kind":"import_preview","source":{"raw":source}});
    let out = setup(&env, &value)?;
    assert!(out.status.success(), "{}", common::text(&out));
    let rows = records(&out)?;
    let review = rows
        .iter()
        .find(|r| r["record"] == "import_preview")
        .and_then(|r| r.get("review"))
        .ok_or("missing review")?
        .clone();
    fs::set_permissions(source.join(".config"), fs::Permissions::from_mode(0o755))?;
    *value.get_mut("operation").ok_or("operation")? =
        json!({"kind":"import","source":{"raw":source},"review":review});
    refused(&setup(&env, &value)?)?;
    assert!(!env.repo().exists());
    assert_only_lock(&env);
    Ok(())
}

#[test]
fn static_import_refuses_replaced_source_directory() -> R {
    let env = TestEnv::new();
    let source = env.root().join("selected");
    fs::create_dir(&source)?;
    fs::write(source.join(".sample"), b"safe\n")?;
    let mut value = request(&env)?;
    *value.get_mut("operation").ok_or("operation")? =
        json!({"kind":"import_preview","source":{"raw":source}});
    let out = setup(&env, &value)?;
    assert!(out.status.success(), "{}", common::text(&out));
    let rows = records(&out)?;
    let review = rows
        .iter()
        .find(|r| r["record"] == "import_preview")
        .and_then(|r| r.get("review"))
        .ok_or("missing review")?
        .clone();
    fs::rename(&source, env.root().join("old-selected"))?;
    fs::create_dir(&source)?;
    fs::write(source.join(".sample"), b"safe\n")?;
    *value.get_mut("operation").ok_or("operation")? =
        json!({"kind":"import","source":{"raw":source},"review":review});
    refused(&setup(&env, &value)?)?;
    assert!(!env.repo().exists());
    assert_only_lock(&env);
    Ok(())
}

#[test]
fn static_import_binds_sha256_review_and_preserves_git_format() -> R {
    let env = TestEnv::new();
    let source = env.root().join("selected");
    fs::create_dir(&source)?;
    fs::write(source.join(".sample"), b"first\n")?;
    fs::set_permissions(source.join(".sample"), fs::Permissions::from_mode(0o644))?;
    let mut value = request(&env)?;
    *value.get_mut("operation").ok_or("operation")? =
        json!({"kind":"import_preview","source":{"raw":source}});
    let preview = setup(&env, &value)?;
    assert!(preview.status.success(), "{}", common::text(&preview));
    let rows = records(&preview)?;
    let review = rows
        .iter()
        .find(|r| r["record"] == "import_preview")
        .and_then(|r| r.get("review"))
        .ok_or("missing review")?
        .clone();
    let digest = review
        .get("manifest_sha256")
        .and_then(Value::as_str)
        .ok_or("missing digest")?;
    assert_eq!(
        digest,
        "44c99adae7bb3b82ceb92e51c50d6eed2f91a30135207461f742279a4634a815"
    );
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    let mut wrong = review.clone();
    *wrong.get_mut("manifest_sha256").ok_or("missing digest")? = json!("0".repeat(64));
    *value.get_mut("operation").ok_or("operation")? =
        json!({"kind":"import","source":{"raw":source},"review":wrong});
    refused(&setup(&env, &value)?)?;
    assert!(!env.repo().exists());
    *value.get_mut("operation").ok_or("operation")? =
        json!({"kind":"import","source":{"raw":source},"review":review});
    let out = setup(&env, &value)?;
    assert!(out.status.success(), "{}", common::text(&out));
    let rows = records(&out)?;
    let receipt = rows
        .iter()
        .find(|r| r["record"] == "setup")
        .ok_or("missing receipt")?;
    assert_eq!(receipt.get("review"), Some(&review));
    assert_eq!(
        common::git_in(&env, &env.repo(), &["rev-parse", "--show-object-format"]).trim(),
        "sha1"
    );
    assert_no_setup_stage(&env)?;
    Ok(())
}

#[test]
fn static_import_requires_sha256_git_support() -> R {
    let mut env = TestEnv::new();
    let source = env.root().join("selected");
    fs::create_dir(&source)?;
    fs::write(source.join(".sample"), b"safe\n")?;
    let real_git = std::env::split_paths(&std::env::var_os("PATH").ok_or("missing PATH")?)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .ok_or("missing git")?;
    let bin = env.root().join("git-wrapper");
    fs::create_dir(&bin)?;
    common::write_exe(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\nfor arg do\n  if [ \"$arg\" = '--object-format=sha256' ]; then exit 1; fi\ndone\nexec {} \"$@\"\n",
            shell_words::quote(real_git.to_str().ok_or("git path")?)
        ),
    )?;
    env.prepend_path(&bin);
    let mut value = request(&env)?;
    *value.get_mut("operation").ok_or("operation")? =
        json!({"kind":"import_preview","source":{"raw":source}});
    let out = setup(&env, &value)?;
    refused(&out)?;
    assert!(common::text(&out).contains("requires Git with SHA-256 repository support"));
    assert!(!env.repo().exists());
    assert_no_setup_stage(&env)?;
    Ok(())
}

#[test]
fn static_import_digest_changes_with_content() -> R {
    let env = TestEnv::new();
    let source = env.root().join("selected");
    fs::create_dir(&source)?;
    fs::write(source.join(".sample"), b"first\n")?;
    let mut value = request(&env)?;
    *value.get_mut("operation").ok_or("operation")? =
        json!({"kind":"import_preview","source":{"raw":source}});
    let first = setup(&env, &value)?;
    assert!(first.status.success(), "{}", common::text(&first));
    let first_rows = records(&first)?;
    let first_digest = first_rows
        .iter()
        .find(|r| r["record"] == "import_preview")
        .and_then(|r| r.pointer("/review/manifest_sha256"))
        .and_then(Value::as_str)
        .ok_or("missing digest")?;
    fs::write(source.join(".sample"), b"other\n")?;
    let second = setup(&env, &value)?;
    assert!(second.status.success(), "{}", common::text(&second));
    let second_rows = records(&second)?;
    let second_digest = second_rows
        .iter()
        .find(|r| r["record"] == "import_preview")
        .and_then(|r| r.pointer("/review/manifest_sha256"))
        .and_then(Value::as_str)
        .ok_or("missing digest")?;
    assert_ne!(first_digest, second_digest);
    assert!(!env.repo().exists());
    assert_no_setup_stage(&env)?;
    Ok(())
}

#[test]
fn static_import_refuses_sensitive_selected_directory_without_echo() -> R {
    let env = TestEnv::new();
    let source = env.root().join(".env");
    fs::create_dir(&source)?;
    fs::write(source.join("sample"), b"safe\n")?;
    let mut value = request(&env)?;
    *value.get_mut("operation").ok_or("operation")? =
        json!({"kind":"import_preview","source":{"raw":source}});
    let out = setup(&env, &value)?;
    refused(&out)?;
    assert!(!common::text(&out).contains(&source.to_string_lossy().to_string()));
    assert!(!env.repo().exists());
    assert_only_lock(&env);
    Ok(())
}

#[test]
fn setup_invalid_request_does_not_echo_input() -> R {
    let env = TestEnv::new();
    let sensitive = format!("{}{}", concat!("gh", "p_"), "A".repeat(36));
    let mut value = request(&env)?;
    *value.get_mut("operation").ok_or("operation")? = json!({"kind": sensitive});
    let out = setup(&env, &value)?;
    refused(&out)?;
    assert!(!common::text(&out).contains(&sensitive));
    Ok(())
}

#[test]
fn static_import_refuses_colliding_file_and_directory_names() -> R {
    let env = TestEnv::new();
    let source = env.root().join("selected");
    fs::create_dir_all(source.join("folder"))?;
    fs::write(source.join("Folder"), b"one\n")?;
    fs::write(source.join("folder/item"), b"two\n")?;
    let mut value = request(&env)?;
    *value.get_mut("operation").ok_or("operation")? =
        json!({"kind":"import_preview","source":{"raw":source}});
    refused(&setup(&env, &value)?)?;
    assert!(!env.repo().exists());
    assert_only_lock(&env);
    Ok(())
}

#[test]
fn static_import_refuses_unsupported_and_secret_entries() -> R {
    for case in [
        "symlink",
        "template",
        "script",
        "secret",
        "binary",
        "variant",
        "gitconfig",
        "env",
        "transform",
        "temporary",
        "bare",
    ] {
        let env = TestEnv::new();
        let source = env.root().join("selected");
        fs::create_dir(&source)?;
        match case {
            "symlink" => symlink("missing", source.join(".link"))?,
            "template" => fs::write(source.join("dot_config"), b"x")?,
            "script" => fs::write(source.join(".script"), b"#!/bin/sh\n")?,
            "secret" => fs::write(
                source.join(".sample"),
                format!(
                    "{}\n{}\n",
                    concat!("-----BEGIN ", "PRIVATE KEY-----"),
                    "A".repeat(64)
                ),
            )?,
            "binary" => fs::write(source.join(".sample"), b"text\0binary")?,
            "variant" => fs::write(source.join(".sample{tmpl}"), b"x")?,
            "gitconfig" => fs::write(source.join(".gitconfig"), b"x")?,
            "env" => fs::write(source.join(".env"), b"settings")?,
            "transform" => fs::write(source.join("symlink_config"), b"settings")?,
            "temporary" => fs::write(source.join(".dotman-tmp-test"), b"settings")?,
            "bare" => {
                fs::write(source.join("HEAD"), b"test")?;
                fs::create_dir(source.join("objects"))?;
                fs::create_dir(source.join("refs"))?;
            }
            _ => return Err("unknown case".into()),
        }
        let home = tree_checksum(&env.home());
        let mut value = request(&env)?;
        *value.get_mut("operation").ok_or("operation")? =
            json!({"kind":"import_preview","source":{"raw":source}});
        refused(&setup(&env, &value)?)?;
        assert!(!env.repo().exists());
        assert_eq!(tree_checksum(&env.home()), home);
        assert_only_lock(&env);
    }
    Ok(())
}

#[test]
fn clone_publishes_without_applying_or_running_hooks() -> R {
    let version = std::process::Command::new("git")
        .arg("--version")
        .output()?;
    let text = String::from_utf8(version.stdout)?;
    let token = text
        .trim()
        .strip_prefix("git version ")
        .ok_or("git version")?
        .split_whitespace()
        .next()
        .ok_or("git version token")?;
    let parts: Vec<_> = token.split('.').map(str::parse::<u32>).collect();
    let [Ok(major), Ok(minor), Ok(patch)] = parts.as_slice() else {
        eprintln!("file clone success requires a numeric Git version");
        return Ok(());
    };
    if (*major, *minor, *patch) < (2, 45, 1) {
        eprintln!("file clone success requires Git 2.45.1");
        return Ok(());
    }
    for present in [false, true] {
        let env = TestEnv::new();
        let seed = env.root().join("seed");
        fs::create_dir(&seed)?;
        common::git_in(&env, &seed, &["init", "-q"]);
        fs::write(seed.join(".managed"), b"repo data\n")?;
        fs::write(
            seed.join(".dotman.toml"),
            b"[hooks]\n\"**\" = \": > hook-canary\"\n",
        )?;
        common::git_in(&env, &seed, &["add", "."]);
        common::git_in(&env, &seed, &["commit", "-qm", "seed"]);
        if present {
            env.write(".managed", b"home data\n", 0o600);
        }
        let home = tree_checksum(&env.home());
        let config = tree_checksum(&env.xdg_config());
        let mut value = request(&env)?;
        let source = format!("file://{}", seed.display());
        *value.get_mut("operation").ok_or("missing operation")? =
            json!({"kind":"clone", "source": source});
        let system = env.root().join("hostile-gitconfig");
        fs::write(
            &system,
            "[url \"file:///nonexistent/\"]\n insteadOf = file://\n",
        )?;
        let out = env.run_stdin(
            &[
                ("GIT_CONFIG_SYSTEM", system.to_str().ok_or("system path")?),
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "url.file:///nonexistent/.insteadOf"),
                ("GIT_CONFIG_VALUE_0", "file://"),
            ],
            &serde_json::to_vec(&value)?,
            &["init", "--setup-from", "-", "--json"],
        );
        assert!(out.status.success(), "{}", common::text(&out));
        let receipt = records(&out)?;
        let [context, setup, result] = receipt.as_slice() else {
            return Err("expected context, setup, result".into());
        };
        assert_eq!(context.get("command"), Some(&json!("init")));
        assert_eq!(context.get("repo_id"), Some(&Value::Null));
        assert_eq!(setup.get("operation"), Some(&json!("clone")));
        let repo_id = setup.get("repo_id").ok_or("missing id")?;
        let id = repo_id
            .get("raw")
            .and_then(Value::as_str)
            .ok_or("missing id")?;
        assert_eq!(repo_id.get("shown").and_then(Value::as_str), Some(id));
        assert_eq!(
            fs::read_to_string(env.repo().join(".git/dotman-id"))?.trim(),
            id
        );
        assert_eq!(result.get("exit_code"), Some(&json!(0)));
        assert_eq!(fs::read(env.repo().join(".managed"))?, b"repo data\n");
        assert_eq!(
            common::git_in(&env, &env.repo(), &["remote", "get-url", "origin"]).trim(),
            source
        );
        assert_eq!(tree_checksum(&env.home()), home);
        assert_eq!(tree_checksum(&env.xdg_config()), config);
        assert_only_lock(&env);
        assert!(!env.repo().join("hook-canary").exists());
        assert!(!env.home().join("hook-canary").exists());
        let status = env.run(&["status", "--json"]);
        assert!(status.status.success(), "{}", common::text(&status));
        let status_records = records(&status)?;
        let status_id = status_records
            .first()
            .and_then(|r| r.get("repo_id"))
            .and_then(|r| r.get("raw"))
            .and_then(Value::as_str);
        assert_eq!(status_id, Some(id));
    }
    Ok(())
}

#[test]
fn old_and_prerelease_git_refuse_file_clone_before_clone_command() -> R {
    for version in ["2.25.0", "2.45.0", "2.45.1-rc1", "2.46.0-rc0"] {
        let mut env = TestEnv::new();
        let bin = env.root().join("old-git-bin");
        fs::create_dir(&bin)?;
        let canary = env.root().join("clone-invoked");
        common::write_exe(
            &bin.join("git"),
            &format!(
                "#!/bin/sh\nfor arg do\n  if [ \"$arg\" = --version ]; then echo 'git version {version}'; exit 0; fi\ndone\n: > '{}'\nexit 97\n",
                canary.display()
            ),
        )?;
        env.prepend_path(&bin);
        let mut value = request(&env)?;
        *value.get_mut("operation").ok_or("missing operation")? =
            json!({"kind":"clone", "source":"file:///synthetic/repo"});
        refused(&setup(&env, &value)?)?;
        assert!(!canary.exists(), "{version}");
        assert!(!env.repo().exists(), "{version}");
        assert_only_lock(&env);
    }
    Ok(())
}

#[test]
fn clone_refuses_stale_and_occupied_destination() -> R {
    for stale in [false, true] {
        let env = TestEnv::new();
        let mut value = request(&env)?;
        *value.get_mut("operation").ok_or("missing operation")? =
            json!({"kind":"clone", "source":"file:///synthetic/repo"});
        if stale {
            *value.get_mut("parent_ino").ok_or("missing parent")? = json!(0);
        } else {
            fs::create_dir(env.repo())?;
            fs::write(env.repo().join("sentinel"), b"untouched")?;
        }
        let before = tree_checksum(&env.xdg_data());
        refused(&setup(&env, &value)?)?;
        assert_eq!(tree_checksum(&env.xdg_data()), before);
        assert!(tree_checksum(&env.xdg_state()).is_empty());
    }
    Ok(())
}

#[test]
fn invalid_clone_sources_never_invoke_git() -> R {
    let mut env = TestEnv::new();
    let bin = env.root().join("invalid-clone-bin");
    fs::create_dir(&bin)?;
    let canary = env.root().join("invalid-clone-git");
    common::write_exe(
        &bin.join("git"),
        &format!("#!/bin/sh\n: > '{}'\nexit 97\n", canary.display()),
    )?;
    env.prepend_path(&bin);
    for source in [
        "https://user@example.com/repo",
        "https://example.com/a//b",
        "https://example.com/../repo",
        "https://example.com/a%2fb",
        "https://127.1/repo",
        "https://0x7f000001/repo",
        "https://example.com:0/repo",
        "https://example.com/repo?q=x",
        "file://localhost/repo",
        "file:///a/../b",
        "ssh://example.com/repo",
        "https://example.com\\evil/repo",
    ] {
        let mut value = request(&env)?;
        *value.get_mut("operation").ok_or("missing operation")? =
            json!({"kind":"clone", "source": source});
        refused(&setup(&env, &value)?)?;
        assert!(!env.repo().exists(), "{source}");
        assert!(tree_checksum(&env.xdg_state()).is_empty(), "{source}");
    }
    let mut value = request(&env)?;
    *value.get_mut("operation").ok_or("missing operation")? =
        json!({"kind":"clone", "source":format!("https://example.invalid/{}", "a".repeat(8192))});
    refused(&setup(&env, &value)?)?;
    assert!(tree_checksum(&env.xdg_state()).is_empty());
    assert!(!canary.exists());
    Ok(())
}

#[test]
fn url_shaped_local_directory_is_not_used_as_clone_source() -> R {
    let mut env = TestEnv::new();
    let shaped = env.root().join("https:/example.invalid/repo");
    fs::create_dir_all(&shaped)?;
    common::git_in(&env, &shaped, &["init", "-q"]);
    let bin = env.root().join("url-git-bin");
    fs::create_dir(&bin)?;
    let log = env.root().join("git-calls");
    common::write_exe(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\nfor arg do\n  if [ \"$arg\" = --version ]; then echo 'git version 2.56.1'; exit 0; fi\ndone\nprintf '%s\\n' \"$*\" >> '{}'\nexit 97\n",
            log.display()
        ),
    )?;
    env.prepend_path(&bin);
    let mut value = request(&env)?;
    *value.get_mut("operation").ok_or("missing operation")? =
        json!({"kind":"clone", "source":"https://example.invalid/repo"});
    let out = env
        .dot_process()
        .current_dir(env.root())
        .args(["init", "--setup-from", "-", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child = out;
    child
        .stdin
        .take()
        .ok_or("missing stdin")?
        .write_all(&serde_json::to_vec(&value)?)?;
    refused(&child.wait_with_output()?)?;
    let calls = fs::read_to_string(&log)?;
    assert_eq!(calls.lines().count(), 1);
    assert!(calls.contains("clone"));
    assert!(calls.contains("https://example.invalid/repo"));
    assert!(!env.repo().exists());
    let prefix = "https://example.invalid/";
    let source = format!(
        "{prefix}{}",
        "a".repeat(8192_usize.saturating_sub(prefix.len()))
    );
    *value.get_mut("operation").ok_or("missing operation")? =
        json!({"kind":"clone", "source":source});
    refused(&setup(&env, &value)?)?;
    assert_eq!(fs::read_to_string(log)?.lines().count(), 2);
    Ok(())
}

#[test]
fn fresh_publishes_a_new_repo_without_applying_or_bootstrapping() -> R {
    let env = TestEnv::new();
    env.write(".managed", b"home stays\n", 0o600);
    env.write_machine_config("[guard]\nallow = []\n");
    let home = tree_checksum(&env.home());
    let config = tree_checksum(&env.xdg_config());
    let out = setup(&env, &request(&env)?)?;
    assert!(out.status.success(), "{}", common::text(&out));
    let records = records(&out)?;
    assert_eq!(records.len(), 3);
    let [context, receipt, result] = records.as_slice() else {
        return Err("expected context, setup, result".into());
    };
    assert_eq!(context["record"], "context");
    assert_eq!(context["command"], "init");
    assert_eq!(context["schema_version"], 1);
    assert!(context["repo_id"].is_null());
    assert_eq!(receipt["record"], "setup");
    assert_eq!(receipt["operation"], "fresh");
    let repo_id = receipt.get("repo_id").ok_or("missing repo_id")?;
    let id = repo_id
        .get("raw")
        .and_then(Value::as_str)
        .ok_or("missing raw ID")?;
    assert_eq!(id.len(), 32);
    assert!(id.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(repo_id.get("shown").and_then(Value::as_str), Some(id));
    assert_eq!(
        fs::read_to_string(env.repo().join(".git/dotman-id"))?.trim(),
        id
    );
    assert_eq!(result["record"], "result");
    assert_eq!(result["exit_code"], 0);
    assert!(env.repo().join(".git").is_dir());
    assert!(env.repo().join(".dotman.toml").is_file());
    assert!(!env.repo().join("install.sh").exists());
    assert_eq!(tree_checksum(&env.home()), home);
    assert_eq!(tree_checksum(&env.xdg_config()), config);
    assert_only_lock(&env);
    let names: Vec<_> = fs::read_dir(env.xdg_data())?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<Result<_, _>>()?;
    assert_eq!(names, vec![env.repo().file_name().ok_or("repo name")?]);
    Ok(())
}

#[test]
fn byte_encoded_paths_are_accepted() -> R {
    let env = TestEnv::new();
    let mut value = request(&env)?;
    *value.get_mut("home").ok_or("missing home")? =
        json!({"bytes": env.home().as_os_str().as_bytes()});
    *value.get_mut("repo").ok_or("missing repo")? =
        json!({"bytes": env.repo().as_os_str().as_bytes()});
    let out = setup(&env, &value)?;
    assert!(out.status.success(), "{}", common::text(&out));
    assert!(records(&out)?.iter().any(|v| v["record"] == "setup"));
    assert!(env.repo().join(".git/dotman-id").is_file());
    Ok(())
}

#[test]
fn existing_destinations_are_not_adopted_or_modified() -> R {
    for kind in ["empty", "git", "file", "symlink"] {
        let env = TestEnv::new();
        let repo = env.repo();
        let target = env.root().join("link-target");
        match kind {
            "empty" => fs::create_dir(&repo)?,
            "git" => {
                fs::create_dir(&repo)?;
                common::git_in(&env, &repo, &["init", "-q"]);
            }
            "file" => fs::write(&repo, b"untouched")?,
            "symlink" => {
                fs::create_dir(&target)?;
                fs::write(target.join("sentinel"), b"untouched")?;
                symlink(&target, &repo)?;
            }
            _ => unreachable!(),
        }
        let before = tree_checksum(&env.xdg_data());
        let target_before = target.is_dir().then(|| tree_checksum(&target));
        refused(&setup(&env, &request(&env)?)?)?;
        assert_eq!(tree_checksum(&env.xdg_data()), before, "{kind}");
        assert_eq!(
            target.is_dir().then(|| tree_checksum(&target)),
            target_before
        );
        assert!(tree_checksum(&env.xdg_state()).is_empty());
    }
    Ok(())
}

#[test]
fn strict_input_and_conflicting_arguments_fail_before_git() -> R {
    let mut env = TestEnv::new();
    let valid = request(&env)?;
    let bin = env.root().join("invalid-input-bin");
    fs::create_dir(&bin)?;
    let canary = env.root().join("invalid-input-invoked-git");
    common::write_exe(
        &bin.join("git"),
        &format!("#!/bin/sh\n: > '{}'\nexit 97\n", canary.display()),
    )?;
    env.prepend_path(&bin);
    let mut inputs = vec![b"{}".to_vec(), b"{".to_vec()];
    let mut extra = valid.clone();
    extra
        .as_object_mut()
        .ok_or("request is not an object")?
        .insert("unexpected".into(), json!(true));
    inputs.push(serde_json::to_vec(&extra)?);
    let mut version = valid.clone();
    *version.get_mut("schema_version").ok_or("missing version")? = json!(2);
    inputs.push(serde_json::to_vec(&version)?);
    let mut clone = valid.clone();
    *clone.get_mut("operation").ok_or("missing operation")? =
        json!({"kind":"clone", "source":"https://example.invalid/../repo"});
    inputs.push(serde_json::to_vec(&clone)?);
    let mut bad_path = valid.clone();
    *bad_path.get_mut("repo").ok_or("missing repo")? = json!({"raw":"relative"});
    inputs.push(serde_json::to_vec(&bad_path)?);
    let mut both = valid.clone();
    *both.get_mut("home").ok_or("missing home")? =
        json!({"raw":env.home(), "bytes":env.home().as_os_str().as_bytes()});
    inputs.push(serde_json::to_vec(&both)?);
    let mut trailing = serde_json::to_vec(&valid)?;
    trailing.extend_from_slice(b" {}");
    inputs.push(trailing);
    for input in inputs {
        let out = run(&env, &input, &["init", "--setup-from", "-", "--json"]);
        assert_eq!(out.status.code(), Some(1));
        refused(&out)?;
        assert!(!env.repo().exists());
        assert!(tree_checksum(&env.xdg_state()).is_empty());
    }
    for extra in ["--dry-run", "--no-import", "--wizard"] {
        let out = run(
            &env,
            &serde_json::to_vec(&valid)?,
            &["init", "--setup-from", "-", "--json", extra],
        );
        assert_eq!(out.status.code(), Some(2), "{extra}");
        assert!(out.stdout.is_empty(), "{extra}");
        assert!(!env.repo().exists());
    }
    let out = run(
        &env,
        &serde_json::to_vec(&valid)?,
        &["init", "--setup-from", "-"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(
        out.stdout.is_empty(),
        "invalid non-JSON CLI syntax emitted output"
    );
    assert!(!canary.exists(), "malformed input started Git");
    Ok(())
}

#[test]
fn stale_parent_and_different_resolved_context_are_refused() -> R {
    for field in ["parent_dev", "parent_ino", "home", "repo"] {
        let env = TestEnv::new();
        let mut value = request(&env)?;
        let entry = value.get_mut(field).ok_or("missing request field")?;
        *entry = match field {
            "parent_dev" | "parent_ino" => {
                let n = entry.as_u64().ok_or("missing stat")?;
                json!(n.wrapping_add(1))
            }
            _ => json!({"raw":env.root().join("different")}),
        };
        refused(&setup(&env, &value)?)?;
        assert!(!env.repo().exists());
        assert_only_lock(&env);
    }
    Ok(())
}

#[test]
fn missing_protected_config_below_destination_is_rejected() -> R {
    let env = TestEnv::new();
    let repo = env.repo();
    let config = repo.join("nested/config.toml");
    let out = env.run_stdin(
        &[("DOTMAN_CONFIG_PATH", config.to_str().ok_or("config path")?)],
        &serde_json::to_vec(&request(&env)?)?,
        &["init", "--setup-from", "-", "--json"],
    );
    refused(&out)?;
    assert!(!repo.exists());
    assert_only_lock(&env);
    Ok(())
}

#[test]
fn linked_locks_are_refused_without_touching_the_target() -> R {
    for hard in [false, true] {
        let env = TestEnv::new();
        fs::create_dir(env.state_dir())?;
        fs::set_permissions(env.state_dir(), fs::Permissions::from_mode(0o700))?;
        let target = env.root().join("lock-target");
        fs::write(&target, b"sentinel")?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644))?;
        let before = fs::metadata(&target)?;
        if hard {
            fs::hard_link(&target, env.state_dir().join("lock"))?;
        } else {
            symlink(&target, env.state_dir().join("lock"))?;
        }
        let state = tree_checksum(&env.xdg_state());
        refused(&setup(&env, &request(&env)?)?)?;
        let after = fs::metadata(&target)?;
        assert_eq!(after.mode(), before.mode());
        assert_eq!(after.mtime(), before.mtime());
        assert_eq!(after.mtime_nsec(), before.mtime_nsec());
        assert_eq!(fs::read(&target)?, b"sentinel");
        assert_eq!(tree_checksum(&env.xdg_state()), state);
        assert!(!env.repo().exists());
    }
    Ok(())
}

#[test]
fn group_writable_parent_is_refused() -> R {
    let env = TestEnv::new();
    fs::set_permissions(env.xdg_data(), fs::Permissions::from_mode(0o770))?;
    let before = tree_checksum(&env.xdg_data());
    refused(&setup(&env, &request(&env)?)?)?;
    assert_eq!(tree_checksum(&env.xdg_data()), before);
    assert_only_lock(&env);
    Ok(())
}

#[derive(Clone, Copy)]
enum Race {
    Parent,
    Destination,
    Stage,
}

fn race(scenario: Race) -> R {
    let mut env = TestEnv::new();
    let value = request(&env)?;
    let real_git = std::env::split_paths(&std::env::var_os("PATH").ok_or("missing PATH")?)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .ok_or("missing git")?;
    let bin = env.root().join("gate-bin");
    fs::create_dir(&bin)?;
    let ready = env.root().join("git-ready");
    let release = env.root().join("git-release");
    let wrapper = format!(
        "#!/bin/sh\nfor arg do\n  if [ \"$arg\" = init ]; then\n    : > '{}'\n    n=0\n    while [ ! -e '{}' ]; do\n      n=$((n + 1))\n      [ \"$n\" -lt 200 ] || exit 98\n      sleep 0.05\n    done\n    break\n  fi\ndone\nexec '{}' \"$@\"\n",
        ready.display(),
        release.display(),
        real_git.display()
    );
    common::write_exe(&bin.join("git"), &wrapper)?;
    env.prepend_path(&bin);
    let home = tree_checksum(&env.home());
    let config = tree_checksum(&env.xdg_config());
    let mut child = env
        .dot_process()
        .args(["init", "--setup-from", "-", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("missing stdin")?
        .write_all(&serde_json::to_vec(&value)?)?;
    let start = Instant::now();
    while !ready.exists() && start.elapsed() < Duration::from_secs(8) {
        if child.try_wait()?.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if !ready.exists() {
        child.kill().ok();
        let out = child.wait_with_output()?;
        return Err(format!("git gate not reached: {}", common::text(&out)).into());
    }
    let original = env.root().join("original-data");
    let stage = if matches!(scenario, Race::Stage) {
        let entries: Vec<_> = fs::read_dir(env.xdg_data())?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<_, _>>()?;
        let [stage] = entries.as_slice() else {
            fs::write(&release, b"continue")?;
            let _ = child.wait_with_output()?;
            return Err("expected one unique stage at git init".into());
        };
        let stage = stage.clone();
        fs::rename(&stage, &original)?;
        fs::create_dir(&stage)?;
        fs::write(stage.join("sentinel"), b"replacement")?;
        Some(stage)
    } else {
        if matches!(scenario, Race::Parent) {
            fs::rename(env.xdg_data(), &original)?;
            fs::create_dir(env.xdg_data())?;
            fs::create_dir(env.repo())?;
            fs::write(env.repo().join("sentinel"), b"replacement")?;
        } else {
            fs::create_dir(env.repo())?;
        }
        None
    };
    let canary = stage.unwrap_or_else(|| env.repo());
    let replacement = tree_checksum(&canary);
    let original_inode = fs::metadata(&canary)?.ino();
    fs::write(&release, b"continue")?;
    let out = child.wait_with_output()?;
    refused(&out)?;
    assert_eq!(tree_checksum(&canary), replacement);
    assert_eq!(fs::metadata(&canary)?.ino(), original_inode);
    match scenario {
        Race::Parent => assert!(
            tree_checksum(&original).is_empty(),
            "staging was not cleaned through the original parent"
        ),
        Race::Destination => assert_eq!(
            fs::read_dir(env.xdg_data())?.count(),
            1,
            "stage was not cleaned"
        ),
        Race::Stage => {
            assert!(original.is_dir(), "renamed original stage was removed");
            assert!(!env.repo().exists(), "repo was published");
        }
    }
    assert_eq!(tree_checksum(&env.home()), home);
    assert_eq!(tree_checksum(&env.xdg_config()), config);
    assert_only_lock(&env);
    Ok(())
}

#[test]
fn parent_replacement_during_git_staging_is_not_published() -> R {
    race(Race::Parent)
}

#[test]
fn destination_created_during_git_staging_is_not_adopted() -> R {
    race(Race::Destination)
}

#[test]
fn replaced_stage_name_is_not_removed() -> R {
    race(Race::Stage)
}
