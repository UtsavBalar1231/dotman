mod common;

use common::{TestEnv, write_exe};
use serde_json::Value;
use std::error::Error;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;

type R<T = ()> = Result<T, Box<dyn Error>>;

fn at<'a>(value: &'a Value, path: &str) -> &'a Value {
    value.pointer(path).unwrap_or(&Value::Null)
}

fn records(env: &TestEnv, vars: &[(&str, &str)], args: &[&str]) -> R<Vec<Value>> {
    let out = env.run_with(vars, args);
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned().into());
    }
    let values: Vec<Value> = String::from_utf8(out.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        values.first().and_then(|v| v["record"].as_str()),
        Some("context")
    );
    assert_eq!(
        values.first().map(|v| &v["schema_version"]),
        Some(&Value::from(1))
    );
    assert_eq!(
        values.first().map(|v| &v["experimental"]),
        Some(&Value::from(true))
    );
    assert_eq!(
        values.last().and_then(|v| v["record"].as_str()),
        Some("result")
    );
    assert_eq!(values.iter().filter(|v| v["record"] == "result").count(), 1);
    if args.first() == Some(&"diff") {
        assert_eq!(values.iter().filter(|v| v["record"] == "report").count(), 1);
    }
    Ok(values)
}

fn entry<'a>(values: &'a [Value], key: &str) -> R<&'a Value> {
    values
        .iter()
        .find(|v| at(v, "/key/raw") == key)
        .ok_or_else(|| format!("missing {key}").into())
}

#[test]
fn snapshot_tuples_match_status_and_payload_and_leave_trees_unchanged() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".private", b"original\n");
    env.write(".private", b"original\n", 0o600);
    env.ok(&["apply", "--take-repo"]);
    env.write(".private", b"changed\n", 0o4600);
    env.write(".target", b"target\n", 0o644);
    env.symlink(".link", env.home().join(".target"));
    env.ok(&["add", ".target", ".link"]);
    let before = env.four_trees();
    let diff = records(&env, &[], &["diff", "--json"])?;
    let status = records(&env, &[], &["status", "--json"])?;
    assert_eq!(before, env.four_trees());
    for key in [".private", ".target", ".link"] {
        let d = entry(&diff, key)?;
        let s = entry(&status, key)?;
        for side in ["r", "source_tuple", "h", "raw_h"] {
            assert_eq!(d[side], s[side], "{key} {side}");
        }
    }
    let private = entry(&diff, ".private")?;
    assert_eq!(at(private, "/r/tuple/mode"), "0600");
    assert_eq!(at(private, "/source_tuple/tuple/mode"), "0644");
    assert_eq!(at(private, "/h/tuple/mode"), "0600");
    assert_eq!(
        at(private, "/h/tuple/hash").as_str(),
        Some(format!("{:032x}", xxhash_rust::xxh3::xxh3_128(b"changed\n")).as_str())
    );
    let payload = at(private, "/payload/raw")
        .as_str()
        .ok_or("missing payload")?;
    assert_eq!(
        payload,
        "diff --git repo/.private home/.private\n\
         index 4b48dee..5ea2ed4 100644\n\
         --- repo/.private\n\
         +++ home/.private\n\
         @@ -1 +1 @@\n\
         -original\n\
         +changed\n"
    );
    let link = entry(&diff, ".link")?;
    assert_eq!(link["status"], "equal");
    assert_ne!(link["h"], link["raw_h"]);
    assert_eq!(
        at(link, "/raw_h/tuple/hash").as_str(),
        Some(
            format!(
                "{:032x}",
                xxhash_rust::xxh3::xxh3_128(env.home().join(".target").as_os_str().as_bytes())
            )
            .as_str()
        )
    );
    Ok(())
}

#[test]
fn hostile_diff_drivers_textconv_and_pagers_do_not_run() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".data", b"old\n", 0o644);
    env.ok(&["add", ".data"]);
    env.write(".data", b"new\n", 0o644);
    let marker = env.root().join("executed");
    let script = env.root().join("hostile");
    write_exe(
        &script,
        &format!(
            "#!/bin/sh\nprintf invoked >> '{}'\nprintf injected\n",
            marker.display()
        ),
    )?;
    let attrs = env.root().join("attributes");
    fs::write(&attrs, "* diff=hostile\n")?;
    env.write_repo(".gitattributes", b"* diff=hostile\n");
    fs::write(
        env.home().join(".gitconfig"),
        format!(
            "[user]\nname = Test\nemail = test@example.invalid\n[core]\nattributesFile = {}\npager = {}\n[diff]\nexternal = {}\n[diff \"hostile\"]\ntextconv = {}\n",
            attrs.display(),
            script.display(),
            script.display(),
            script.display()
        ),
    )?;
    let hostile = script.to_string_lossy();
    let values = records(
        &env,
        &[
            ("GIT_EXTERNAL_DIFF", &hostile),
            ("GIT_PAGER", &hostile),
            ("NO_PAGER", ""),
        ],
        &["diff", "--json", ".data"],
    )?;
    let payload = at(entry(&values, ".data")?, "/payload/raw")
        .as_str()
        .ok_or("missing diff")?;
    assert!(payload.contains("-old\n+new\n"), "{payload}");
    assert!(!payload.contains("injected"));
    assert!(!payload.contains('\u{1b}'));
    assert!(!marker.exists());
    Ok(())
}

#[test]
fn missing_and_blocked_paths_are_reported() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".gone", b"old\n", 0o644);
    env.write("dir/file", b"old\n", 0o644);
    env.ok(&["add", ".gone", "dir/file"]);
    fs::remove_file(env.repo().join(".gone"))?;
    fs::remove_dir_all(env.home().join("dir"))?;
    env.symlink("dir", env.root());
    let values = records(&env, &[], &["diff", "--json"])?;
    let gone = entry(&values, ".gone")?;
    assert_eq!(at(gone, "/r/state"), "missing");
    assert!(
        at(gone, "/payload/raw")
            .as_str()
            .ok_or("missing payload")?
            .contains("+old\n")
    );
    assert_eq!(entry(&values, "dir/file")?["status"], "blocked");
    fs::remove_file(env.home().join(".gone"))?;
    fs::set_permissions(
        env.repo().join("dir/file"),
        fs::Permissions::from_mode(0o644),
    )?;
    Ok(())
}

#[test]
fn deleted_tracked_sources_without_state_are_selected() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".tracked", b"repo\n");
    env.git(&["-C", &env.repo().to_string_lossy(), "add", ".tracked"]);
    fs::remove_file(env.repo().join(".tracked"))?;
    env.write(".tracked", b"home\n", 0o644);
    let values = records(&env, &[], &["diff", "--json", ".tracked"])?;
    let item = entry(&values, ".tracked")?;
    assert_eq!(at(item, "/r/state"), "missing");
    assert!(
        at(item, "/payload/raw")
            .as_str()
            .ok_or("missing payload")?
            .contains("+home\n")
    );
    Ok(())
}

#[test]
fn template_errors_and_ambiguous_sources_do_not_invent_tuples() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".bad{tmpl}", b"{{ vars.undefined }}\n");
    env.write_repo(".ambiguous", b"one\n");
    env.write_repo(".ambiguous{tmpl}", b"two\n");
    let values = records(&env, &[], &["diff", "--json"])?;
    let bad = entry(&values, ".bad")?;
    assert_eq!(bad["status"], "template_error");
    assert_eq!(at(bad, "/r/reason"), "template_error");
    let ambiguous = entry(&values, ".ambiguous")?;
    assert_eq!(ambiguous["status"], "ambiguous");
    assert_eq!(at(ambiguous, "/r/state"), "unavailable");
    Ok(())
}

#[test]
fn template_tuple_and_payload_describe_rendered_content() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_machine_config("[vars]\nfont = \"14\"\n");
    env.write_repo(".font{tmpl}", b"font {{ vars.font }}\n");
    env.write(".font", b"font 18\n", 0o644);
    let values = records(&env, &[], &["diff", "--json", ".font"])?;
    let item = entry(&values, ".font")?;
    assert_eq!(
        at(item, "/r/tuple/hash").as_str(),
        Some(format!("{:032x}", xxhash_rust::xxh3::xxh3_128(b"font 14\n")).as_str())
    );
    assert!(
        at(item, "/payload/raw")
            .as_str()
            .ok_or("missing payload")?
            .contains("-font 14\n+font 18\n")
    );
    Ok(())
}

#[test]
fn explicit_modes_missing_home_and_kind_changes_match_status() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".dotman.toml", b"[modes]\n\".mode\" = \"0640\"\n");
    env.write_repo(".mode", b"repo\n");
    env.write(".mode", b"home\n", 0o600);
    env.write_repo(".missing", b"missing\n");
    env.write_repo(".kind", b"file\n");
    env.symlink(".kind", "target");
    let diff = records(&env, &[], &["diff", "--json"])?;
    let status = records(&env, &[], &["status", "--json"])?;
    for key in [".mode", ".missing", ".kind"] {
        let d = entry(&diff, key)?;
        let s = entry(&status, key)?;
        for side in ["r", "source_tuple", "h", "raw_h"] {
            assert_eq!(d[side], s[side], "{key} {side}");
        }
    }
    assert_eq!(at(entry(&diff, ".mode")?, "/r/tuple/mode"), "0640");
    assert_eq!(at(entry(&diff, ".missing")?, "/h/state"), "missing");
    let kind = entry(&diff, ".kind")?;
    assert_eq!(at(kind, "/r/tuple/kind"), "file");
    assert_eq!(at(kind, "/h/tuple/kind"), "symlink");
    assert_eq!(kind["status"], "different");
    Ok(())
}

#[test]
fn repo_cannot_be_used_as_the_private_snapshot_base() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".file", b"repo\n");
    env.write(".file", b"home\n", 0o644);
    let repo = env.repo();
    let out = env.run_with(
        &[("XDG_RUNTIME_DIR", repo.to_str().ok_or("non-UTF-8 repo")?)],
        &["diff", "--json", ".file"],
    );
    assert_eq!(out.status.code(), Some(1));
    let values: Vec<Value> = String::from_utf8(out.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        values.last().map(|v| &v["record"]),
        Some(&Value::from("result"))
    );
    assert!(!fs::read_dir(repo)?.any(|item| item.is_ok_and(|item| {
        item.file_name()
            .to_string_lossy()
            .starts_with(".dotman-tmp-")
    })));
    Ok(())
}

#[test]
fn invalid_selection_returns_one_error_result() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let out = env.run(&["diff", "--json", "missing"]);
    assert_eq!(out.status.code(), Some(1));
    let values: Vec<Value> = String::from_utf8(out.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        values
            .first()
            .and_then(|v| v.get("record"))
            .and_then(Value::as_str),
        Some("context")
    );
    assert_eq!(values.iter().filter(|v| v["record"] == "result").count(), 1);
    assert_eq!(
        values
            .last()
            .and_then(|v| v.get("exit_code"))
            .and_then(Value::as_i64),
        Some(1)
    );
    Ok(())
}

#[test]
fn portable_link_target_is_the_target_in_the_payload() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write("old-target", b"x", 0o644);
    env.write("new-target", b"x", 0o644);
    env.symlink(".link", "old-target");
    env.ok(&["add", ".link"]);
    fs::remove_file(env.home().join(".link"))?;
    env.symlink(".link", env.home().join("new-target"));
    let values = records(&env, &[], &["diff", "--json", ".link"])?;
    let link = entry(&values, ".link")?;
    let payload = at(link, "/payload/raw").as_str().ok_or("missing payload")?;
    assert!(payload.contains("-old-target"), "{payload}");
    assert!(payload.contains("+new-target"), "{payload}");
    assert_eq!(
        at(link, "/h/tuple/hash").as_str(),
        Some(format!("{:032x}", xxhash_rust::xxh3::xxh3_128(b"new-target")).as_str())
    );
    assert_ne!(link["h"], link["raw_h"]);
    Ok(())
}

#[test]
fn system_diff_is_read_only_and_reports_disabled_scope() -> R {
    let env = common::system_setup(&["/etc"]);
    let key = "/etc/dotman-test.conf";
    env.write_system(key, b"old\n", 0o644);
    env.ok(&["add", "--system", key]);
    env.write_system(key, b"new\n", 0o4755);
    env.write_machine_config("[system]\nroots = []\n");
    let before = env.four_trees();
    let system_before = fs::read(env.system_path(key))?;
    let values = records(&env, &[], &["diff", "--json", key])?;
    let item = entry(&values, key)?;
    let status = records(&env, &[], &["status", "--json", key])?;
    let status_item = entry(&status, key)?;
    for side in ["r", "source_tuple", "h", "raw_h"] {
        assert_eq!(item[side], status_item[side], "{side}");
    }
    assert_eq!(item["system_off"], true);
    assert_eq!(at(item, "/h/tuple/mode"), "0755");
    assert!(
        at(item, "/payload/raw")
            .as_str()
            .ok_or("missing payload")?
            .contains("-old\n+new\n")
    );
    assert_eq!(env.four_trees(), before);
    assert_eq!(fs::read(env.system_path(key))?, system_before);
    Ok(())
}

#[test]
fn deleted_tracked_system_source_without_state_is_selected() -> R {
    let env = common::system_setup(&["/etc"]);
    let key = "/etc/dotman-tracked.conf";
    let source = ".dotman/system/etc/dotman-tracked.conf";
    env.write_repo(source, b"repo\n");
    env.git(&["-C", &env.repo().to_string_lossy(), "add", "-f", source]);
    fs::remove_file(env.repo().join(source))?;
    env.write_system(key, b"home\n", 0o644);
    let values = records(&env, &[], &["diff", "--json", key])?;
    let item = entry(&values, key)?;
    assert_eq!(at(item, "/r/state"), "missing");
    assert!(
        at(item, "/payload/raw")
            .as_str()
            .ok_or("missing payload")?
            .contains("+home\n")
    );
    Ok(())
}

#[test]
fn option_keys_and_non_utf8_paths_and_payload_are_preserved() -> R {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write("--item", b"old \xff\n", 0o644);
    env.ok(&["add", "--", "--item"]);
    env.write("--item", b"new \xfe\n", 0o644);
    let home = env.root().join(OsString::from_vec(b"home-\xff".to_vec()));
    fs::rename(env.home(), &home)?;
    let out = env
        .dot_process()
        .env("HOME", &home)
        .current_dir(&home)
        .args(["diff", "--json", "--", "--item"])
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let values: Vec<Value> = String::from_utf8(out.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let item = entry(&values, "--item")?;
    assert_eq!(
        at(item, "/path/bytes"),
        &serde_json::json!(home.join("--item").as_os_str().as_bytes())
    );
    let payload: Vec<u8> = serde_json::from_value(at(item, "/payload/bytes").clone())?;
    assert!(
        payload
            .windows(b"-old \xff\n".len())
            .any(|w| w == b"-old \xff\n")
    );
    assert!(
        payload
            .windows(b"+new \xfe\n".len())
            .any(|w| w == b"+new \xfe\n")
    );
    assert_eq!(
        values.last().map(|v| &v["record"]),
        Some(&Value::from("result"))
    );
    Ok(())
}

#[test]
fn interrupted_git_diff_removes_private_snapshots() -> R {
    use std::os::unix::process::CommandExt;
    let mut env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".a", b"repo\n");
    env.write(".a", b"home\n", 0o644);
    let real_git = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .ok_or("git missing")?;
    let bin = env.root().join("bin");
    fs::create_dir(&bin)?;
    let marker = env.root().join("interrupt");
    write_exe(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\nfor arg do\n if [ \"$arg\" = --no-index ]; then\n  trap '' TERM\n  : > '{}'\n  kill -TERM 0\n  exit 42\n fi\ndone\nexec '{}' \"$@\"\n",
            marker.display(),
            real_git.display()
        ),
    )?;
    env.prepend_path(&bin);
    let runtime = env.root().join("runtime");
    fs::create_dir(&runtime)?;
    let out = env
        .dot_process()
        .env("XDG_RUNTIME_DIR", &runtime)
        .args(["diff", "--json", ".a"])
        .process_group(0)
        .output()?;
    assert!(marker.exists());
    assert_eq!(out.status.code(), Some(1));
    let values: Vec<Value> = String::from_utf8(out.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        values.last().map(|v| &v["record"]),
        Some(&Value::from("result"))
    );
    assert_eq!(fs::read_dir(runtime)?.count(), 0);
    Ok(())
}
