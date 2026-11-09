//! Encrypted `{age}` files and `dotman key`, through the binary. Tests that need an age CLI print why
//! they skip when neither age nor rage is on PATH; `DOTMAN_REQUIRE_AGE=1` makes that a failure.

mod common;

use common::{TestEnv, age_installed, files_containing, mode_of};
use dotman::repo_config::RepoConfig;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;

const HOSTS: &str = ".config/gh/hosts.yml";
const HOSTS_AGE: &str = ".config/gh/hosts{age}.yml";
const PLAIN: &[u8] = b"github.com:\n  oauth_token: gho_hunter2\n";
const ARMOR: &[u8] = b"-----BEGIN AGE ENCRYPTED FILE-----";
const MARKER: &[u8] = b"AGE-SECRET-KEY-";
// The guard matches the shape, `AGE-SECRET-KEY-1` and 58 bech32 characters, so this need not be a
// real identity.
const FAKE_KEY: &str =
    "AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ";

fn exits(out: &Output, code: i32) -> String {
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(code), "{err}");
    err
}

fn identity_path(env: &TestEnv) -> PathBuf {
    env.xdg_config().join("dotman/age-identity.txt")
}

fn identity(env: &TestEnv) -> String {
    fs::read_to_string(identity_path(env)).unwrap_or_default()
}

fn ciphertext(env: &TestEnv) -> Vec<u8> {
    fs::read(env.repo().join(HOSTS_AGE)).unwrap_or_default()
}

fn remote_path(env: &TestEnv) -> String {
    env.remote_path().to_string_lossy().into_owned()
}

/// Every blob ever pushed, as one text.
fn remote_history(env: &TestEnv) -> String {
    env.git(&[
        "--git-dir",
        &remote_path(env),
        "log",
        "--all",
        "-p",
        "--format=",
    ])
}

fn remote_tree(env: &TestEnv) -> String {
    env.git(&[
        "--git-dir",
        &remote_path(env),
        "ls-tree",
        "-r",
        "--name-only",
        "main",
    ])
}

fn remote_head(env: &TestEnv) -> String {
    env.git(&["--git-dir", &remote_path(env), "rev-parse", "main"])
}

/// A device with a key and `.config/gh/hosts.yml` added encrypted, synced to the remote.
fn keyed_device() -> TestEnv {
    encrypted_at(0o600).0
}

/// Like [`keyed_device`], with the file at `mode` when it is added; also returns `add`'s stderr.
fn encrypted_at(mode: u32) -> (TestEnv, String) {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.ok(&["key", "init"]);
    env.write(HOSTS, PLAIN, mode);
    let added = env.ok(&["add", "--encrypt", HOSTS]);
    env.ok(&["sync"]);
    env.git(&[
        "--git-dir",
        &remote_path(&env),
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    (env, added)
}

/// A second device that applies with `env`'s key.
fn keyed_clone(env: &TestEnv) -> TestEnv {
    let other = env.second_device();
    let key = identity(env);
    other.ok_with(&[("DOTMAN_AGE_KEY", &key)], &["init", &other.remote_url()]);
    other
}

/// A repo of its own with a fresh key, for an identity no other device has.
fn other_key() -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["key", "init"]);
    env
}

#[test]
fn a_keyless_device_is_locked_and_the_key_in_the_environment_applies_it() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    let stored = ciphertext(&a);
    assert!(stored.starts_with(ARMOR));
    assert!(!a.repo().join(HOSTS).exists());
    assert!(remote_tree(&a).contains(HOSTS_AGE));
    assert!(!remote_history(&a).contains("gho_hunter2"));

    let b = a.second_device();
    let err = exits(&b.run(&["init", &b.remote_url()]), 0);
    assert!(err.contains("locked"), "{err}");
    assert!(!b.home().join(HOSTS).exists());
    assert!(!b.home().join(".config").exists(), "nothing written for it");
    assert_eq!(b.token_of("status", HOSTS).as_deref(), Some("locked"));

    let key = identity(&a);
    let vars = [("DOTMAN_AGE_KEY", key.as_str())];
    exits(&b.run_with(&vars, &["apply"]), 0);
    assert_eq!(b.read(HOSTS), PLAIN);
    assert_eq!(mode_of(&b.home().join(HOSTS)), 0o600);
    let status = b.porcelain_with(&vars, "status");
    assert!(
        status.contains(&("clean".to_owned(), HOSTS.to_owned())),
        "{status:?}"
    );
    for dir in [b.home(), b.state_dir(), b.repo()] {
        assert_eq!(files_containing(&dir, MARKER), Vec::<String>::new());
    }
}

#[test]
fn ciphertext_changes_only_when_the_plaintext_does() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    let first = ciphertext(&a);
    exits(&a.run(&["sync"]), 0);
    exits(&a.run(&["add", HOSTS]), 0);
    assert_eq!(ciphertext(&a), first);

    a.write(HOSTS, b"github.com:\n  oauth_token: gho_rotated\n", 0o600);
    assert_eq!(
        a.token_of("status", HOSTS).as_deref(),
        Some("changed-locally")
    );
    exits(&a.run(&["sync"]), 0);
    let second = ciphertext(&a);
    assert!(second.starts_with(ARMOR));
    assert_ne!(second, first);
    assert_eq!(a.token_of("status", HOSTS).as_deref(), Some("clean"));

    let b = a.second_device();
    let key = identity(&a);
    exits(
        &b.run_with(&[("DOTMAN_AGE_KEY", &key)], &["init", &b.remote_url()]),
        0,
    );
    assert_eq!(b.read(HOSTS), b"github.com:\n  oauth_token: gho_rotated\n");
}

#[test]
fn add_refuses_a_plain_age_key_and_the_identity_files() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let keys = ".config/sops/age/keys.txt";
    env.write(keys, format!("# created\n{FAKE_KEY}\n").as_bytes(), 0o600);
    for args in [&["add", keys][..], &["add", "--yes", keys]] {
        let err = env.fails(args);
        assert!(err.contains("AGE-SECRET-KEY-"), "{err}");
    }
    assert!(!env.repo().join(keys).exists());
    assert!(!env.repo().join(".config").exists());

    let own = env.home().join(".age/id.txt");
    env.write(".age/id.txt", FAKE_KEY.as_bytes(), 0o600);
    env.write_machine_config(&format!("[age]\nidentity = \"{}\"\n", own.display()));
    let err = env.fails(&["add", "--encrypt", ".age/id.txt"]);
    assert!(err.contains("never manages its own files"), "{err}");

    env.write(".keys/age.txt", FAKE_KEY.as_bytes(), 0o600);
    let file = env.home().join(".keys/age.txt");
    let vars = [("DOTMAN_AGE_KEY_FILE", file.to_str().unwrap())];
    let out = env.run_with(&vars, &["add", ".keys/age.txt"]);
    let err = exits(&out, 1);
    assert!(err.contains("DOTMAN_AGE_KEY_FILE"), "{err}");
}

#[test]
fn a_new_key_file_under_a_managed_dir_is_held_and_never_pushed() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".config/app/a.conf", b"a\n", 0o644);
    env.ok(&["add", ".config"]);
    exits(&env.run(&["sync"]), 0);

    let keys = ".config/sops/age/keys.txt";
    env.write(keys, format!("{FAKE_KEY}\n").as_bytes(), 0o600);
    assert_eq!(env.token_of("status", keys).as_deref(), Some("held"));
    let human = String::from_utf8_lossy(&env.run(&["status"]).stdout).into_owned();
    assert!(
        human.contains("keys.txt (line 1 holds an age secret key"),
        "{human}"
    );

    let err = exits(&env.run(&["sync"]), 3);
    assert!(err.contains("held back"), "{err}");
    assert!(!remote_tree(&env).contains("keys.txt"));
    assert!(!remote_history(&env).contains("AGE-SECRET-KEY-"));
    assert!(!env.repo().join(keys).exists());
}

#[test]
fn a_managed_file_edited_to_hold_a_key_is_held_on_sync() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".config/app/a.conf", b"a\n", 0o644);
    env.ok(&["add", ".config/app/a.conf"]);
    exits(&env.run(&["sync"]), 0);
    let head = remote_head(&env);

    env.write(
        ".config/app/a.conf",
        format!("a\n{FAKE_KEY}\n").as_bytes(),
        0o644,
    );
    let before = (env.four_trees(), env.remote_refs());
    let err = exits(&env.run(&["sync", "--dry-run"]), 3);
    assert!(err.contains(".config/app/a.conf: held"), "{err}");
    assert_eq!((env.four_trees(), env.remote_refs()), before);

    let err = exits(&env.run(&["sync"]), 3);
    assert!(err.contains(".config/app/a.conf: held"), "{err}");
    assert!(
        !err.contains("changed locally"),
        "the second pass leaves it alone: {err}"
    );
    assert_eq!(
        fs::read(env.repo().join(".config/app/a.conf")).unwrap(),
        b"a\n"
    );
    let repo = env.repo().to_string_lossy().into_owned();
    assert_eq!(env.git(&["-C", &repo, "status", "--porcelain"]), "");
    assert_eq!(remote_head(&env), head);
    assert!(!remote_history(&env).contains("AGE-SECRET-KEY-"));
    exits(&env.run(&["sync"]), 3);
}

#[test]
fn the_age_key_is_refused_in_the_template_allowlist() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_machine_config("[template]\nenv = [\"HOME\", \"DOTMAN_AGE_KEY\"]\n");
    let err = env.fails(&["status"]);
    assert!(
        err.contains("`[template] env` lists DOTMAN_AGE_KEY"),
        "{err}"
    );
}

#[test]
fn key_init_creates_a_private_identity_once_and_never_prints_it() {
    if !age_installed() {
        return;
    }
    let env = TestEnv::new();
    env.ok(&["init"]);
    let out = env.run(&["key", "init"]);
    let err = exits(&out, 0);
    assert!(!err.contains("AGE-SECRET-KEY-") && !out.stdout.windows(15).any(|w| w == MARKER));
    assert!(err.contains("password manager"), "{err}");
    let created = identity(&env);
    assert!(created.contains("AGE-SECRET-KEY-1"));
    assert_eq!(mode_of(&identity_path(&env)), 0o600);
    assert_eq!(mode_of(&env.xdg_config().join("dotman")), 0o700);
    let recipients = env.repo_config().age.recipients;
    assert_eq!(recipients.len(), 1);
    assert!(recipients.iter().all(|r| r.starts_with("age1")));

    let err = env.fails(&["key", "init"]);
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(identity(&env), created);
    assert_eq!(env.repo_config().age.recipients, recipients);
}

#[test]
fn key_import_replaces_a_different_identity_only_with_force_and_keeps_a_backup() {
    if !age_installed() {
        return;
    }
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["key", "init"]);
    let old = identity(&env);
    let other = other_key();
    let new_file = identity_path(&other);
    let new_arg = new_file.to_str().unwrap();

    let err = env.fails(&["key", "import", new_arg]);
    assert!(err.contains("--force"), "{err}");
    assert_eq!(identity(&env), old);
    assert_eq!(env.backups(".dotman/age-identity.txt"), Vec::new());

    exits(
        &env.run(&["key", "import", new_arg, "--force", "--add-recipient"]),
        0,
    );
    assert_eq!(identity(&env), identity(&other));
    assert_eq!(mode_of(&identity_path(&env)), 0o600);
    assert_eq!(
        env.backups(".dotman/age-identity.txt"),
        [(old.into_bytes(), 0o600)]
    );
    let runs = fs::read_dir(env.state_dir().join("backups")).unwrap();
    for run in runs {
        assert_eq!(mode_of(&run.unwrap().path()), 0o700);
    }
    assert_eq!(env.repo_config().age.recipients.len(), 2);
}

#[test]
fn key_import_reads_stdin_and_reports_a_recipient_that_is_not_listed() {
    if !age_installed() {
        return;
    }
    let env = TestEnv::new();
    env.ok(&["init"]);
    let other = other_key();
    let text = identity(&other);

    let err = exits(
        &env.run_stdin(&[], text.as_bytes(), &["key", "import", "-"]),
        3,
    );
    assert!(err.contains("is not in [age] recipients"), "{err}");
    assert!(err.contains("--add-recipient"), "{err}");
    assert_eq!(identity(&env), text);
    assert_eq!(mode_of(&identity_path(&env)), 0o600);
    assert_eq!(env.repo_config().age.recipients, Vec::<String>::new());

    let out = env.run_stdin(&[], text.as_bytes(), &["key", "import", "--add-recipient"]);
    exits(&out, 0);
    assert_eq!(
        env.repo_config().age.recipients,
        other.repo_config().age.recipients
    );

    let err = exits(&env.run_stdin(&[], b"not a key\n", &["key", "import"]), 1);
    assert!(err.contains("holds no age identity"), "{err}");
}

#[test]
fn an_imported_recipient_can_decrypt_after_a_keyed_machine_syncs() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    let b = a.second_device();
    exits(&b.run(&["init", &b.remote_url()]), 0);
    let other = other_key();
    let new_file = identity_path(&other);
    exits(
        &b.run(&[
            "key",
            "import",
            new_file.to_str().unwrap(),
            "--add-recipient",
        ]),
        0,
    );
    let err = exits(&b.run(&["sync"]), 0);
    assert!(err.contains("none was re-encrypted"), "{err}");
    assert!(!b.home().join(HOSTS).exists());

    let err = exits(&a.run(&["sync"]), 0);
    assert!(err.contains("re-encrypt"), "{err}");
    exits(&b.run(&["sync"]), 0);
    assert_eq!(b.read(HOSTS), PLAIN);
    assert_eq!(mode_of(&b.home().join(HOSTS)), 0o600);
}

#[test]
fn one_keyed_machine_re_encrypts_and_the_next_finds_nothing_to_do() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    let key = identity(&a);
    let vars = [("DOTMAN_AGE_KEY", key.as_str())];
    let b = a.second_device();
    exits(&b.run_with(&vars, &["init", &b.remote_url()]), 0);
    assert_eq!(b.read(HOSTS), PLAIN);
    let before = ciphertext(&a);

    let other = other_key();
    let config_file = a.repo().join(".dotman.toml");
    let mut config = RepoConfig::load(&config_file).unwrap();
    config
        .age
        .recipients
        .extend(other.repo_config().age.recipients);
    config.save(&config_file, false).unwrap();

    let err = exits(&a.run(&["sync"]), 0);
    assert!(
        err.contains("re-encrypt .config/gh/hosts{age}.yml"),
        "{err}"
    );
    assert_ne!(ciphertext(&a), before);
    let record = fs::read_to_string(a.repo().join(".dotman/age-recipients")).unwrap();
    assert_eq!(record.lines().count(), 2);
    let head = remote_head(&a);

    let err = exits(&b.run_with(&vars, &["sync"]), 0);
    assert!(!err.contains("re-encrypt"), "{err}");
    assert_eq!(remote_head(&a), head);
    assert_eq!(b.token_of("status", HOSTS).as_deref(), Some("locked"));
    let status = b.porcelain_with(&vars, "status");
    assert!(
        status.contains(&("clean".to_owned(), HOSTS.to_owned())),
        "{status:?}"
    );

    let c = a.second_device();
    let other_identity = identity(&other);
    let only_new = [("DOTMAN_AGE_KEY", other_identity.as_str())];
    exits(&c.run_with(&only_new, &["init", &c.remote_url()]), 0);
    assert_eq!(c.read(HOSTS), PLAIN);
}

#[test]
fn a_dry_run_reports_a_due_re_encryption_and_writes_nothing() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    let mut config = a.repo_config();
    config
        .age
        .recipients
        .extend(other_key().repo_config().age.recipients);
    config.save(&a.repo().join(".dotman.toml"), false).unwrap();
    let before = (a.four_trees(), a.remote_refs());
    let err = exits(&a.run(&["sync", "--dry-run"]), 0);
    assert!(err.contains("would re-encrypt 1 file"), "{err}");
    assert_eq!((a.four_trees(), a.remote_refs()), before);
}

#[test]
fn a_damaged_ciphertext_is_skipped_and_fails_the_run() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    fs::remove_file(a.home().join(HOSTS)).unwrap();
    a.write_repo(HOSTS_AGE, b"not an age file\n");
    a.write_repo(".plain", b"p\n");
    let err = exits(&a.run(&["apply", "--restore-deleted"]), 1);
    assert!(err.contains(".config/gh/hosts.yml: skipped:"), "{err}");
    // age says "failed to read header"; rage says "failed to fill whole buffer".
    assert!(
        err.contains("failed to read header") || err.contains("failed to fill whole buffer"),
        "{err}"
    );
    assert!(err.contains("1 path could not be applied"), "{err}");
    assert!(!err.contains("locked"), "{err}");
    assert!(!a.home().join(HOSTS).exists());
    assert_eq!(a.read(".plain"), b"p\n", "other paths still apply");
    assert_eq!(a.token_of("status", HOSTS).as_deref(), Some("unreadable"));
}

#[test]
fn a_key_file_under_a_managed_dir_is_never_scanned() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".config/app/a.conf", b"a\n", 0o644);
    env.ok(&["add", ".config"]);
    env.write(".config/keys/age.txt", FAKE_KEY.as_bytes(), 0o600);
    let file = env.home().join(".config/keys/age.txt");
    let vars = [("DOTMAN_AGE_KEY_FILE", file.to_str().unwrap())];
    let status = env.porcelain_with(&vars, "status");
    assert!(
        status.iter().all(|(_, key)| key != ".config/keys/age.txt"),
        "{status:?}"
    );
    exits(&env.run_with(&vars, &["sync"]), 0);
    assert!(!env.repo().join(".config/keys/age.txt").exists());
}

#[test]
fn without_a_remote_sync_re_encrypts_after_its_apply() {
    if !age_installed() {
        return;
    }
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["key", "init"]);
    env.write(".netrc", b"machine x\n", 0o600);
    env.ok(&["add", "--encrypt", "--for", "os!=plan9", ".netrc"]);
    let source = ".netrc{os!=plan9+age}";
    assert!(
        fs::read(env.repo().join(source))
            .unwrap()
            .starts_with(ARMOR)
    );
    exits(&env.run(&["sync"]), 0);
    let before = fs::read(env.repo().join(source)).unwrap();

    let mut config = env.repo_config();
    config
        .age
        .recipients
        .extend(other_key().repo_config().age.recipients);
    config
        .save(&env.repo().join(".dotman.toml"), false)
        .unwrap();
    let err = exits(&env.run(&["sync", "-m", "my message"]), 0);
    assert!(err.contains("re-encrypt .netrc{os!=plan9+age}"), "{err}");
    assert_ne!(fs::read(env.repo().join(source)).unwrap(), before);
    let repo = env.repo().to_string_lossy().into_owned();
    let subject = env.git(&["-C", &repo, "log", "-1", "--format=%s"]);
    assert!(subject.starts_with("sync("), "{subject}");
    assert_eq!(env.git(&["-C", &repo, "status", "--porcelain"]), "");
    let err = exits(&env.run(&["sync"]), 0);
    assert!(!err.contains("re-encrypt"), "{err}");
}

#[test]
fn a_shared_mode_is_not_recorded_for_an_encrypted_file_and_a_stricter_one_is() {
    if !age_installed() {
        return;
    }
    let (a, added) = encrypted_at(0o644);
    assert!(
        added.contains(".config/gh/hosts.yml is 0644; this encrypted file applies at 0600"),
        "{added}"
    );
    assert!(!a.repo_config().modes.contains_key(HOSTS_AGE));
    assert_eq!(
        mode_of(&a.home().join(HOSTS)),
        0o600,
        "the sync tightened it"
    );
    assert_eq!(a.token_of("status", HOSTS).as_deref(), Some("clean"));
    assert_eq!(mode_of(&keyed_clone(&a).home().join(HOSTS)), 0o600);

    let (a, added) = encrypted_at(0o400);
    assert!(!added.contains("applies at"), "{added}");
    assert_eq!(
        a.repo_config().modes.get(HOSTS_AGE).map(String::as_str),
        Some("0400")
    );
    assert_eq!(mode_of(&keyed_clone(&a).home().join(HOSTS)), 0o400);
}

#[test]
fn a_loosened_encrypted_file_is_tightened_and_its_content_still_captured() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    let before = ciphertext(&a);
    fs::set_permissions(a.home().join(HOSTS), fs::Permissions::from_mode(0o644)).unwrap();
    let err = exits(&a.run(&["sync"]), 0);
    assert!(
        err.contains("is 0644; this encrypted file applies at 0600"),
        "{err}"
    );
    assert_eq!(mode_of(&a.home().join(HOSTS)), 0o600);
    assert_eq!(ciphertext(&a), before);
    assert!(!a.repo_config().modes.contains_key(HOSTS_AGE));

    a.write(HOSTS, b"rotated\n", 0o644);
    let err = exits(&a.run(&["sync"]), 0);
    assert!(err.contains("is 0644"), "{err}");
    assert_eq!(mode_of(&a.home().join(HOSTS)), 0o600);
    assert_ne!(ciphertext(&a), before);
    assert!(!a.repo_config().modes.contains_key(HOSTS_AGE));
    let b = keyed_clone(&a);
    assert_eq!(b.read(HOSTS), b"rotated\n");
    assert_eq!(mode_of(&b.home().join(HOSTS)), 0o600);
}

#[test]
fn a_looser_mode_written_into_modes_by_hand_is_honored() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    let mut config = a.repo_config();
    config.modes.insert(HOSTS_AGE.to_owned(), "0640".to_owned());
    config.save(&a.repo().join(".dotman.toml"), false).unwrap();
    let err = exits(&a.run(&["sync"]), 0);
    assert!(!err.contains("applies at"), "{err}");
    assert_eq!(mode_of(&a.home().join(HOSTS)), 0o640);
    let err = a.ok(&["add", HOSTS]);
    assert!(!err.contains("applies at"), "{err}");
    assert_eq!(
        a.repo_config().modes.get(HOSTS_AGE).map(String::as_str),
        Some("0640")
    );
    assert_eq!(a.token_of("status", HOSTS).as_deref(), Some("clean"));
}

#[test]
fn hooks_and_the_editor_never_see_the_age_key() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let hook_env = env.root().join("hook-env");
    env.write_repo(".config/app/a.conf", b"a\n");
    env.write_repo(
        ".dotman.toml",
        format!(
            "[hooks]\n\".config/app/**\" = \"env > '{}'\"\n",
            hook_env.display()
        )
        .as_bytes(),
    );
    let vars = [("DOTMAN_AGE_KEY", FAKE_KEY)];
    exits(&env.run_with(&vars, &["apply", "--approve-hooks"]), 0);
    let seen = fs::read_to_string(&hook_env).unwrap();
    assert!(seen.contains("HOME="), "{seen}");
    assert!(!seen.contains("DOTMAN_AGE_KEY"), "{seen}");

    let editor_env = env.root().join("editor-env");
    let editor = env.write_script(
        "env-editor.sh",
        &format!("#!/bin/sh\nenv > '{}'\n", editor_env.display()),
    );
    let vars = [
        ("DOTMAN_AGE_KEY", FAKE_KEY),
        ("EDITOR", editor.to_str().unwrap()),
    ];
    exits(&env.run_with(&vars, &["edit", ".config/app/a.conf"]), 0);
    let seen = fs::read_to_string(&editor_env).unwrap();
    assert!(seen.contains("HOME="), "{seen}");
    assert!(!seen.contains("DOTMAN_AGE_KEY"), "{seen}");
}

#[test]
fn edit_of_an_encrypted_file_points_at_the_file_itself() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(HOSTS_AGE, ARMOR);
    let opened = env.root().join("opened");
    let editor = env.write_script(
        "editor.sh",
        &format!("#!/bin/sh\ntouch '{}'\n", opened.display()),
    );
    let out = env.run_with(&[("EDITOR", editor.to_str().unwrap())], &["edit", HOSTS]);
    let err = exits(&out, 1);
    assert!(
        err.contains("edit `~/.config/gh/hosts.yml` directly, then `dotman sync`"),
        "{err}"
    );
    assert!(!opened.exists());
}

#[test]
fn restore_decrypts_an_earlier_version_and_leaves_the_repo_alone() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    a.write(HOSTS, b"newer\n", 0o600);
    exits(&a.run(&["sync"]), 0);
    let source = ciphertext(&a);
    let state = fs::read(a.state_dir().join("state.toml")).unwrap();

    let before = a.four_trees();
    exits(
        &a.run(&["restore", HOSTS, "--from", "HEAD~1", "--dry-run"]),
        0,
    );
    assert_eq!(a.four_trees(), before);

    exits(&a.run(&["restore", HOSTS, "--from", "HEAD~1"]), 0);
    assert_eq!(a.read(HOSTS), PLAIN);
    assert_eq!(mode_of(&a.home().join(HOSTS)), 0o600);
    assert_eq!(a.backups(HOSTS), [(b"newer\n".to_vec(), 0o600)]);
    assert_eq!(ciphertext(&a), source);
    assert_eq!(fs::read(a.state_dir().join("state.toml")).unwrap(), state);
    let leftovers: Vec<String> = fs::read_dir(a.state_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("dotman-restore-"))
        .collect();
    assert_eq!(leftovers, Vec::<String>::new());

    let b = a.second_device();
    exits(&b.run(&["init", &b.remote_url()]), 0);
    let err = exits(&b.run(&["restore", HOSTS, "--from", "HEAD~1"]), 1);
    assert!(err.contains("dotman key import"), "{err}");
}

#[test]
fn dry_runs_of_add_encrypt_and_apply_change_nothing() {
    if !age_installed() {
        return;
    }
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["key", "init"]);
    env.write(HOSTS, PLAIN, 0o600);
    let before = env.four_trees();
    let err = env.ok(&["add", "--encrypt", "--dry-run", HOSTS]);
    assert!(
        err.contains("would add .config/gh/hosts.yml as .config/gh/hosts{age}.yml"),
        "{err}"
    );
    assert_eq!(env.four_trees(), before);

    env.ok(&["add", "--encrypt", HOSTS]);
    fs::remove_file(env.home().join(HOSTS)).unwrap();
    let before = env.four_trees();
    let err = env.ok(&["apply", "--restore-deleted", "--dry-run"]);
    assert!(err.contains("would apply .config/gh/hosts.yml"), "{err}");
    assert_eq!(env.four_trees(), before);
}

#[test]
fn encrypting_a_plain_file_replaces_its_source_and_records_only_other_modes() {
    if !age_installed() {
        return;
    }
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["key", "init"]);
    env.write(HOSTS, PLAIN, 0o600);
    env.write(".netrc", b"machine x\n", 0o400);
    env.ok(&["add", HOSTS]);
    let err = env.ok(&["add", "--encrypt", HOSTS, ".netrc"]);
    assert!(err.contains("git history still has it"), "{err}");
    assert!(!env.repo().join(HOSTS).exists());
    assert!(ciphertext(&env).starts_with(ARMOR));
    let modes = env.repo_config().modes;
    assert!(
        !modes.contains_key(HOSTS) && !modes.contains_key(HOSTS_AGE),
        "{modes:?}"
    );
    assert_eq!(modes.get(".netrc{age}").map(String::as_str), Some("0400"));
    assert_eq!(env.token_of("status", HOSTS).as_deref(), Some("clean"));
    assert_eq!(env.token_of("status", ".netrc").as_deref(), Some("clean"));
    let record = fs::read_to_string(env.repo().join(".dotman/age-recipients")).unwrap();
    assert_eq!(
        record.lines().map(str::to_owned).collect::<Vec<_>>(),
        env.repo_config().age.recipients
    );
    let listed = String::from_utf8_lossy(&env.run(&["list"]).stdout).into_owned();
    assert!(
        listed.contains("(.config/gh/hosts{age}.yml)  [age]"),
        "{listed}"
    );
}

#[test]
fn rerunning_an_interrupted_add_encrypt_removes_the_plain_source() {
    if !age_installed() {
        return;
    }
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["key", "init"]);
    env.write(HOSTS, PLAIN, 0o600);
    env.ok(&["add", "--encrypt", HOSTS]);
    let sealed = ciphertext(&env);
    // An interruption before the plain source is removed leaves both.
    env.write_repo(HOSTS, PLAIN);

    let err = env.ok(&["add", "--encrypt", "--dry-run", HOSTS]);
    assert!(
        err.contains(
            ".config/gh/hosts.yml holds .config/gh/hosts.yml in plain text and would be removed"
        ),
        "{err}"
    );
    assert!(env.repo().join(HOSTS).exists());
    let err = env.ok(&["add", "--encrypt", HOSTS]);
    assert!(err.contains("git history still has it"), "{err}");
    assert!(!env.repo().join(HOSTS).exists());
    assert_eq!(ciphertext(&env), sealed, "the ciphertext already held H");
    assert_eq!(env.token_of("status", HOSTS).as_deref(), Some("clean"));

    // When the ciphertext does not hold what $HOME has, the pair stays ambiguous.
    env.write_repo(HOSTS, PLAIN);
    env.write(HOSTS, b"github.com:\n  oauth_token: gho_new\n", 0o600);
    let err = env.fails(&["add", "--encrypt", HOSTS]);
    assert!(
        err.contains("equally specific") && err.contains("forget ~/.config/gh/hosts.yml"),
        "{err}"
    );
    assert!(env.repo().join(HOSTS).exists());
}

#[test]
fn without_an_age_cli_encrypted_paths_are_locked_and_encrypting_names_the_install() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(HOSTS_AGE, ARMOR);
    env.write_machine_config("[age]\ncommand = \"/nonexistent/age\"\n");
    assert_eq!(env.token_of("status", HOSTS).as_deref(), Some("locked"));
    let err = exits(&env.run(&["apply"]), 0);
    assert!(
        err.contains("locked") && err.contains("[age] command"),
        "{err}"
    );
    assert!(!env.home().join(HOSTS).exists());

    env.write_machine_config("");
    env.write_repo(".dotman.toml", b"[age]\nrecipients = [\"age1example\"]\n");
    // git only: every guarded command asks git about unresolved conflicts.
    let empty = env.root().join("empty-path");
    fs::create_dir(&empty).unwrap();
    let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    std::os::unix::fs::symlink(git, empty.join("git")).unwrap();
    env.write(".netrc", b"machine x\n", 0o600);
    let vars = [("PATH", empty.to_str().unwrap())];
    let err = exits(&env.run_with(&vars, &["add", "--encrypt", ".netrc"]), 1);
    // The hint follows this machine's os-release; its cases are unit-tested.
    assert!(
        err.contains("rage or age")
            && (err.contains("install one with `")
                || err.contains("github.com/FiloSottile/age/releases")),
        "{err}"
    );
    assert!(!env.repo().join(".netrc{age}").exists());
}

#[test]
fn diff_compares_the_plaintext_and_removes_its_temp_dir() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    a.write(HOSTS, b"github.com:\n  oauth_token: gho_local\n", 0o600);
    let runtime = a.root().join("runtime");
    fs::create_dir(&runtime).unwrap();
    let out = a.run_with(&[("XDG_RUNTIME_DIR", runtime.to_str().unwrap())], &["diff"]);
    exits(&out, 0);
    let shown = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(shown.contains("+  oauth_token: gho_local"), "{shown}");
    assert!(shown.contains("-  oauth_token: gho_hunter2"), "{shown}");
    assert_eq!(fs::read_dir(&runtime).unwrap().count(), 0);

    exits(&a.run(&["diff"]), 0);
    let leftovers = fs::read_dir(a.state_dir())
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.file_name().to_string_lossy().starts_with("dotman-diff-"))
        })
        .count();
    assert_eq!(leftovers, 0);

    let b = a.second_device();
    exits(&b.run(&["init", &b.remote_url()]), 0);
    let err = exits(&b.run(&["diff"]), 0);
    assert!(
        err.contains(".config/gh/hosts.yml: locked, not compared"),
        "{err}"
    );
}

// Without a TTY git starts no pager, even with `-p`, so an external diff stands in for it: a child
// of git that interrupts the whole process group while dot waits. dot runs in a group of its own,
// so the interrupt never reaches the test runner.
#[test]
fn an_interrupted_diff_leaves_no_temp_dir_behind() {
    use std::os::unix::process::CommandExt;

    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".a", b"repo\n");
    env.write(".a", b"home\n", 0o644);
    let ran = env.root().join("interrupted");
    let stub = env.write_script(
        "interrupt.sh",
        &format!("#!/bin/sh\ntouch '{}'\nkill -INT 0\n", ran.display()),
    );
    let out = env
        .dot_process()
        .env("GIT_EXTERNAL_DIFF", &stub)
        .arg("diff")
        .process_group(0)
        .output()
        .unwrap();
    assert!(ran.exists(), "the stub never ran: {out:?}");
    assert!(out.status.code().is_some(), "dot died by a signal: {out:?}");
    let left: Vec<String> = fs::read_dir(env.state_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("dotman-diff-"))
        .collect();
    assert_eq!(left, Vec::<String>::new());
}

#[test]
fn doctor_warns_on_one_recipient_counts_locked_files_and_checks_the_key_reader() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    let out = String::from_utf8_lossy(&a.run(&["doctor"]).stdout).into_owned();
    assert!(out.contains("warn  age recipients: only one"), "{out}");
    assert!(
        out.contains("pass  encrypted files: 1, none locked"),
        "{out}"
    );

    let b = a.second_device();
    exits(&b.run(&["init", &b.remote_url()]), 0);
    let out = String::from_utf8_lossy(&b.run(&["doctor"]).stdout).into_owned();
    assert!(
        out.contains("warn  encrypted files: 1 of 1 locked"),
        "{out}"
    );

    let key = identity(&a);
    let doctor = b.run_with(&[("DOTMAN_AGE_KEY", &key)], &["doctor"]);
    let out = String::from_utf8_lossy(&doctor.stdout).into_owned();
    assert!(
        out.contains("pass  DOTMAN_AGE_KEY is read by age "),
        "{out}"
    );
    assert!(
        out.contains("pass  encrypted files: 1, none locked"),
        "{out}"
    );
}

/// A second device of `a` without a key, whose `[age] command` is the real age behind a shell
/// `case` over its arguments, with the real age-keygen beside it. `None` without an age CLI.
fn wrapped_age(a: &TestEnv, arms: &str) -> Option<TestEnv> {
    let path = std::env::var_os("PATH")?;
    let find = |name: &str| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    };
    let (age, keygen) = (find("age")?, find("age-keygen")?);
    let b = a.second_device();
    exits(&b.run(&["init", &b.remote_url()]), 0);
    let dir = b.root().join("age-wrapper");
    fs::create_dir(&dir).ok()?;
    let wrapper = dir.join("age");
    let script = format!(
        "#!/bin/sh\ncase \" $* \" in\n{arms}\nesac\nexec '{}' \"$@\"\n",
        age.display()
    );
    fs::write(&wrapper, script).ok()?;
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).ok()?;
    std::os::unix::fs::symlink(keygen, dir.join("age-keygen")).ok()?;
    let config = b.xdg_config().join("dotman/config.toml");
    let text = fs::read_to_string(&config).unwrap_or_default();
    b.write_machine_config(&format!(
        "{text}\n[age]\ncommand = \"{}\"\n",
        wrapper.display()
    ));
    Some(b)
}

/// `DOTMAN_AGE_KEY` works by what the CLI does, not by what `--version` says: a build that
/// prints `(devel)` reads the key, and so does one without `-i -`, through `/dev/stdin`. The key
/// is written to no file either way.
#[test]
fn the_key_in_the_environment_is_probed_not_read_from_the_version() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    let key = identity(&a);
    let vars = [("DOTMAN_AGE_KEY", key.as_str())];
    let devel = "  *' --version '*) echo '(devel)'; exit 0;;";
    let no_dash =
        "  *' -i - '*) echo 'age: error: open -: no such file or directory' >&2; exit 1;;";
    for arms in [devel, no_dash] {
        let b = wrapped_age(&a, arms).unwrap();
        assert!(!b.home().join(HOSTS).exists(), "locked without the key");
        let err = exits(&b.run_with(&vars, &["apply"]), 0);
        assert!(!err.contains("cannot be used"), "{arms}: {err}");
        assert_eq!(b.read(HOSTS), PLAIN, "{arms}");
        for dir in [b.home(), b.state_dir(), b.repo(), b.xdg_config()] {
            assert_eq!(
                files_containing(&dir, MARKER),
                Vec::<String>::new(),
                "{arms}"
            );
        }
    }
    let b = wrapped_age(&a, devel).unwrap();
    let out = b.run_with(&vars, &["doctor"]);
    let report = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        report.contains("pass  DOTMAN_AGE_KEY is read by age (devel)"),
        "{report}"
    );
}

/// The probe decrypts a fixed file with a fixed identity, so it needs no keygen: with none beside
/// the configured age, `DOTMAN_AGE_KEY` still decrypts, doctor passes it, and no key reaches a file.
#[test]
fn the_key_in_the_environment_needs_no_keygen() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    let key = identity(&a);
    let vars = [("DOTMAN_AGE_KEY", key.as_str())];
    let b = wrapped_age(&a, "").unwrap();
    fs::remove_file(b.root().join("age-wrapper/age-keygen")).unwrap();
    let err = exits(&b.run_with(&vars, &["apply"]), 0);
    assert!(!err.contains("cannot be used"), "{err}");
    assert_eq!(b.read(HOSTS), PLAIN);
    let report = String::from_utf8_lossy(&b.run_with(&vars, &["doctor"]).stdout).into_owned();
    assert!(
        report.contains("pass  DOTMAN_AGE_KEY is read by age"),
        "{report}"
    );
    for dir in [b.home(), b.state_dir(), b.repo(), b.xdg_config()] {
        assert_eq!(files_containing(&dir, MARKER), Vec::<String>::new());
    }
}

/// rage is picked first, and when it cannot read the key from stdin, the age on PATH reads it,
/// with no rage-keygen installed.
#[test]
fn a_rage_that_cannot_read_the_key_falls_back_to_age() {
    if !age_installed() {
        return;
    }
    let Some(age) = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("age"))
        .find(|candidate| candidate.is_file())
    else {
        return;
    };
    let a = keyed_device();
    let key = identity(&a);
    let mut b = a.second_device();
    exits(&b.run(&["init", &b.remote_url()]), 0);
    let dir = b.root().join("rage-only");
    fs::create_dir(&dir).unwrap();
    let rage = dir.join("rage");
    fs::write(
        &rage,
        format!(
            "#!/bin/sh\ncase \" $* \" in\n  *' -i - '*|*' -i /dev/stdin '*) echo 'rage: no identity' >&2; exit 1;;\nesac\nexec '{}' \"$@\"\n",
            age.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&rage, fs::Permissions::from_mode(0o755)).unwrap();
    b.prepend_path(&dir);
    let err = exits(&b.run_with(&[("DOTMAN_AGE_KEY", &key)], &["apply"]), 0);
    assert!(!err.contains("cannot be used"), "{err}");
    assert_eq!(b.read(HOSTS), PLAIN);
}

/// A CLI that reads no identity from stdin at all leaves the files locked, and the warning says
/// that dotman never writes the key to a file and names `DOTMAN_AGE_KEY_FILE`.
#[test]
fn a_cli_that_cannot_read_the_key_from_stdin_says_why_and_names_the_key_file() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    let key = identity(&a);
    let arms = "  *' -i - '*|*' -i /dev/stdin '*) echo 'age: error: no identity' >&2; exit 1;;";
    let b = wrapped_age(&a, arms).unwrap();
    let err = exits(&b.run_with(&[("DOTMAN_AGE_KEY", &key)], &["apply"]), 0);
    assert!(
        err.contains("DOTMAN_AGE_KEY is set but cannot be used")
            && err.contains("dotman never writes the key to a file")
            && err.contains("DOTMAN_AGE_KEY_FILE"),
        "{err}"
    );
    assert!(err.contains("locked"), "{err}");
    assert!(!b.home().join(HOSTS).exists());
    assert!(!err.contains("AGE-SECRET-KEY-1"), "{err}");
}

/// `age --version` is bounded: a CLI that never answers it cannot hold the lock, and doctor
/// still finishes and reports the key reader.
#[test]
fn a_hung_version_probe_is_bounded() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    let key = identity(&a);
    let b = wrapped_age(&a, "  *' --version '*) exec sleep 60;;").unwrap();
    let started = std::time::Instant::now();
    let out = b.run_with(&[("DOTMAN_AGE_KEY", &key)], &["doctor"]);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "doctor waited on age --version"
    );
    let report = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        report.contains("pass  DOTMAN_AGE_KEY is read by age version unknown"),
        "{report}"
    );
    exits(&b.run_with(&[("DOTMAN_AGE_KEY", &key)], &["apply"]), 0);
    assert_eq!(b.read(HOSTS), PLAIN);
}

/// `--encrypt` resolves a content hold: the file goes in as ciphertext, sync pushes it, and no
/// plaintext copy of the credential reaches the repo or the remote.
#[test]
fn encrypt_stores_a_file_the_content_guard_holds() {
    if !age_installed() {
        return;
    }
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.ok(&["key", "init"]);
    let token = format!("ghp_{}", "a1B2".repeat(9));
    env.write(
        ".config/zsh/local.zsh",
        format!("export GH={token}\n").as_bytes(),
        0o600,
    );
    exits(&env.run(&["add", ".config/zsh/local.zsh"]), 1);
    exits(&env.run(&["add", "--encrypt", ".config/zsh/local.zsh"]), 0);
    exits(&env.run(&["sync"]), 0);
    assert!(remote_tree(&env).contains(".config/zsh/local{age}.zsh"));
    assert!(!remote_history(&env).contains(&token));
    assert_eq!(
        files_containing(&env.repo(), token.as_bytes()),
        Vec::<String>::new()
    );
}

/// What machine `box` keeps to itself; nothing the content guard holds.
const BOX_ONLY: &[u8] = b"github.com:\n  oauth_token: box-only-hunter3\n";
const BOX_VARIANT: &str = ".config/gh/hosts{host=box+age}.yml";

/// How many objects reachable in the remote hold `needle`, read one by one.
fn remote_objects_holding(env: &TestEnv, needle: &[u8]) -> usize {
    let remote = remote_path(env);
    env.git(&["--git-dir", &remote, "rev-list", "--all", "--objects"])
        .lines()
        .filter_map(|line| line.split(' ').next())
        .filter(|oid| {
            let out = env.git_raw(&["--git-dir", &remote, "cat-file", "-p", oid]);
            out.stdout
                .windows(needle.len())
                .any(|bytes| bytes == needle)
        })
        .count()
}

/// Machine A, then machine `box` holding A's key, whose encrypted `HOSTS` is both-changed: A's
/// edit reached box's repo before box's own edit was captured.
// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn encrypted_conflict() -> (TestEnv, TestEnv) {
    let a = keyed_device();
    let b = a.second_device();
    b.write_machine_config("[identity]\nhost = \"box\"\n");
    fs::copy(identity_path(&a), identity_path(&b)).expect("copy the key");
    b.ok(&["init", &b.remote_url(), "--take-repo"]);
    a.write(HOSTS, b"github.com:\n  oauth_token: from-a\n", 0o600);
    b.write(HOSTS, BOX_ONLY, 0o600);
    a.ok(&["sync"]);
    b.ok(&["git", "pull", "-q", "--rebase", "origin", "main"]);
    assert_eq!(b.token_of("status", HOSTS).as_deref(), Some("both-changed"));
    (a, b)
}

/// Typed by hand, a plain `--for` variant of an `{age}` file is refused before anything is
/// written, and the command the refusal names stores it encrypted.
#[test]
fn a_plain_variant_of_an_encrypted_file_is_refused_naming_encrypt() {
    if !age_installed() {
        return;
    }
    let (_a, b) = encrypted_conflict();
    let before = b.four_trees();
    let mut err = String::new();
    for args in [
        &["add", "--dry-run", "--for", "host=box", HOSTS][..],
        &["add", "--for", "host=box", HOSTS],
    ] {
        err = exits(&b.run(args), 1);
        assert!(
            err.contains("stores .config/gh/hosts.yml encrypted"),
            "{err}"
        );
        assert_eq!(b.four_trees(), before, "{args:?}: {err}");
    }
    exits(
        &common::run_hint(&b, &err, "add --encrypt --for host=box"),
        0,
    );
    exits(&b.run(&["sync"]), 0);
    assert!(remote_tree(&b).contains(BOX_VARIANT));
    assert_eq!(remote_objects_holding(&b, b"box-only"), 0);
}
