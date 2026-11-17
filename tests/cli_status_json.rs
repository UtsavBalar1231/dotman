mod common;

use common::TestEnv;
use dotman::output::escape_line;
use dotman::state::State;
use serde_json::{Value, json};
use std::error::Error;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::time::Instant;

type R<T = ()> = Result<T, Box<dyn Error>>;

fn at<'a>(value: &'a Value, path: &str) -> &'a Value {
    value.pointer(path).unwrap_or(&Value::Null)
}

fn records(env: &TestEnv, vars: &[(&str, &str)]) -> R<Vec<Value>> {
    let out = env.run_with(vars, &["status", "--json"]);
    if !out.status.success() {
        return Err(String::from_utf8(out.stderr)?.into());
    }
    String::from_utf8(out.stdout)?
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn record<'a>(records: &'a [Value], kind: &str) -> R<&'a Value> {
    records
        .iter()
        .find(|value| at(value, "/record") == kind)
        .ok_or_else(|| format!("missing {kind}").into())
}

fn entry<'a>(records: &'a [Value], key: &str, status: &str) -> R<&'a Value> {
    records
        .iter()
        .find(|value| at(value, "/key/raw") == key && at(value, "/status") == status)
        .ok_or_else(|| format!("missing {status}: {key}").into())
}

fn pairs(records: &[Value]) -> Vec<(String, String)> {
    records
        .iter()
        .filter_map(|value| {
            Some((
                at(value, "/status").as_str()?.to_owned(),
                at(value, "/key/raw").as_str()?.to_owned(),
            ))
        })
        .collect()
}

fn parity(env: &TestEnv, records: &[Value], vars: &[(&str, &str)]) {
    let mut expected = env.porcelain_with(vars, "status");
    let mut actual = pairs(records);
    expected.sort();
    actual.sort();
    assert_eq!(actual, expected);
}

fn escaped(value: &Value) {
    match value {
        Value::Object(object) => {
            if let Some(raw) = object.get("raw").and_then(Value::as_str) {
                assert_eq!(
                    object.get("shown").and_then(Value::as_str),
                    Some(escape_line(raw).as_ref())
                );
            }
            for child in object.values() {
                escaped(child);
            }
        }
        Value::Array(array) => {
            for child in array {
                escaped(child);
            }
        }
        _ => {}
    }
}

#[test]
fn status_json_preserves_tuples_modes_and_raw_absolute_links() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".private", b"private fixture\n");
    env.write(".private", b"private fixture\n", 0o600);
    env.ok(&["apply", "--take-repo"]);
    env.write(".target", b"target fixture\n", 0o644);
    env.symlink(".link", env.home().join(".target"));
    env.ok(&["add", ".target", ".link"]);
    let before = env.four_trees();
    let records = records(&env, &[])?;
    parity(&env, &records, &[]);
    assert_eq!(env.four_trees(), before);
    assert_eq!(at(record(&records, "context")?, "/schema_version"), 1);
    assert_eq!(at(record(&records, "context")?, "/experimental"), true);
    assert_eq!(at(record(&records, "result")?, "/exit_code"), 0);
    assert_eq!(
        records.last().and_then(|value| value.get("record")),
        Some(&json!("result"))
    );
    let state = State::read(&env.state_dir().join("state.toml"))?.ok_or("missing state")?;
    for (key, saved) in &state.files {
        let current = entry(&records, key, "clean")?;
        let expected = json!({"kind": saved.kind, "hash": saved.hash, "mode": saved.mode});
        for side in ["/r/tuple", "/h/tuple", "/s/tuple"] {
            assert_eq!(at(current, side), &expected, "{key}: {side}");
        }
    }
    let private = entry(&records, ".private", "clean")?;
    assert_eq!(at(private, "/kept_mode"), "0600");
    assert_eq!(at(private, "/repo_mode"), "0644");
    assert_eq!(at(private, "/source_tuple/tuple/mode"), "0644");
    let link = entry(&records, ".link", "clean")?;
    assert_ne!(at(link, "/raw_h/tuple/hash"), at(link, "/h/tuple/hash"));
    let raw_hash = format!(
        "{:032x}",
        xxhash_rust::xxh3::xxh3_128(env.home().join(".target").as_os_str().as_bytes())
    );
    assert_eq!(at(link, "/raw_h/tuple/hash"), raw_hash.as_str());
    Ok(())
}

#[test]
fn status_json_keeps_duplicate_hold_occurrences_without_file_content() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".a", b"initial home fixture\n", 0o644);
    env.write(".b", b"initial repo fixture\n", 0o644);
    env.ok(&["add", ".a", ".b"]);
    let secret = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789ABCD");
    env.write(".a", secret.as_bytes(), 0o644);
    env.write_repo(".b", secret.as_bytes());
    let records = records(&env, &[])?;
    parity(&env, &records, &[]);
    entry(&records, ".a", "changed-locally")?;
    let held = entry(&records, ".a", "held-in-home")?;
    assert_eq!(
        at(held, "/finding"),
        &json!({"rule":"github-token", "line":1, "place":"content"})
    );
    entry(&records, ".b", "held-in-repo")?;
    let text = serde_json::to_string(&records)?;
    for forbidden in [secret, "initial home fixture", "initial repo fixture"] {
        assert!(!text.contains(forbidden));
    }
    Ok(())
}

#[test]
fn status_json_reports_locked_and_template_errors_as_unavailable_not_missing() -> R {
    if !common::age_installed() {
        return Ok(());
    }
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["key", "init"]);
    let secret = concat!("synthetic-", "content-must-not-escape");
    env.write(".encrypted", secret.as_bytes(), 0o600);
    env.ok(&["add", "--encrypt", ".encrypted"]);
    let unlocked = records(&env, &[])?;
    let encrypted = entry(&unlocked, ".encrypted", "clean")?;
    assert_eq!(at(encrypted, "/encrypted"), true);
    assert_eq!(at(encrypted, "/r/state"), "present");
    assert_eq!(at(encrypted, "/source/raw"), ".encrypted{age}");
    assert!(!serde_json::to_string(&unlocked)?.contains(secret));
    fs::remove_file(env.xdg_config().join("dotman/age-identity.txt"))?;
    env.write_repo(
        ".bad{tmpl}",
        format!("{secret}\n{{{{ vars.undefined }}}}\n").as_bytes(),
    );
    env.write_repo(".blocked", b"blocked fixture");
    fs::create_dir(env.home().join(".blocked"))?;
    let records = records(&env, &[])?;
    parity(&env, &records, &[]);
    let locked = entry(&records, ".encrypted", "locked")?;
    assert_eq!(at(locked, "/r/state"), "unavailable");
    assert_eq!(at(locked, "/r/reason"), "locked");
    assert_eq!(at(locked, "/encrypted"), true);
    assert_eq!(at(locked, "/source/raw"), at(encrypted, "/source/raw"));
    let bad = entry(&records, ".bad", "template-error")?;
    assert_eq!(at(bad, "/r/reason"), "template_error");
    let blocked = entry(&records, ".blocked", "blocked")?;
    assert_eq!(at(blocked, "/h/state"), "blocked");
    assert_eq!(at(blocked, "/h/reason/raw"), "directory");
    assert!(!serde_json::to_string(&records)?.contains(secret));
    Ok(())
}

#[test]
fn status_json_preserves_deleted_sources_and_filter_parity() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".gone", b"gone fixture", 0o644);
    env.ok(&["add", ".gone"]);
    env.git(&["-C", &env.repo().to_string_lossy(), "add", ".gone"]);
    env.git(&[
        "-C",
        &env.repo().to_string_lossy(),
        "commit",
        "-qm",
        "fixture",
    ]);
    fs::remove_file(env.repo().join(".gone"))?;
    env.write_repo(".other", b"other fixture");
    let records = records(&env, &[])?;
    parity(&env, &records, &[]);
    let gone = entry(&records, ".gone", "forgotten")?;
    assert_eq!(at(gone, "/r/state"), "missing");
    assert_eq!(at(gone, "/h/state"), "present");
    let filtered = env.run(&["status", "--json", ".gone"]);
    assert!(filtered.status.success());
    let filtered: Vec<Value> = String::from_utf8(filtered.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        pairs(&filtered),
        vec![("forgotten".to_owned(), ".gone".to_owned())]
    );
    Ok(())
}

#[test]
fn status_json_reviews_identity_hooks_pending_commands_and_directories() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".v{host=a}", b"a");
    env.write_repo(".v{host=b}", b"b");
    let mut config = env.repo_config();
    config
        .hooks
        .insert(".v".to_owned(), "true\n# review\tfixture".to_owned());
    config.dirs.paths = vec!["incoming".to_owned()];
    config.save(&env.repo().join(".dotman.toml"), false)?;
    env.ok_with(&[("DOTMAN_HOST", "a")], &["apply"]);
    env.write("incoming/waiting", b"waiting", 0o644);
    let vars = [("DOTMAN_HOST", "b")];
    let records = records(&env, &vars)?;
    parity(&env, &records, &vars);
    let changed = entry(&records, ".v", "identity-changed")?;
    assert_eq!(at(changed, "/identity_changed"), true);
    assert_eq!(at(changed, "/source/raw"), ".v{host=b}");
    let review = record(&records, "review_context")?;
    assert_eq!(at(review, "/current_identity/host/raw"), "b");
    assert_eq!(at(review, "/recorded_identity/host/raw"), "a");
    assert_eq!(at(review, "/hooks/standing"), "unapproved");
    assert_eq!(
        at(review, "/hooks/canonical/raw"),
        serde_json::to_string(&config.hooks)?.as_str()
    );
    assert_eq!(at(review, "/hooks/pending_keys/0/raw"), ".v");
    assert_eq!(
        at(review, "/hooks/pending_commands/0/raw"),
        "true\n# review\tfixture"
    );
    assert_eq!(at(review, "/unapproved_dirs/0/path/raw"), "incoming");
    assert_eq!(at(review, "/unapproved_dirs/0/waiting"), 1);
    assert_eq!(at(review, "/waiting_keys/0/raw"), "incoming/waiting");
    for value in &records {
        escaped(value);
    }
    Ok(())
}

#[test]
fn status_json_reviews_malformed_hook_globs_without_changing_approvals() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".a", b"hook fixture");
    let mut config = env.repo_config();
    config.hooks.insert(".a".to_owned(), "true".to_owned());
    config.save(&env.repo().join(".dotman.toml"), false)?;
    env.ok(&["apply"]);
    config.hooks = [("[".to_owned(), "true".to_owned())].into();
    config.save(&env.repo().join(".dotman.toml"), false)?;
    let before = env.four_trees();
    let approval = State::read(&env.state_dir().join("state.toml"))?;
    let porcelain = env.run(&["status", "--porcelain"]);
    let out = env.run(&["status", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.status.code(), porcelain.status.code());
    assert_eq!(out.stderr, porcelain.stderr);
    let records: Vec<Value> = String::from_utf8(out.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        records
            .iter()
            .map(|value| at(value, "/record").clone())
            .collect::<Vec<_>>(),
        ["context", "entry", "review_context", "summary", "result"]
            .iter()
            .map(|kind| json!(kind))
            .collect::<Vec<_>>()
    );
    let review = record(&records, "review_context")?;
    assert_eq!(at(review, "/hooks/standing"), "invalid");
    assert_eq!(at(review, "/hooks/pending_commands"), &json!([]));
    assert_eq!(at(review, "/hooks/pending_keys/0/raw"), ".a");
    assert_eq!(
        at(review, "/hooks/canonical/raw"),
        serde_json::to_string(&config.hooks)?.as_str()
    );
    assert_eq!(at(record(&records, "result")?, "/exit_code"), 0);
    parity(&env, &records, &[]);
    assert_eq!(env.four_trees(), before);
    assert_eq!(State::read(&env.state_dir().join("state.toml"))?, approval);
    Ok(())
}

#[test]
fn status_json_retains_ignore_patterns_and_affected_entries() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write("d/managed", b"managed", 0o644);
    env.ok(&["add", "d"]);
    let state_file = env.state_dir().join("state.toml");
    let mut state = State::read(&state_file)?.ok_or("missing state")?;
    state.ignore_seen.insert("**/*.held".to_owned());
    state.save(&state_file, false)?;
    env.write("d/x.held", b"ignored fixture", 0o644);
    let before = env.four_trees();
    let records = records(&env, &[])?;
    assert_eq!(env.four_trees(), before);
    let review = record(&records, "review_context")?;
    assert_eq!(at(review, "/retained_ignore_patterns/0/raw"), "**/*.held");
    assert_eq!(at(review, "/ignored_entries/0/key/raw"), "d/x.held");
    assert_eq!(at(review, "/ignored_entries/0/pattern/raw"), "**/*.held");
    parity(&env, &records, &[]);
    Ok(())
}

#[test]
fn status_json_escapes_hostile_names_and_keeps_case_unicode_collisions() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    for key in [
        ".<img src=x onerror=canary>",
        ".bidi\u{202e}",
        ".case",
        ".CASE",
        ".café",
        ".cafe\u{301}",
    ] {
        env.write_repo(key, b"hostile name fixture");
    }
    env.write_repo("refused\n\u{1b}", b"refused fixture");
    env.git(&["-C", &env.repo().to_string_lossy(), "add", "-A"]);
    let records = records(&env, &[])?;
    parity(&env, &records, &[]);
    for value in &records {
        escaped(value);
    }
    for key in [".case", ".CASE", ".café", ".cafe\u{301}"] {
        assert_eq!(at(entry(&records, key, "collision")?, "/scope"), "home");
    }
    let review = record(&records, "review_context")?;
    assert!(
        at(review, "/warnings")
            .as_array()
            .is_some_and(|warnings| !warnings.is_empty())
    );
    assert!(serde_json::to_string(at(review, "/warnings"))?.contains(".bidi"));
    Ok(())
}

#[test]
fn status_json_paths_keep_non_utf8_bytes() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let repo = env.root().join(std::ffi::OsStr::from_bytes(b"repo-\xff"));
    fs::rename(env.repo(), &repo)?;
    let out = env
        .dot()
        .env("DOTMAN_REPO_PATH", &repo)
        .args(["status", "--json"])
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let records: Vec<Value> = String::from_utf8(out.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let context = record(&records, "context")?;
    assert_eq!(
        at(context, "/repo/bytes"),
        &json!(repo.as_os_str().as_bytes())
    );
    assert!(at(context, "/repo/raw").is_null());
    assert_eq!(
        at(context, "/repo/shown"),
        escape_line(&repo.to_string_lossy()).as_ref()
    );
    Ok(())
}

#[test]
fn status_json_system_scope_is_explicit_and_read_only() -> R {
    let mut env = TestEnv::new();
    env.use_system_root();
    env.ok(&["init"]);
    env.write_repo(".dotman/system/etc/example.conf", b"system fixture");
    env.write_system("/etc/example.conf", b"system fixture", 0o644);
    let records = records(&env, &[])?;
    parity(&env, &records, &[]);
    let system = entry(&records, "/etc/example.conf", "system-off")?;
    assert_eq!(at(system, "/scope"), "system");
    assert_eq!(at(system, "/system"), true);
    assert_eq!(at(system, "/r/tuple"), at(system, "/h/tuple"));
    Ok(())
}

#[test]
fn status_json_unreadable_home_and_variant_failures_are_not_missing() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo("x", b"plain");
    env.write_repo("x{tmpl}", b"template");
    env.write_repo("y{tmpl+age}", b"invalid");
    env.write_repo(".unreadable", b"repo fixture");
    env.write(".unreadable", b"home fixture", 0o000);
    let records = records(&env, &[])?;
    parity(&env, &records, &[]);
    let ambiguous = entry(&records, "x", "ambiguous")?;
    assert_eq!(at(ambiguous, "/r/state"), "unavailable");
    assert_eq!(at(ambiguous, "/alternatives/0/raw"), "x");
    assert_eq!(at(ambiguous, "/alternatives/1/raw"), "x{tmpl}");
    let invalid = entry(&records, "y", "invalid")?;
    assert_eq!(at(invalid, "/invalid_sources/0/source/raw"), "y{tmpl+age}");
    if dotman::sys::geteuid() != 0 {
        let unreadable = entry(&records, ".unreadable", "unreadable")?;
        assert_eq!(at(unreadable, "/h/state"), "unavailable");
        assert_eq!(at(unreadable, "/raw_h/state"), "unavailable");
    }
    fs::set_permissions(
        env.home().join(".unreadable"),
        fs::Permissions::from_mode(0o600),
    )?;
    Ok(())
}

#[test]
fn status_json_remote_position_is_last_fetched_or_explicitly_unknown() -> R {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    let initial = records(&env, &[])?;
    assert_eq!(
        at(record(&initial, "review_context")?, "/remote/basis"),
        "unknown"
    );
    env.write(".a", b"remote fixture", 0o644);
    env.ok(&["add", ".a"]);
    env.git(&["-C", &env.repo().to_string_lossy(), "add", "-A"]);
    env.git(&[
        "-C",
        &env.repo().to_string_lossy(),
        "commit",
        "-qm",
        "fixture",
    ]);
    let unfetched = records(&env, &[])?;
    assert_eq!(
        at(record(&unfetched, "review_context")?, "/remote/basis"),
        "not_fetched"
    );
    env.ok(&["sync"]);
    let fetched = records(&env, &[])?;
    let review = record(&fetched, "review_context")?;
    assert_eq!(at(review, "/remote/basis"), "last_fetched");
    assert_eq!(at(review, "/remote/ahead"), 0);
    assert_eq!(at(review, "/remote/behind"), 0);
    Ok(())
}

#[test]
fn status_json_early_failure_has_only_one_error_result_and_flag_conflicts() -> R {
    let env = TestEnv::new();
    let repo = env.root().join("setup-repository");
    let before = env.four_trees();
    let out = env
        .dot()
        .env("DOTMAN_REPO_PATH", &repo)
        .args(["status", "--json"])
        .output()?;
    assert_eq!(out.status.code(), Some(1));
    let records: Vec<Value> = String::from_utf8(out.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(records.len(), 2);
    assert_eq!(
        at(records.first().ok_or("missing context")?, "/record"),
        "context"
    );
    let context = record(&records, "context")?;
    assert_eq!(
        at(context, "/repo/raw"),
        repo.to_str().ok_or("fixture path not UTF-8")?
    );
    assert!(context.get("repo_id").is_some_and(Value::is_null));
    assert_eq!(at(record(&records, "result")?, "/exit_code"), 1);
    assert_eq!(at(record(&records, "result")?, "/outcome"), "error");
    assert_eq!(
        at(records.last().ok_or("missing result")?, "/record"),
        "result"
    );
    assert_eq!(env.four_trees(), before);
    assert!(!repo.exists());
    let conflict = env.run(&["status", "--json", "--porcelain"]);
    assert_eq!(conflict.status.code(), Some(2));
    assert_eq!(conflict.stdout, [] as [u8; 0]);
    env.ok(&["init"]);
    let id_file = env.repo().join(".git/dotman-id");
    let id = fs::read(&id_file)?;
    fs::write(&id_file, [0xff])?;
    let before = env.four_trees();
    let porcelain = env.run(&["status", "--porcelain"]);
    let out = env.run(&["status", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.status.code(), porcelain.status.code());
    assert_eq!(out.stderr, porcelain.stderr);
    let records: Vec<Value> = String::from_utf8(out.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(records.len(), 2);
    assert!(
        record(&records, "context")?
            .get("repo_id")
            .is_some_and(Value::is_null)
    );
    assert_eq!(at(record(&records, "result")?, "/exit_code"), 1);
    assert_eq!(env.four_trees(), before);
    fs::write(&id_file, id)?;
    fs::create_dir_all(env.repo().join(".git/rebase-merge"))?;
    let before = env.four_trees();
    let porcelain = env.run(&["status", "--porcelain"]);
    let out = env.run(&["status", "--json"]);
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(out.status.code(), porcelain.status.code());
    assert_eq!(out.stderr, porcelain.stderr);
    let records: Vec<Value> = String::from_utf8(out.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    assert_eq!(
        at(record(&records, "result")?, "/error_kind"),
        "in_progress"
    );
    assert_eq!(records.len(), 2);
    assert!(
        record(&records, "context")?
            .get("repo_id")
            .is_some_and(Value::is_null)
    );
    assert_eq!(env.four_trees(), before);
    Ok(())
}

#[test]
#[ignore = "10k-key same-tree timing receipt; run in release mode with --nocapture"]
fn status_json_10k_same_tree_benchmark() -> R {
    let env = TestEnv::new();
    env.ok(&["init"]);
    for n in 0..10_000 {
        env.write_repo(&format!("d/key-{n:05}"), b"benchmark fixture\n");
    }
    let mut porcelain = Vec::new();
    let mut json_times = Vec::new();
    for round in 0..8 {
        for mode in if round % 2 == 0 {
            ["--porcelain", "--json"]
        } else {
            ["--json", "--porcelain"]
        } {
            let start = Instant::now();
            let out = env.run(&["status", mode]);
            let elapsed = start.elapsed().as_micros();
            assert!(out.status.success());
            if round != 0 {
                if mode == "--json" {
                    json_times.push(elapsed);
                } else {
                    porcelain.push(elapsed);
                }
            }
        }
    }
    porcelain.sort_unstable();
    json_times.sort_unstable();
    let p = porcelain.get(3).ok_or("missing porcelain median")?;
    let j = json_times.get(3).ok_or("missing JSON median")?;
    println!(
        "10k keys, release CLI, warm round discarded, 7 alternating samples per mode, captured stdout; porcelain_us={porcelain:?}; json_us={json_times:?}; median_porcelain_us={p}; median_json_us={j}"
    );
    assert!(
        *j <= p.saturating_mul(2),
        "JSON median exceeds 2x porcelain"
    );
    Ok(())
}
