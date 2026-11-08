//! `dot import` and `dot import --overlay` against the legacy-v2 fixture, which is read-only.

mod common;

use common::{Entry, TestEnv, accounting, mode_of, tree_checksum};
use dotman::repo_config::RepoConfig;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use xxhash_rust::xxh3::xxh3_128;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/legacy-v2/repo");
const FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/legacy-v2");
const OVERLAY_MESSAGE: &str = "import: overlay from legacy dotman";
const ZERO: &str = "00000000000000000000000000000000";

/// HEAD of the fixture as its README lists it: key, git mode, xxh3 (zero means empty).
const MANIFEST: [(&str, &str, &str); 6] = [
    (".bashrc", "100644", "4658b874248d98c42d6da39e3e2e9e11"),
    (
        ".config/abs.conf",
        "100644",
        "0b7c91d785e41df5639779f3afec3634",
    ),
    (
        ".config/side.toml",
        "100644",
        "cb891aff7d073dd2e3076d70272c8ca2",
    ),
    (".empty", "100644", ZERO),
    (
        ".local/bin/run",
        "100755",
        "35d709aba01c13cbfa76c57292b6ec0e",
    ),
    (".ssh/config", "100644", "044442cc692e68f4c0ad058053236021"),
];

/// Hashes the fixture when created; `check` fails if the import changed it.
struct Pristine(BTreeMap<String, Entry>);

impl Pristine {
    fn new() -> Self {
        Self(tree_checksum(Path::new(FIXTURE_ROOT)))
    }

    fn check(&self) {
        assert_eq!(
            tree_checksum(Path::new(FIXTURE_ROOT)),
            self.0,
            "the legacy fixture was modified"
        );
    }
}

// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("create dir");
    for item in fs::read_dir(from).expect("read dir") {
        let item = item.expect("dir entry");
        let target = to.join(item.file_name());
        if item.file_type().expect("file type").is_dir() {
            copy_dir(&item.path(), &target);
        } else {
            fs::copy(item.path(), &target).expect("copy file");
        }
    }
}

fn git_in(env: &TestEnv, repo: &Path, args: &[&str]) -> String {
    let repo = repo.to_string_lossy();
    let mut full = vec!["-C", &*repo];
    full.extend_from_slice(args);
    env.git(&full)
}

fn blob(env: &TestEnv, rev_path: &str) -> Vec<u8> {
    let repo = env.repo();
    let out = env.git_raw(&["-C", &repo.to_string_lossy(), "cat-file", "blob", rev_path]);
    assert!(out.status.success(), "no {rev_path}");
    out.stdout
}

// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn commits(env: &TestEnv) -> usize {
    git_in(env, &env.repo(), &["rev-list", "--count", "HEAD"])
        .trim()
        .parse()
        .expect("a count")
}

fn mode_table(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
    entries
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn import_rebuilds_the_history_and_matches_the_readme_manifest() {
    let pristine = Pristine::new();
    let env = TestEnv::new();
    let home_before = tree_checksum(&env.home());
    let err = env.ok(&["import", FIXTURE, "--no-bootstrap"]);

    assert_eq!(accounting(&err), (6, 6, 0), "{err}");
    assert!(err.contains("run `dot status`, then `dot apply`"), "{err}");
    assert!(err.contains("remote-mappings.toml is ignored"), "{err}");
    assert!(err.contains("/home/legacyuser/.config/abs.conf as .config/abs.conf"));

    let repo = env.repo();
    assert_eq!(commits(&env), 4);
    let head_parents = git_in(&env, &repo, &["log", "-1", "--format=%P", "HEAD"]);
    assert_eq!(head_parents.split_whitespace().count(), 2, "{head_parents}");
    let log = git_in(&env, &repo, &["log", "--format=%at|%an|%ae|%s"]);
    let lines: BTreeSet<&str> = log.lines().collect();
    let expected = [
        "1700000000|legacyuser|test@example.invalid|c1: initial",
        "1700000100|legacyuser|test@example.invalid|c2: edit bashrc, add absolute key",
        "1700000200|legacyuser|test@example.invalid|side: add side.toml",
        "1700000300|legacyuser|test@example.invalid|c3: merge side into main",
    ];
    assert_eq!(lines, BTreeSet::from(expected));

    let tree = git_in(&env, &repo, &["ls-tree", "-r", "HEAD"]);
    let mut listed = BTreeMap::new();
    for line in tree.lines() {
        let (meta, path) = line.split_once('\t').expect("tab");
        listed.insert(
            path.to_owned(),
            meta.split(' ').next().expect("mode").to_owned(),
        );
    }
    assert_eq!(listed.len(), 7, "{listed:?}");
    for (key, mode, hash) in MANIFEST {
        assert_eq!(listed.get(key).map(String::as_str), Some(mode), "{key}");
        let bytes = blob(&env, &format!("HEAD:{key}"));
        if hash == ZERO {
            assert!(bytes.is_empty(), "{key}");
        } else {
            assert_eq!(format!("{:032x}", xxh3_128(&bytes)), hash, "{key}");
        }
        assert_eq!(
            fs::read(repo.join(key)).expect("worktree file"),
            bytes,
            "{key}"
        );
    }
    assert!(!listed.keys().any(|key| key.starts_with('/')));

    let modes = mode_table(&[(".ssh/config", "0600")]);
    assert_eq!(env.repo_config().modes, modes);
    for oid in git_in(&env, &repo, &["rev-list", "HEAD"]).lines() {
        let config = blob(&env, &format!("{oid}:.dotman.toml"));
        let config = RepoConfig::parse(&String::from_utf8(config).expect("utf8")).expect("parse");
        assert_eq!(config.modes, modes, "{oid}");
    }

    assert_eq!(git_in(&env, &repo, &["status", "--porcelain"]), "");
    assert_eq!(tree_checksum(&env.home()), home_before, "HOME is untouched");
    assert!(!env.state_dir().join("state.toml").exists());
    assert!(!env.state_dir().join("backups").exists());
    pristine.check();
}

#[test]
fn an_imported_repo_applies_with_the_recorded_modes() {
    let pristine = Pristine::new();
    let env = TestEnv::new();
    env.ok(&["import", FIXTURE]);
    env.ok(&["apply"]);
    assert_eq!(mode_of(&env.home().join(".ssh/config")), 0o600);
    assert_eq!(mode_of(&env.home().join(".local/bin/run")), 0o755);
    assert_eq!(mode_of(&env.home().join(".bashrc")), 0o644);
    assert_eq!(env.read(".config/abs.conf"), b"abs=true\n");
    assert_eq!(env.read(".empty"), Vec::<u8>::new());
    let statuses = env.porcelain("status");
    assert_eq!(statuses.len(), 6);
    assert!(
        statuses.iter().all(|(token, _)| token == "clean"),
        "{statuses:?}"
    );
    pristine.check();
}

#[test]
fn import_dry_run_reports_and_creates_nothing() {
    let pristine = Pristine::new();
    let env = TestEnv::new();
    let before = env.four_trees();
    let err = env.ok(&["import", "--dry-run", FIXTURE]);
    assert_eq!(accounting(&err), (6, 6, 0), "{err}");
    assert!(err.contains("would import 4 commits"), "{err}");
    assert_eq!(env.four_trees(), before);
    assert!(!env.repo().exists() && !env.state_dir().exists());

    env.ok(&["init"]);
    let before = env.four_trees();
    env.ok(&["import", "--dry-run", FIXTURE]);
    assert_eq!(env.four_trees(), before);
    assert_eq!(
        fs::read_dir(env.repo().join(".git/refs/heads"))
            .expect("refs")
            .count(),
        0
    );
    pristine.check();
}

#[test]
fn without_a_source_the_legacy_repo_in_home_is_used() {
    let pristine = Pristine::new();
    let env = TestEnv::new();
    let err = env.fails(&["import"]);
    assert!(err.contains("dot import <source>"), "{err}");
    assert!(!env.repo().exists());

    copy_dir(Path::new(FIXTURE), &env.home().join(".dotman"));
    let err = env.ok(&["import"]);
    assert_eq!(accounting(&err), (6, 6, 0), "{err}");
    assert_eq!(commits(&env), 4);
    pristine.check();
}

#[test]
fn a_populated_repo_is_refused_and_the_error_names_overlay() {
    let pristine = Pristine::new();
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".bashrc", b"mine\n", 0o644);
    env.ok(&["add", ".bashrc"]);
    let before = env.four_trees();
    for args in [&["import", FIXTURE][..], &["import", "--dry-run", FIXTURE]] {
        let err = env.fails(args);
        assert!(err.contains("--overlay"), "{err}");
    }
    assert_eq!(env.four_trees(), before);

    env.ok(&["sync"]);
    let before = env.four_trees();
    let err = env.fails(&["import", FIXTURE]);
    assert!(err.contains("--overlay"), "{err}");
    assert_eq!(env.four_trees(), before);
    pristine.check();
}

#[test]
fn a_source_that_is_in_or_around_the_repo_is_refused() {
    let pristine = Pristine::new();
    let env = TestEnv::new();
    env.ok(&["init"]);
    let inside = env.repo().join("legacy");
    copy_dir(Path::new(FIXTURE), &inside);
    let inside_before = tree_checksum(&inside);
    let err = env.fails(&["import", &inside.to_string_lossy()]);
    assert!(err.contains("inside the dotman repo"), "{err}");
    assert_eq!(tree_checksum(&inside), inside_before);

    let legacy = env.root().join("legacy-copy");
    copy_dir(Path::new(FIXTURE), &legacy);
    let legacy_before = tree_checksum(&legacy);
    let source = legacy.to_string_lossy().into_owned();
    let nested = legacy.join("nested").to_string_lossy().into_owned();
    for repo in [source.as_str(), nested.as_str()] {
        let out = env.run_with(&[("DOTMAN_REPO_PATH", repo)], &["import", &source]);
        assert_eq!(out.status.code(), Some(1));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("inside the dotman repo"), "{err}");
    }
    assert_eq!(tree_checksum(&legacy), legacy_before);
    pristine.check();
}

#[test]
fn the_default_config_is_replaced_silently() {
    let pristine = Pristine::new();
    let env = TestEnv::new();
    env.ok(&["init", "--no-bootstrap"]);
    let err = env.ok(&["import", FIXTURE, "--no-bootstrap"]);
    assert!(
        !err.contains("edited") && !err.contains("backed up"),
        "{err}"
    );
    assert!(!env.state_dir().join("backups").exists());
    assert_eq!(
        env.repo_config().modes,
        mode_table(&[(".ssh/config", "0600")])
    );
    assert_eq!(commits(&env), 4);
    assert_eq!(git_in(&env, &env.repo(), &["status", "--porcelain"]), "");
    pristine.check();
}

#[test]
fn an_edited_config_is_backed_up_and_reported() {
    let pristine = Pristine::new();
    let env = TestEnv::new();
    env.ok(&["init"]);
    let config = env.repo().join(".dotman.toml");
    let mut text = fs::read_to_string(&config).expect("read config");
    text.push_str("# my own notes\n");
    fs::write(&config, &text).expect("edit config");

    let before = env.four_trees();
    let err = env.ok(&["import", "--dry-run", FIXTURE]);
    assert!(err.contains("was edited; it would be backed up"), "{err}");
    assert_eq!(env.four_trees(), before);

    let err = env.ok(&["import", FIXTURE]);
    assert!(err.contains("was edited; it will be backed up"), "{err}");
    let backups = env.backups(".dotman.toml");
    assert_eq!(backups, [(text.into_bytes(), 0o600)]);
    let now = fs::read_to_string(&config).expect("read config");
    assert!(!now.contains("my own notes"));
    assert_eq!(
        env.repo_config().modes,
        mode_table(&[(".ssh/config", "0600")])
    );
    pristine.check();
}

#[test]
fn a_missing_git_identity_stops_the_import_before_anything_is_created() {
    let pristine = Pristine::new();
    let env = TestEnv::new();
    fs::remove_file(env.home().join(".gitconfig")).expect("remove gitconfig");
    let err = env.fails(&["import", FIXTURE]);
    assert!(err.contains("user.name"), "{err}");
    assert!(!env.repo().exists());
    pristine.check();
}

#[test]
fn overlay_flags_are_checked() {
    let pristine = Pristine::new();
    let env = TestEnv::new();
    for source in [None, Some(FIXTURE)] {
        let mut args = vec!["import", "--include-missing"];
        args.extend(source);
        let err = env.fails(&args);
        assert!(err.contains("only applies to --overlay"), "{err}");
    }
    let out = env.run(&["import", FIXTURE, "--overlay", FIXTURE]);
    assert_eq!(out.status.code(), Some(2));
    assert!(!env.repo().exists());
    pristine.check();
}

// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
/// A `$HOME`-shaped git repo holding the fixture's HEAD with every mode 0644, `.ssh/config`
/// present and `.config/side.toml` absent. `run_mode` is the mode of `.local/bin/run`.
fn overlay_source(env: &TestEnv, run_mode: u32, bashrc: &[u8]) -> PathBuf {
    let src = env.root().join("overlay-src");
    let files: [(&str, &[u8], u32); 5] = [
        (".bashrc", bashrc, 0o644),
        (".local/bin/run", b"#!/bin/sh\necho run\n", run_mode),
        (".ssh/config", b"Host example\n  User legacyuser\n", 0o644),
        (".empty", b"", 0o644),
        (".config/abs.conf", b"abs=true\n", 0o644),
    ];
    for (key, bytes, mode) in files {
        let path = src.join(key);
        fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
        fs::write(&path, bytes).expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("chmod");
    }
    git_in(env, &src, &["init", "-q"]);
    git_in(env, &src, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    git_in(env, &src, &["add", "-A"]);
    git_in(env, &src, &["commit", "-q", "-m", "base"]);
    src
}

const BASHRC: &[u8] = b"# bashrc v2\nexport EDITOR=vim\n";

fn overlay_env(run_mode: u32, bashrc: &[u8]) -> TestEnv {
    let env = TestEnv::new();
    let src = overlay_source(&env, run_mode, bashrc);
    env.ok(&["init", &src.to_string_lossy(), "--take-repo"]);
    env
}

/// The git mode of `key` in `HEAD`, such as `100755`.
fn tree_mode(env: &TestEnv, key: &str) -> String {
    let listing = git_in(env, &env.repo(), &["ls-tree", "HEAD", "--", key]);
    listing.split(' ').next().unwrap_or_default().to_owned()
}

fn changed_files(env: &TestEnv) -> BTreeSet<String> {
    git_in(
        env,
        &env.repo(),
        &["show", "--name-only", "--format=", "HEAD"],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

#[test]
fn overlay_records_missing_modes_reports_missing_files_and_makes_one_commit() {
    let pristine = Pristine::new();
    let env = overlay_env(0o644, BASHRC);
    let before = commits(&env);
    let home_before = tree_checksum(&env.home());

    let out = env.run(&["import", "--overlay", FIXTURE]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (6, 5, 1), "{err}");
    assert!(err.contains(".config/side.toml: not in the repo"), "{err}");
    assert!(err.contains("--include-missing"), "{err}");

    assert_eq!(
        env.repo_config().modes,
        mode_table(&[(".ssh/config", "0600")])
    );
    assert!(!env.repo().join(".config/side.toml").exists());
    assert_eq!(commits(&env), before.saturating_add(1));
    let subject = git_in(&env, &env.repo(), &["log", "-1", "--format=%s"]);
    assert_eq!(subject.trim(), OVERLAY_MESSAGE);
    assert_eq!(
        changed_files(&env),
        BTreeSet::from([".dotman.toml".to_owned(), ".local/bin/run".to_owned()])
    );
    assert_eq!(tree_mode(&env, ".local/bin/run"), "100755");
    assert_eq!(tree_mode(&env, ".bashrc"), "100644");
    assert_eq!(git_in(&env, &env.repo(), &["status", "--porcelain"]), "");
    assert_eq!(tree_checksum(&env.home()), home_before, "HOME is untouched");

    env.ok(&["apply"]);
    assert_eq!(mode_of(&env.home().join(".ssh/config")), 0o600);
    assert_eq!(mode_of(&env.home().join(".local/bin/run")), 0o755);
    pristine.check();
}

#[test]
fn overlay_with_include_missing_adds_the_file() {
    let pristine = Pristine::new();
    let env = overlay_env(0o644, BASHRC);
    let before = commits(&env);

    let err = env.ok(&["import", "--overlay", FIXTURE, "--include-missing"]);
    assert_eq!(accounting(&err), (6, 6, 0), "{err}");
    assert_eq!(
        fs::read(env.repo().join(".config/side.toml")).expect("added file"),
        b"side = 1\n"
    );
    assert_eq!(mode_of(&env.repo().join(".config/side.toml")), 0o644);
    assert_eq!(
        env.repo_config().modes,
        mode_table(&[(".ssh/config", "0600")])
    );
    assert_eq!(commits(&env), before.saturating_add(1));
    assert_eq!(
        changed_files(&env),
        BTreeSet::from([
            ".config/side.toml".to_owned(),
            ".dotman.toml".to_owned(),
            ".local/bin/run".to_owned()
        ])
    );
    assert_eq!(tree_mode(&env, ".local/bin/run"), "100755");
    assert_eq!(tree_mode(&env, ".config/side.toml"), "100644");
    assert_eq!(git_in(&env, &env.repo(), &["status", "--porcelain"]), "");

    let err = env.ok(&["import", "--overlay", FIXTURE, "--include-missing"]);
    assert!(err.contains("no commit made"), "{err}");
    assert_eq!(commits(&env), before.saturating_add(1));
    pristine.check();
}

#[test]
fn overlay_reports_an_extra_exec_bit_and_leaves_it() {
    let pristine = Pristine::new();
    let env = overlay_env(0o644, BASHRC);
    let repo = env.repo();
    fs::set_permissions(repo.join(".bashrc"), fs::Permissions::from_mode(0o755))
        .expect("make it executable");
    git_in(&env, &repo, &["add", "-A"]);
    git_in(&env, &repo, &["commit", "-q", "-m", "exec"]);
    assert_eq!(tree_mode(&env, ".bashrc"), "100755");
    let before = commits(&env);

    let out = env.run(&["import", "--overlay", FIXTURE, "--include-missing"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (6, 5, 1), "{err}");
    assert!(
        err.contains(".bashrc: the repo file is executable and the legacy mode 0644 is not"),
        "{err}"
    );
    assert_eq!(tree_mode(&env, ".bashrc"), "100755");
    assert_eq!(mode_of(&repo.join(".bashrc")), 0o755);
    assert!(!changed_files(&env).contains(".bashrc"));
    assert_eq!(commits(&env), before.saturating_add(1));
    pristine.check();
}

#[test]
fn overlay_with_nothing_to_change_makes_no_commit() {
    let pristine = Pristine::new();
    let env = overlay_env(0o755, BASHRC);
    env.ok(&["import", "--overlay", FIXTURE, "--include-missing"]);
    let before = commits(&env);
    let head = git_in(&env, &env.repo(), &["rev-parse", "HEAD"]);

    let err = env.ok(&["import", "--overlay", FIXTURE, "--include-missing"]);
    assert_eq!(accounting(&err), (6, 6, 0), "{err}");
    assert!(err.contains("no commit made"), "{err}");
    assert_eq!(commits(&env), before);
    assert_eq!(git_in(&env, &env.repo(), &["rev-parse", "HEAD"]), head);
    pristine.check();
}

#[test]
fn overlay_reports_content_differences_and_leaves_them() {
    let pristine = Pristine::new();
    let env = overlay_env(0o644, b"# my own bashrc\n");
    let out = env.run(&["import", "--overlay", FIXTURE]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (6, 4, 2), "{err}");
    assert!(err.contains("content differs"), "{err}");
    assert!(err.contains("\n  .bashrc\n"), "{err}");
    assert_eq!(
        fs::read(env.repo().join(".bashrc")).expect("bashrc"),
        b"# my own bashrc\n"
    );
    pristine.check();
}

#[test]
fn overlay_never_overrides_an_existing_mode_entry() {
    let pristine = Pristine::new();
    let env = overlay_env(0o755, BASHRC);
    let config = env.repo().join(".dotman.toml");
    let mut parsed = RepoConfig::load(&config).expect("load");
    parsed
        .modes
        .insert(".ssh/config".to_owned(), "0640".to_owned());
    parsed.save(&config, false).expect("save");
    git_in(&env, &env.repo(), &["add", "-A"]);
    git_in(&env, &env.repo(), &["commit", "-q", "-m", "mine"]);
    let before = commits(&env);

    let out = env.run(&["import", "--overlay", FIXTURE]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(".ssh/config: [modes] has 0640, the legacy mode is 0600"),
        "{err}"
    );
    assert_eq!(
        env.repo_config().modes,
        mode_table(&[(".ssh/config", "0640")])
    );
    assert_eq!(commits(&env), before, "nothing else to change");
    pristine.check();
}

#[test]
fn overlay_dry_run_changes_nothing() {
    let pristine = Pristine::new();
    let env = overlay_env(0o644, BASHRC);
    let before = env.four_trees();
    let out = env.run(&[
        "import",
        "--overlay",
        FIXTURE,
        "--include-missing",
        "--dry-run",
    ]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(
        err.contains(&format!("would commit: {OVERLAY_MESSAGE}")),
        "{err}"
    );
    assert_eq!(env.four_trees(), before);
    pristine.check();
}

#[test]
fn overlay_needs_a_clean_repo_that_exists() {
    let pristine = Pristine::new();
    let env = TestEnv::new();
    let err = env.fails(&["import", "--overlay", FIXTURE]);
    assert!(err.contains("dot init"), "{err}");

    let env = overlay_env(0o644, BASHRC);
    env.write_repo("untracked", b"x");
    let err = env.fails(&["import", "--overlay", FIXTURE]);
    assert!(err.contains("uncommitted changes"), "{err}");
    pristine.check();
}
