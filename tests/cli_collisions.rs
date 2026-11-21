mod common;

use common::TestEnv;

fn assert_commit_paths(env: &TestEnv, keys: &[&str]) {
    let repo = env.repo().to_string_lossy().into_owned();
    let out = env.git_raw(&["-C", &repo, "ls-tree", "-r", "-z", "--name-only", "HEAD"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let paths: Vec<_> = out.stdout.split(|byte| *byte == 0).collect();
    for key in keys {
        assert_eq!(
            paths.iter().filter(|path| **path == key.as_bytes()).count(),
            1,
            "{paths:?}"
        );
    }
}

/// A device with a remote and a committed repo whose index holds each of `keys`.
fn twin_repo(keys: &[&str]) -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["init", "--remote", &env.remote_url()]);
    let repo = env.repo().to_string_lossy().into_owned();
    let entries: Vec<_> = keys.iter().map(|key| (*key, &b""[..])).collect();
    env.stage_index_entries(&entries);
    env.git(&["-C", &repo, "commit", "-q", "-m", "twins"]);
    assert_commit_paths(&env, keys);
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

#[test]
fn a_pair_that_arrives_with_the_pull_stops_sync_before_anything_is_written() {
    let a = TestEnv::new();
    a.ok(&["init"]);
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write(".a", b"a\n", 0o644);
    a.ok(&["add", ".a"]);
    a.ok(&["sync"]);
    let remote = a.remote_path().to_string_lossy().into_owned();
    a.git(&[
        "--git-dir",
        &remote,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    let b = a.second_device();
    b.ok(&["init", &b.remote_url(), "--take-repo"]);

    let keys = [".config/x/Case.conf", ".config/x/case.conf"];
    a.stage_index_entries(&[
        (".config/x/Case.conf", b"upper\n"),
        (".config/x/case.conf", b"lower\n"),
    ]);
    let repo = a.repo().to_string_lossy().into_owned();
    a.git(&["-C", &repo, "commit", "-q", "-m", "both"]);
    assert_commit_paths(&a, &keys);
    assert_eq!(
        a.git(&["-C", &repo, "show", "HEAD:.config/x/Case.conf"]),
        "upper\n"
    );
    assert_eq!(
        a.git(&["-C", &repo, "show", "HEAD:.config/x/case.conf"]),
        "lower\n"
    );
    a.git(&["-C", &repo, "push", "-q", "origin", "main"]);
    let published = a.remote_refs();

    b.write(".a", b"edited on b\n", 0o644);
    let err = b.fails(&["sync"]);
    assert!(err.contains(".config/x/Case.conf"), "{err}");
    assert!(err.contains("differ only in case or Unicode"), "{err}");
    for key in keys {
        assert!(!b.home().join(key).exists(), "{key}");
    }
    assert_commit_paths(&b, &keys);
    assert_eq!(b.remote_refs(), published, "nothing is pushed");
}
