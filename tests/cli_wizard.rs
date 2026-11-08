//! `dotman init` flags, `gh` repo creation and the remote visibility check, through the binary. No
//! test reaches the network or the real `gh`: remotes are local, https URLs are rewritten to local
//! paths by `git config` environment variables, and `gh` is a stub.

mod common;

use common::{TestEnv, age_installed, mode_of, tree_checksum};
use dotman::config::Config;
use dotman::state::{RemotePublic, State};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

const PLAIN: &[u8] = b"github.com:\n  oauth_token: gho_hunter2\n";
const HOSTS: &str = ".config/gh/hosts.yml";
const HOSTS_AGE: &str = ".config/gh/hosts{age}.yml";
const SSH_URL: &str = "git@github.com:me/dotfiles.git";

fn exits(out: &Output, code: i32) -> String {
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(code), "{err}");
    err
}

/// A `gh` that logs its arguments to the returned file. `auth status` exits `auth`.
// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn install_gh(env: &mut TestEnv, auth: i32) -> PathBuf {
    let dir = env.root().join("gh-stub");
    fs::create_dir_all(&dir).expect("create the stub dir");
    let log = env.root().join("gh.log");
    let body = format!(
        "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$1 $2\" in\n  'auth status') exit {auth};;\n  'repo create') echo \"https://github.com/$3\";;\n  'repo view') echo '{{\"sshUrl\":\"{SSH_URL}\"}}';;\n  'api user') echo me;;\n  *) exit 99;;\nesac\n",
        log.display()
    );
    let gh = dir.join("gh");
    fs::write(&gh, body).expect("write the stub");
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("chmod the stub");
    env.prepend_path(&dir);
    log
}

fn gh_calls(log: &Path) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn machine_config(env: &TestEnv) -> Config {
    Config::load(&env.xdg_config().join("dotman/config.toml")).expect("load the machine config")
}

// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn state(env: &TestEnv) -> State {
    State::read(&env.state_dir().join("state.toml"))
        .expect("read state.toml")
        .expect("state.toml exists")
}

fn identity_path(env: &TestEnv) -> PathBuf {
    env.xdg_config().join("dotman/age-identity.txt")
}

fn assert_no_local_identity(env: &TestEnv) {
    let repo = env.repo().to_string_lossy().into_owned();
    let out = env.git_raw(&["-C", &repo, "config", "--local", "--get-regexp", "^user[.]"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

fn repo_git(env: &TestEnv, args: &[&str]) -> String {
    let repo = env.repo().to_string_lossy().into_owned();
    let mut full = vec!["-C", repo.as_str()];
    full.extend_from_slice(args);
    env.git(&full)
}

/// Environment that makes `https://example.org/me/dots.git` answer from the local bare remote,
/// and every other https URL fail.
fn public_remote_vars(env: &TestEnv) -> Vec<(String, String)> {
    let local = env.remote_path().to_string_lossy().into_owned();
    [
        ("GIT_CONFIG_COUNT", "3".to_owned()),
        (
            "GIT_CONFIG_KEY_0",
            "url.file:///dotman-tests-no-network/.insteadOf".to_owned(),
        ),
        ("GIT_CONFIG_VALUE_0", "https://".to_owned()),
        ("GIT_CONFIG_KEY_1", format!("url.{local}.insteadOf")),
        (
            "GIT_CONFIG_VALUE_1",
            "https://example.org/me/dots.git".to_owned(),
        ),
        (
            "GIT_CONFIG_KEY_2",
            "url.file:///dotman-tests-no-network/.insteadOf".to_owned(),
        ),
        ("GIT_CONFIG_VALUE_2", "http://".to_owned()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}

fn run_public(env: &TestEnv, args: &[&str]) -> Output {
    let vars = public_remote_vars(env);
    let pairs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    env.run_with(&pairs, args)
}

#[test]
fn a_flag_for_every_question_is_recorded_without_a_prompt() {
    let mut env = TestEnv::new();
    env.use_system_root();
    env.clear_git_identity();
    let key = age_installed();
    let key_flag = if key { "--create-key" } else { "--no-key" };
    let remote = env.remote_url();
    let err = env.ok(&[
        "init",
        "--git-name",
        "Ann Lee",
        "--git-email",
        "ann@example.org",
        "--remote",
        &remote,
        "--host",
        "laptop",
        "--profile",
        "work",
        "--profile",
        "home",
        "--system-root",
        "/etc/keyd",
        key_flag,
    ]);

    assert_eq!(
        repo_git(&env, &["config", "--local", "user.name"]).trim(),
        "Ann Lee"
    );
    assert_eq!(
        repo_git(&env, &["config", "--local", "user.email"]).trim(),
        "ann@example.org"
    );
    let global = fs::read_to_string(env.home().join(".gitconfig")).expect("read .gitconfig");
    assert!(!global.contains("Ann"), "{global}");
    assert_eq!(
        repo_git(&env, &["remote", "get-url", "origin"]).trim(),
        remote
    );

    let machine = machine_config(&env);
    assert_eq!(machine.identity.host.as_deref(), Some("laptop"));
    assert_eq!(machine.profiles, ["work", "home"]);
    assert_eq!(machine.system.roots, [PathBuf::from("/etc/keyd")]);
    let mode = mode_of(&env.xdg_config().join("dotman/config.toml"));
    assert_eq!(mode, 0o600);

    if key {
        let identity = identity_path(&env);
        assert!(
            fs::read_to_string(&identity)
                .expect("the identity")
                .contains("AGE-SECRET-KEY-")
        );
        assert_eq!(mode_of(&identity), 0o600);
        assert!(
            !env.repo().join(".dotman.toml").exists()
                || !env.repo_config().age.recipients.is_empty()
        );
        assert!(!err.contains("AGE-SECRET-KEY-"), "{err}");
    } else {
        assert!(!identity_path(&env).exists());
    }
}

#[test]
fn the_global_flag_writes_the_global_git_config() {
    let env = TestEnv::new();
    env.clear_git_identity();
    env.ok(&[
        "init",
        "--git-name",
        "Ann Lee",
        "--git-email",
        "ann@example.org",
        "--git-global",
    ]);
    let global = fs::read_to_string(env.home().join(".gitconfig")).expect("read .gitconfig");
    assert!(
        global.contains("Ann Lee") && global.contains("ann@example.org"),
        "{global}"
    );
    assert_no_local_identity(&env);
}

#[test]
fn an_existing_identity_is_left_alone() {
    let env = TestEnv::new();
    let err = env.ok(&[
        "init",
        "--git-name",
        "Other",
        "--git-email",
        "o@example.org",
    ]);
    assert!(err.contains("already has an identity"), "{err}");
    assert_no_local_identity(&env);
}

#[test]
fn a_run_with_no_flags_creates_the_repo_without_a_remote() {
    let env = TestEnv::new();
    let out = env.run(&["init"]);
    exits(&out, 0);
    assert!(env.repo().join(".git").exists());
    assert_eq!(repo_git(&env, &["remote"]).trim(), "");
    assert!(!env.xdg_config().join("dotman").exists());
    assert!(!identity_path(&env).exists());
}

#[test]
fn gh_creates_a_private_repo_and_origin_is_its_ssh_url() {
    let mut env = TestEnv::new();
    let log = install_gh(&mut env, 0);
    let err = env.ok(&["init", "--create-github", "me/dotfiles"]);
    assert_eq!(
        gh_calls(&log),
        [
            "auth status -h github.com",
            "repo create me/dotfiles --private",
            "repo view me/dotfiles --json sshUrl"
        ]
    );
    assert_eq!(
        repo_git(&env, &["remote", "get-url", "origin"]).trim(),
        SSH_URL
    );
    assert!(err.contains("me/dotfiles"), "{err}");
    let state = state(&env);
    assert_eq!(state.remote_url.as_deref(), Some(SSH_URL));
    assert_eq!(state.remote_public, Some(RemotePublic::Unknown));
}

#[test]
fn gh_that_is_not_logged_in_stops_init_with_the_login_hint() {
    let mut env = TestEnv::new();
    let log = install_gh(&mut env, 1);
    let err = exits(&env.run(&["init", "--create-github", "me/dotfiles"]), 1);
    assert!(err.contains("gh auth login"), "{err}");
    assert_eq!(gh_calls(&log), ["auth status -h github.com"]);
    assert!(!env.repo().exists(), "nothing was created");
}

#[test]
fn the_real_gh_is_never_reached_by_a_test() {
    let env = TestEnv::new();
    let err = exits(&env.run(&["init", "--create-github", "me/dotfiles"]), 1);
    assert!(err.contains("gh auth login"), "{err}");
    assert!(!env.repo().exists());
}

#[test]
fn flags_that_name_two_remotes_are_refused() {
    let env = TestEnv::new();
    for args in [
        &["init", "--remote", "x", "--no-remote"][..],
        &["init", "--remote", "x", "--create-github", "me/x"],
        &["init", "--no-remote", "--create-github", "me/x"],
        &["init", "--create-key", "--no-key"],
        &["init", "--no-system", "--system-root", "/etc"],
    ] {
        assert_eq!(env.run(args).status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn a_flag_that_cannot_be_used_is_refused_before_anything_is_created() {
    let mut env = TestEnv::new();
    env.use_system_root();
    for args in [
        &["init", "--host", "a b"][..],
        &["init", "--profile", "ok", "--profile", "no+pe"],
        &["init", "--system-root", "/proc/x"],
        &["init", "--system-root", "etc"],
        &["init", "--create-github", "not-a-slug"],
        &["init", "--git-email", "nobody"],
    ] {
        exits(&env.run(args), 1);
        assert!(!env.repo().exists(), "{args:?}");
    }
    assert!(!env.xdg_config().join("dotman").exists());
}

#[test]
fn a_file_remote_is_unknown_and_never_warns() {
    let env = TestEnv::new();
    let url = env.remote_url();
    let err = env.ok(&["init", "--remote", &url]);
    assert!(!err.contains("public"), "{err}");
    let state = state(&env);
    assert_eq!(state.remote_public, Some(RemotePublic::Unknown));
    assert_eq!(state.remote_url.as_deref(), Some(url.as_str()));
    assert!(state.remote_checked_at.is_some());
}

#[test]
fn a_public_remote_warns_and_turns_on_the_secret_name_guard() {
    let env = TestEnv::new();
    let out = run_public(&env, &["init", "--remote", "git@example.org:me/dots.git"]);
    let err = exits(&out, 0);
    assert!(err.contains("is public"), "{err}");
    assert!(
        err.contains("--encrypt") && err.contains("--allow-plain"),
        "{err}"
    );
    let recorded = state(&env);
    assert_eq!(recorded.remote_public, Some(RemotePublic::True));
    assert_eq!(
        recorded.remote_url.as_deref(),
        Some("git@example.org:me/dots.git")
    );

    env.write(".env", b"TOKEN=1\n", 0o600);
    env.write(".bashrc", b"# rc\n", 0o644);
    let err = env.fails(&["add", ".env", ".bashrc"]);
    assert!(err.contains(".env"), "{err}");
    assert!(
        err.contains("--encrypt") && err.contains("--allow-plain"),
        "{err}"
    );
    assert!(!env.repo().join(".env").exists());
    assert!(
        !env.repo().join(".bashrc").exists(),
        "nothing is added after a refusal"
    );

    env.write(".config/x/.env", b"TOKEN=2\n", 0o600);
    env.write(".config/x/rc", b"x\n", 0o644);
    let out = env.run(&["add", "--yes", ".config/x"]);
    let err = exits(&out, 3);
    assert!(err.contains("held .config/x/.env"), "{err}");
    assert!(env.repo().join(".config/x/rc").exists());
    assert!(!env.repo().join(".config/x/.env").exists());

    env.ok(&["add", "--allow-plain", ".env"]);
    assert_eq!(
        fs::read(env.repo().join(".env")).expect("added"),
        b"TOKEN=1\n"
    );
}

#[test]
fn a_private_looking_remote_keeps_the_guard_off() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", "git@example.org:me/dots.git"]);
    assert_eq!(state(&env).remote_public, Some(RemotePublic::Unknown));
    env.write(".env", b"TOKEN=1\n", 0o600);
    env.ok(&["add", ".env"]);
}

#[test]
fn add_checks_a_remote_that_has_no_stored_result() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.git(&[
        "-C",
        &env.repo().to_string_lossy(),
        "remote",
        "add",
        "origin",
        "git@example.org:me/dots.git",
    ]);
    env.write(".env", b"TOKEN=1\n", 0o600);
    let out = run_public(&env, &["add", ".env"]);
    let err = exits(&out, 1);
    assert!(err.contains("--allow-plain"), "{err}");
    assert_eq!(state(&env).remote_public, Some(RemotePublic::True));
}

#[test]
fn encrypting_is_allowed_with_a_public_remote() {
    if !age_installed() {
        return;
    }
    let env = TestEnv::new();
    exits(
        &run_public(&env, &["init", "--remote", "git@example.org:me/dots.git"]),
        0,
    );
    env.ok(&["key", "init"]);
    env.write(HOSTS, PLAIN, 0o600);
    env.ok(&["add", "--encrypt", HOSTS]);
    assert!(env.repo().join(HOSTS_AGE).exists());
}

#[test]
fn a_dry_run_changes_nothing() {
    let mut env = TestEnv::new();
    env.use_system_root();
    env.clear_git_identity();
    let log = install_gh(&mut env, 0);
    let xdg_config = env.xdg_config();
    let before = (env.four_trees(), tree_checksum(&xdg_config));
    let out = env.run(&[
        "init",
        "--dry-run",
        "--create-github",
        "me/x",
        "--create-key",
        "--git-name",
        "n",
        "--git-email",
        "e@example.org",
        "--git-global",
        "--host",
        "h",
        "--profile",
        "p",
        "--system-root",
        "/etc/keyd",
    ]);
    let err = exits(&out, 0);
    assert!(
        err.contains("would create the private GitHub repo me/x"),
        "{err}"
    );
    assert!(
        !gh_calls(&log)
            .iter()
            .any(|call| call.starts_with("repo create")),
        "{:?}",
        gh_calls(&log)
    );
    assert!(!env.repo().exists());
    assert!(!identity_path(&env).exists());
    assert!(!env.repo().join("install.sh").exists());
    assert_eq!((env.four_trees(), tree_checksum(&xdg_config)), before);
    let global = fs::read_to_string(env.home().join(".gitconfig")).expect("read .gitconfig");
    assert!(!global.contains("user"), "{global}");
}

/// A device with a key and `.config/gh/hosts.yml` added encrypted, synced to the remote.
fn keyed_device() -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.ok(&["key", "init"]);
    env.write(HOSTS, PLAIN, 0o600);
    env.ok(&["add", "--encrypt", HOSTS]);
    env.ok(&["sync"]);
    env.git(&[
        "--git-dir",
        &env.remote_path().to_string_lossy(),
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    env
}

// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn identity_text(env: &TestEnv) -> String {
    fs::read_to_string(identity_path(env)).expect("the identity")
}

/// The identity of `env` in a file outside HOME, as a user would keep a backup.
// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn key_file_of(env: &TestEnv, holder: &TestEnv) -> PathBuf {
    let path = holder.root().join("backup-key.txt");
    fs::write(&path, identity_text(env)).expect("write the key file");
    path
}

#[test]
fn an_encrypted_repo_is_locked_without_a_key_and_decrypts_with_the_key_file() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();

    let b = a.second_device();
    let err = exits(&b.run(&["init", &b.remote_url()]), 0);
    assert!(err.contains("locked"), "{err}");
    assert!(!b.home().join(HOSTS).exists());
    assert!(!identity_path(&b).exists());

    let c = a.second_device();
    let file = key_file_of(&a, &c);
    let file = file.to_string_lossy().into_owned();
    let err = exits(
        &c.run(&["init", &c.remote_url(), "--age-key-file", &file]),
        0,
    );
    assert!(!err.contains("locked"), "{err}");
    assert_eq!(c.read(HOSTS), PLAIN);
    assert_eq!(identity_text(&c), identity_text(&a));
    assert_eq!(mode_of(&identity_path(&c)), 0o600);
}

#[test]
fn a_different_key_file_is_refused_without_force_and_nothing_is_cloned() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    let other = TestEnv::new();
    other.ok(&["init"]);
    other.ok(&["key", "init"]);

    let c = a.second_device();
    fs::create_dir_all(identity_path(&c).parent().expect("a parent")).expect("create dir");
    fs::write(identity_path(&c), identity_text(&other)).expect("write the identity");
    let file = key_file_of(&a, &c).to_string_lossy().into_owned();

    let err = exits(
        &c.run(&["init", &c.remote_url(), "--age-key-file", &file]),
        1,
    );
    assert!(err.contains("--force"), "{err}");
    assert!(!c.repo().exists(), "the refusal comes before the clone");
    assert_eq!(identity_text(&c), identity_text(&other));

    exits(
        &c.run(&["init", &c.remote_url(), "--age-key-file", &file, "--force"]),
        0,
    );
    assert_eq!(identity_text(&c), identity_text(&a));
    assert_eq!(c.read(HOSTS), PLAIN);
    assert_eq!(c.backups(".dotman/age-identity.txt").len(), 1);
}

#[test]
fn a_clone_gets_the_machine_answers_before_it_is_applied() {
    let a = TestEnv::new();
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write(".bashrc", b"base\n", 0o644);
    a.ok(&["add", ".bashrc"]);
    a.write_repo(".bashrc{host=box}", b"box\n");
    a.push_repo();

    let mut b = a.second_device();
    b.use_system_root();
    b.clear_git_identity();
    b.ok(&[
        "init",
        &b.remote_url(),
        "--host",
        "box",
        "--profile",
        "work",
        "--system-root",
        "/etc/keyd",
        "--git-name",
        "Ann",
        "--git-email",
        "ann@example.org",
    ]);
    assert_eq!(
        b.read(".bashrc"),
        b"box\n",
        "the host answer applies to this very apply"
    );
    let machine = machine_config(&b);
    assert_eq!(machine.profiles, ["work"]);
    assert_eq!(machine.system.roots, [PathBuf::from("/etc/keyd")]);
    assert_eq!(
        repo_git(&b, &["config", "--local", "user.name"]).trim(),
        "Ann"
    );
    assert_eq!(
        state(&b).remote_url.as_deref(),
        Some(b.remote_url().as_str())
    );
}

#[test]
fn a_dry_run_clone_writes_no_answer() {
    let a = TestEnv::new();
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write(".bashrc", b"base\n", 0o644);
    a.ok(&["add", ".bashrc"]);
    a.push_repo();

    let mut b = a.second_device();
    b.use_system_root();
    b.clear_git_identity();
    let xdg_config = b.xdg_config();
    let before = (b.four_trees(), tree_checksum(&xdg_config));
    let err = exits(
        &b.run(&[
            "init",
            &b.remote_url(),
            "--dry-run",
            "--host",
            "box",
            "--profile",
            "work",
            "--system-root",
            "/etc/keyd",
            "--git-name",
            "Ann",
            "--git-email",
            "ann@example.org",
        ]),
        0,
    );
    assert!(err.contains("would write"), "{err}");
    assert!(!b.repo().exists());
    assert_eq!((b.four_trees(), tree_checksum(&xdg_config)), before);
}

#[test]
fn an_import_records_the_remote_it_brought() {
    let env = TestEnv::new();
    let work = env.root().join("work");
    fs::create_dir_all(&work).expect("create the work tree");
    let work_arg = work.to_string_lossy().into_owned();
    env.git(&["-C", &work_arg, "init", "-q", "-b", "main"]);
    fs::write(work.join(".vimrc"), "v\n").expect("write a file");
    env.git(&["-C", &work_arg, "add", "-A"]);
    env.git(&["-C", &work_arg, "commit", "-q", "-m", "first"]);
    let bare = env.root().join("src.git");
    env.git(&["clone", "-q", "--bare", &work_arg, &bare.to_string_lossy()]);
    let url = format!("file://{}", bare.display());

    let err = env.ok(&["import", &url]);
    assert!(!err.contains("is public"), "{err}");
    let recorded = state(&env);
    assert_eq!(recorded.remote_url.as_deref(), Some(url.as_str()));
    assert_eq!(recorded.remote_public, Some(RemotePublic::Unknown));
    assert!(recorded.remote_checked_at.is_some());
}

#[test]
fn sync_rechecks_a_remote_that_changed() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    let first = state(&env);
    assert_eq!(first.remote_public, Some(RemotePublic::Unknown));

    env.git(&[
        "-C",
        &env.repo().to_string_lossy(),
        "remote",
        "set-url",
        "origin",
        "https://example.org/me/dots.git",
    ]);
    exits(&run_public(&env, &["sync", "--no-hooks"]), 0);
    let after = state(&env);
    assert_eq!(
        after.remote_url.as_deref(),
        Some("https://example.org/me/dots.git")
    );
    assert_eq!(after.remote_public, Some(RemotePublic::True));
}
