//! Smoke test for the hermetic harness in `tests/common`.

mod common;

use common::{Entry, Kind, TestEnv, tree_checksum};
use std::path::PathBuf;
use xxhash_rust::xxh3::xxh3_128;

#[test]
fn dot_version_runs_in_isolated_env() {
    let env = TestEnv::new();
    env.dot().arg("--version").assert().success();
    assert!(env.remote_path().join("HEAD").exists());
    assert_eq!(env.remote_refs(), "");
    assert_eq!(env.second_device().remote_path(), env.remote_path());
}

#[test]
fn tree_checksum_known_tree() {
    let env = TestEnv::new();
    let tree = env.root().join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("a.txt"), b"alpha").unwrap();
    std::fs::create_dir(tree.join("sub")).unwrap();
    std::fs::write(tree.join("sub/b.txt"), b"beta").unwrap();
    std::os::unix::fs::symlink("a.txt", tree.join("link")).unwrap();
    for (p, m) in [
        ("", 0o755),
        ("a.txt", 0o644),
        ("sub", 0o755),
        ("sub/b.txt", 0o600),
    ] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tree.join(p), std::fs::Permissions::from_mode(m)).unwrap();
    }

    let hex = |b: &[u8]| Some(format!("{:032x}", xxh3_128(b)));
    let mut got = tree_checksum(&tree);
    let link = got.get_mut("link").unwrap();
    assert_eq!(link.kind, Kind::Symlink);
    link.mode = 0; // symlink mode is platform-defined

    let expected = [
        (
            "a.txt",
            Entry {
                kind: Kind::File,
                mode: 0o644,
                hash: hex(b"alpha"),
                target: None,
            },
        ),
        (
            "link",
            Entry {
                kind: Kind::Symlink,
                mode: 0,
                hash: None,
                target: Some(PathBuf::from("a.txt")),
            },
        ),
        (
            "sub",
            Entry {
                kind: Kind::Dir,
                mode: 0o755,
                hash: None,
                target: None,
            },
        ),
        (
            "sub/b.txt",
            Entry {
                kind: Kind::File,
                mode: 0o600,
                hash: hex(b"beta"),
                target: None,
            },
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    assert_eq!(got, expected);
}

#[test]
fn git_uses_harness_identity_only() {
    let env = TestEnv::new();
    assert_eq!(
        env.git(&["config", "--get", "user.email"]).trim(),
        "test@example.invalid"
    );
    assert!(
        env.git(&["var", "GIT_AUTHOR_IDENT"])
            .contains("Test User <test@example.invalid>")
    );
}
