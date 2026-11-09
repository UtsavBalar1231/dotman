mod common;

use common::TestEnv;

/// A device with a remote and a committed repo whose index holds each of `keys`.
fn twin_repo(keys: &[&str]) -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["init", "--remote", &env.remote_url()]);
    let repo = env.repo().to_string_lossy().into_owned();
    let blob = env.git(&["-C", &repo, "hash-object", "-w", "/dev/null"]);
    for key in keys {
        let info = format!("100644,{},{key}", blob.trim());
        env.git(&["-C", &repo, "update-index", "--add", "--cacheinfo", &info]);
    }
    env.git(&["-C", &repo, "commit", "-q", "-m", "twins"]);
    env
}

fn assert_refused(env: &TestEnv, a: &str, b: &str) {
    for args in [
        &["apply"][..],
        &["apply", "--dry-run"],
        &["sync"],
        &["sync", "--dry-run"],
    ] {
        let before = (env.four_trees(), env.remote_refs());
        let err = env.fails(args);
        assert!(err.contains(a) && err.contains(b), "{err}");
        assert!(err.contains("differ only in case or Unicode"), "{err}");
        assert!(err.contains("git mv"), "{err}");
        assert_eq!((env.four_trees(), env.remote_refs()), before, "{args:?}");
    }
    let rows = env.porcelain("status");
    for key in [a, b] {
        assert!(
            rows.iter().any(|(t, k)| t == "collision" && k == key),
            "{rows:?}"
        );
    }
}

#[test]
fn keys_that_differ_only_in_case_are_reported_and_refused() {
    let env = twin_repo(&["x/A.conf", "x/a.conf"]);
    assert_refused(&env, "x/A.conf", "x/a.conf");
}

#[test]
fn keys_that_differ_only_in_normalization_are_reported_and_refused() {
    let env = twin_repo(&["caf\u{e9}.conf", "cafe\u{301}.conf"]);
    assert_refused(&env, "caf\u{e9}.conf", "cafe\u{301}.conf");
}

#[test]
fn system_sources_that_differ_only_in_case_are_refused() {
    let env = twin_repo(&[".dotman/system/etc/A.conf", ".dotman/system/etc/a.conf"]);
    assert_refused(&env, "/etc/A.conf", "/etc/a.conf");
}

#[test]
fn a_variant_and_its_plain_file_are_not_twins() {
    let env = twin_repo(&["x/a.conf", "x/a.conf{os=linux}"]);
    assert!(
        env.porcelain("status")
            .iter()
            .all(|(t, _)| t != "collision")
    );
    env.ok(&["apply", "--dry-run"]);
}
