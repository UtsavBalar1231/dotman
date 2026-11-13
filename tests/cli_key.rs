//! `dotman key init` with an identity already in place.

mod common;

use common::{TestEnv, age_installed, mode_of};
use std::fs;
use std::path::PathBuf;

// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
/// A test machine whose `[age] command` is a stub `age` beside a stub `age-keygen`. Its `-o`
/// writes an identity-shaped file and logs the new file's mode, its directory's mode and name, and
/// whether the identity file already exists; its `-y` prints a fixed recipient for a native key.
fn stub_keygen() -> (TestEnv, PathBuf) {
    let env = TestEnv::new();
    let stubs = env.root().join("stubs");
    fs::create_dir(&stubs).expect("stub dir");
    let log = env.root().join("keygen.log");
    let identity = env.xdg_config().join("dotman/age-identity.txt");
    env.write_script("stubs/age", "#!/bin/sh\nexit 1\n");
    env.write_script(
        "stubs/age-keygen",
        &format!(
            r#"#!/bin/sh
case "$1" in
  -o)
    umask 077
    printf '# created: 2026-01-01T00:00:00Z\n# public key: age1stub\nAGE-SECRET-KEY-1STUB\n' > "$2" || exit 1
    dir=$(dirname "$2")
    if [ -e '{identity}' ]; then there=present; else there=absent; fi
    printf '%s %s %s %s\n' "$(stat -c %a "$2")" "$(stat -c %a "$dir")" "$(basename "$dir")" "$there" >> '{log}'
    ;;
  -y)
    if grep -q '^AGE-SECRET-KEY-1' ; then echo age1stubrecipient; else echo 'bad identity' >&2; exit 1; fi
    ;;
  *) exit 1 ;;
esac
"#,
            identity = identity.display(),
            log = log.display()
        ),
    );
    env.write_machine_config(&format!(
        "[age]\ncommand = \"{}\"\n",
        stubs.join("age").display()
    ));
    env.ok(&["init"]);
    (env, log)
}

/// `key init` has keygen write into a 0700 temp directory beside the identity file and renames the
/// result into place at 0600, so a kill never leaves a partial identity where the key belongs.
#[test]
fn key_init_generates_the_identity_aside_and_renames_it_into_place() {
    let (env, log) = stub_keygen();
    env.ok(&["key", "init"]);
    let logged = fs::read_to_string(&log).expect("keygen ran");
    let fields: Vec<&str> = logged.split_whitespace().collect();
    assert_eq!(fields.len(), 4, "one keygen run: {logged}");
    assert_eq!(fields.first(), Some(&"600"), "the new file: {logged}");
    assert_eq!(fields.get(1), Some(&"700"), "its directory: {logged}");
    assert!(
        fields
            .get(2)
            .is_some_and(|name| name.starts_with(".dotman-tmp-")),
        "{logged}"
    );
    assert_eq!(fields.get(3), Some(&"absent"), "{logged}");

    let dir = env.xdg_config().join("dotman");
    let identity = dir.join("age-identity.txt");
    assert_eq!(mode_of(&identity), 0o600);
    assert!(
        fs::read_to_string(&identity)
            .expect("identity")
            .contains("AGE-SECRET-KEY-1STUB")
    );
    let left: Vec<String> = fs::read_dir(&dir)
        .expect("read dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.starts_with(".dotman-tmp-"))
        .collect();
    assert_eq!(left, Vec::<String>::new());
    assert_eq!(env.repo_config().age.recipients, ["age1stubrecipient"]);
}

/// An identity file that is empty or holds only the header lines, as a keygen killed mid-write
/// leaves, is reported, backed up and replaced by `key init`, which lists the new recipient. A
/// plugin identity, whose recipient cannot be derived here, is still never replaced.
#[test]
fn key_init_backs_up_and_replaces_an_identity_file_that_holds_no_identity() {
    for old in [
        &b""[..],
        b"# created: 2026-01-01T00:00:00Z\n# public key: age1old\n",
    ] {
        let (env, _) = stub_keygen();
        let identity = env.xdg_config().join("dotman/age-identity.txt");
        env.write(&identity, old, 0o600);
        let err = env.ok(&["key", "init"]);
        assert!(
            err.contains("so it holds no usable age identity; backed up to")
                && err.contains("and a new identity replaces it"),
            "{err}"
        );
        assert!(err.contains("created an age identity"), "{err}");
        assert_eq!(
            env.backups(".dotman/age-identity.txt"),
            [(old.to_vec(), 0o600)]
        );
        assert!(
            fs::read_to_string(&identity)
                .expect("identity")
                .contains("AGE-SECRET-KEY-1STUB")
        );
        assert_eq!(mode_of(&identity), 0o600);
        assert_eq!(env.repo_config().age.recipients, ["age1stubrecipient"]);
    }

    let (env, log) = stub_keygen();
    let identity = env.xdg_config().join("dotman/age-identity.txt");
    let plugin = b"AGE-PLUGIN-YUBIKEY-1QQQQQQQQ\n";
    env.write(&identity, plugin, 0o600);
    let err = env.fails(&["key", "init"]);
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(fs::read(&identity).expect("identity"), plugin);
    assert!(!log.exists(), "no new identity was generated");
}

#[test]
fn key_init_lists_the_recipient_of_an_identity_a_killed_run_left_unlisted() {
    if !age_installed() {
        return;
    }
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["key", "init"]);
    let identity = env.xdg_config().join("dotman/age-identity.txt");
    let created = fs::read(&identity).expect("identity");
    let mut config = env.repo_config();
    let recipients = std::mem::take(&mut config.age.recipients);
    assert_eq!(recipients.len(), 1);
    config
        .save(&env.repo().join(".dotman.toml"), false)
        .expect("save .dotman.toml");

    let err = env.ok(&["key", "init"]);
    assert!(
        err.contains("added its recipient") && err.contains("password manager"),
        "{err}"
    );
    assert_eq!(env.repo_config().age.recipients, recipients);
    assert_eq!(fs::read(&identity).expect("identity"), created);

    let err = env.fails(&["key", "init"]);
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(env.repo_config().age.recipients, recipients);
}

// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
/// A desktop that cloned before the laptop made its key, and the laptop, whose `sync` has since
/// pushed its recipient and, with `second`, another one.
fn stale_desktop(second: bool) -> (TestEnv, TestEnv) {
    let laptop = TestEnv::new();
    laptop.ok(&["init", "--remote", &laptop.remote_url()]);
    laptop.write(".zshrc", b"export A=1\n", 0o644);
    laptop.ok(&["add", ".zshrc"]);
    laptop.ok(&["sync"]);
    let remote = laptop.remote_path().to_string_lossy().into_owned();
    laptop.git(&[
        "--git-dir",
        &remote,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    let desktop = laptop.second_device();
    desktop.ok(&["init", &desktop.remote_url()]);
    laptop.ok(&["key", "init"]);
    if second {
        let other = TestEnv::new();
        other.ok(&["init"]);
        other.ok(&["key", "init"]);
        let mut config = laptop.repo_config();
        config
            .age
            .recipients
            .extend(other.repo_config().age.recipients);
        config
            .save(&laptop.repo().join(".dotman.toml"), false)
            .expect("save .dotman.toml");
    }
    laptop.ok(&["sync"]);
    (laptop, desktop)
}

fn identity_file(env: &TestEnv) -> String {
    env.xdg_config()
        .join("dotman/age-identity.txt")
        .to_string_lossy()
        .into_owned()
}

#[test]
fn key_import_on_a_stale_clone_does_not_ask_when_origin_lists_the_recipient() {
    if !age_installed() {
        return;
    }
    let (laptop, desktop) = stale_desktop(false);
    assert_eq!(desktop.repo_config().age.recipients, Vec::<String>::new());

    let err = desktop.ok(&["key", "import", &identity_file(&laptop)]);
    assert!(
        err.contains("origin already lists its recipient; `dotman sync` brings the list here"),
        "{err}"
    );
    assert!(!err.contains("is not in [age] recipients"), "{err}");
    assert_eq!(desktop.repo_config().age.recipients, Vec::<String>::new());
    let repo = desktop.repo().to_string_lossy().into_owned();
    assert_eq!(desktop.git(&["-C", &repo, "status", "--porcelain"]), "");
}

#[test]
fn key_import_on_a_stale_clone_does_not_ask_when_origin_lists_the_recipient_and_the_next_sync_has_no_conflict()
 {
    if !age_installed() {
        return;
    }
    let (laptop, desktop) = stale_desktop(true);
    assert_eq!(laptop.repo_config().age.recipients.len(), 2);

    let err = desktop.ok(&["key", "import", "--add-recipient", &identity_file(&laptop)]);
    assert!(err.contains("origin already lists its recipient"), "{err}");
    assert_eq!(desktop.repo_config().age.recipients, Vec::<String>::new());

    desktop.ok(&["sync"]);
    assert_eq!(
        desktop.repo_config().age.recipients,
        laptop.repo_config().age.recipients
    );
}

/// An origin that accepts the fetch and never answers costs `key import` ten seconds, not the
/// time the origin takes.
#[test]
fn key_import_asks_when_origin_does_not_answer() {
    if !age_installed() {
        return;
    }
    let (laptop, desktop) = stale_desktop(false);
    let repo = desktop.repo().to_string_lossy().into_owned();
    let slow = desktop.write_script("slow-upload-pack", "#!/bin/sh\nexec sleep 20\n");
    desktop.git(&[
        "-C",
        &repo,
        "config",
        "remote.origin.uploadpack",
        &slow.to_string_lossy(),
    ]);

    let started = std::time::Instant::now();
    let out = desktop.run(&["key", "import", &identity_file(&laptop)]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(18),
        "{err}"
    );
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(!err.contains("origin already lists"), "{err}");
    assert!(err.contains("is not in [age] recipients"), "{err}");
}

#[test]
fn key_import_asks_when_origin_cannot_be_reached() {
    if !age_installed() {
        return;
    }
    let (laptop, desktop) = stale_desktop(false);
    let repo = desktop.repo().to_string_lossy().into_owned();
    desktop.git(&[
        "-C",
        &repo,
        "remote",
        "set-url",
        "origin",
        "file:///nonexistent/dots.git",
    ]);

    let file = identity_file(&laptop);
    let out = desktop.run(&["key", "import", &file]);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(!err.contains("origin already lists"), "{err}");
    assert!(err.contains("is not in [age] recipients"), "{err}");
    assert!(
        err.contains(&format!(
            "`dotman key import --add-recipient {file}` adds it"
        )),
        "{err}"
    );
    assert_eq!(desktop.repo_config().age.recipients, Vec::<String>::new());
}
