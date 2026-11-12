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
    let parameters = format!(
        "'url.file:///dotman-tests-no-network/.insteadOf=https://' \
         'url.{local}.insteadOf=https://example.org/me/dots.git' \
         'url.file:///dotman-tests-no-network/.insteadOf=http://'"
    );
    vec![("GIT_CONFIG_PARAMETERS".to_owned(), parameters)]
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

/// The calls that need no repo, writing the global git config and asking whether the remote
/// answers without credentials, work from a working directory that holds a `.git` gitfile whose
/// target is gone, as a removed submodule leaves.
#[test]
fn a_broken_gitfile_in_the_working_directory_does_not_stop_the_repoless_calls() {
    let env = TestEnv::new();
    env.clear_git_identity();
    let stray = env.root().join("stray");
    fs::create_dir_all(&stray).expect("mkdir");
    fs::write(stray.join(".git"), "gitdir: /nonexistent/.git/modules/x\n").expect("a gitfile");
    let out = env
        .dot()
        .current_dir(&stray)
        .args([
            "init",
            "--git-name",
            "Ann Lee",
            "--git-email",
            "ann@example.org",
            "--git-global",
        ])
        .output()
        .expect("run dotman");
    exits(&out, 0);
    let global = fs::read_to_string(env.home().join(".gitconfig")).expect("read .gitconfig");
    assert!(
        global.contains("Ann Lee") && global.contains("ann@example.org"),
        "{global}"
    );

    let out = env
        .dot()
        .current_dir(&stray)
        .envs(public_remote_vars(&env))
        .args(["init", "--remote", "git@example.org:me/dots.git"])
        .output()
        .expect("run dotman");
    let err = exits(&out, 0);
    assert!(err.contains("is public"), "{err}");
    assert_eq!(state(&env).remote_public, Some(RemotePublic::True));
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
        &["init", "--profile", "y"],
        &["init", "--profile", "work", "--profile", "No"],
    ] {
        exits(&env.run(args), 1);
        assert!(!env.repo().exists(), "{args:?}");
    }
    assert!(!env.xdg_config().join("dotman").exists());
    let err = env.fails(&["init", "--profile", "yes"]);
    assert!(err.contains("`yes` answers a yes/no question"), "{err}");
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

/// A `git` on PATH that SIGKILLs the dotman calling it once the repo path holds a `.git`: a kill
/// just after `init <url>` moved its clone into place. Returns the stub, to remove afterwards.
// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn kill_once_cloned(env: &mut TestEnv) -> PathBuf {
    let real = std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH");
    let dir = env.root().join("kill-stub");
    fs::create_dir_all(&dir).expect("create the stub dir");
    let stub = dir.join("git");
    fs::write(
        &stub,
        format!(
            "#!/bin/sh\n[ -d '{}/.git' ] && {{ kill -KILL \"$PPID\"; exit 137; }}\nexec '{}' \"$@\"\n",
            env.repo().display(),
            real.display()
        ),
    )
    .expect("write the stub");
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).expect("chmod the stub");
    env.prepend_path(&dir);
    stub
}

#[test]
fn a_rerun_of_an_init_killed_after_its_clone_finishes_the_machine_setup() {
    if !age_installed() {
        return;
    }
    let a = keyed_device();
    let mut c = a.second_device();
    let file = key_file_of(&a, &c).to_string_lossy().into_owned();
    let url = c.remote_url();
    let args = ["init", &url, "--host", "box", "--age-key-file", &file];
    let stub = kill_once_cloned(&mut c);
    let out = c.run(&args);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), None, "{err}");
    fs::remove_file(stub).expect("remove the stub");
    assert!(c.repo().join(".git/dotman-id").is_file());
    assert!(!c.xdg_config().join("dotman/config.toml").exists());
    assert!(!identity_path(&c).exists());

    let before = (c.four_trees(), tree_checksum(&c.xdg_config()));
    let dry = [&args[..], &["--dry-run"]].concat();
    let err = exits(&c.run(&dry), 0);
    assert!(err.contains("would write"), "{err}");
    assert!(err.contains("would import the age identity"), "{err}");
    assert_eq!((c.four_trees(), tree_checksum(&c.xdg_config())), before);

    let err = exits(&c.run(&args), 0);
    assert!(!err.contains("locked"), "{err}");
    assert_eq!(machine_config(&c).identity.host.as_deref(), Some("box"));
    assert_eq!(identity_text(&c), identity_text(&a));
    assert_eq!(c.read(HOSTS), PLAIN);
}

#[test]
fn init_over_a_killed_clone_refuses_another_url_and_a_repo_init_did_not_make() {
    let a = TestEnv::new();
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write(".bashrc", b"base\n", 0o644);
    a.ok(&["add", ".bashrc"]);
    a.push_repo();

    let mut b = a.second_device();
    let url = b.remote_url();
    let stub = kill_once_cloned(&mut b);
    assert_eq!(b.run(&["init", &url, "--host", "box"]).status.code(), None);
    fs::remove_file(stub).expect("remove the stub");

    let elsewhere = format!("file://{}", b.root().join("elsewhere.git").display());
    let err = exits(&b.run(&["init", &elsewhere, "--host", "box"]), 1);
    assert!(err.contains("a repo already exists"), "{err}");
    fs::remove_file(b.repo().join(".git/dotman-id")).expect("remove dotman-id");
    let err = exits(&b.run(&["init", &url, "--host", "box"]), 1);
    assert!(err.contains("a repo already exists"), "{err}");
    assert!(!b.xdg_config().join("dotman/config.toml").exists());
    assert!(!b.home().join(".bashrc").exists());
}

/// A repo with `.bashrc` pushed to the remote.
fn published() -> TestEnv {
    let a = TestEnv::new();
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write(".bashrc", b"base\n", 0o644);
    a.ok(&["add", ".bashrc"]);
    a.push_repo();
    a
}

/// git resolves a relative remote against the repo, so `--remote` stores a local path made absolute
/// against the directory it was typed in; an ssh form, with a colon before any `/`, stays as typed.
#[test]
fn a_relative_remote_is_stored_absolute_and_reaches_the_remote() {
    let env = TestEnv::new();
    let remote = fs::canonicalize(env.remote_path()).expect("canonical remote");
    let up: PathBuf = env.home().components().skip(1).map(|_| "..").collect();
    let relative = up.join(remote.strip_prefix("/").expect("an absolute remote"));
    let relative = relative.to_string_lossy().into_owned();
    let stored = remote.to_string_lossy().into_owned();
    env.ok(&["init", "--no-import", "--remote", &relative]);
    assert_eq!(
        repo_git(&env, &["config", "remote.origin.url"]).trim(),
        stored
    );
    repo_git(&env, &["ls-remote", "origin"]);
    let again = env.ok(&["init", "--remote", &relative]);
    assert!(
        again.contains(&format!("origin is already {stored}")),
        "{again}"
    );

    let home = fs::canonicalize(env.home()).expect("canonical home");
    let dry = env.ok(&["init", "--remote", "elsewhere.git", "--dry-run"]);
    let would = format!(
        "would set origin to {}",
        home.join("elsewhere.git").display()
    );
    assert!(dry.contains(&would), "{dry}");
    for url in ["gh:me/dotfiles.git", SSH_URL] {
        env.ok(&["init", "--remote", url]);
        assert_eq!(repo_git(&env, &["config", "remote.origin.url"]).trim(), url);
    }
}

/// git expands a leading `~/` in a local path against the home of whoever runs it, so `--remote`,
/// a clone of a source with such an origin and the import's hint for one keep it as typed, and
/// git then reaches `$HOME/dots.git`.
#[test]
fn a_remote_starting_with_a_tilde_is_kept_as_typed() {
    let env = TestEnv::new();
    let tilde = "~/dots.git";
    let dots = env.home().join("dots.git");
    env.git(&["init", "-q", "--bare", &dots.to_string_lossy()]);
    env.ok(&["init", "--no-import", "--remote", tilde]);
    assert_eq!(
        repo_git(&env, &["config", "remote.origin.url"]).trim(),
        tilde
    );
    let reached = env.git_raw(&["-C", &env.repo().to_string_lossy(), "ls-remote", "origin"]);
    exits(&reached, 0);

    let b = TestEnv::new();
    let work = common::work_tree(&b, "src", &[(".bashrc", "# rc\n", false)], None);
    common::git_in(&b, &work, &["config", "remote.origin.url", tilde]);
    b.ok(&["init", &work.to_string_lossy()]);
    assert_eq!(repo_git(&b, &["config", "remote.origin.url"]).trim(), tilde);

    let c = TestEnv::new();
    let err = c.ok(&["import", &work.to_string_lossy(), "--no-bootstrap"]);
    common::check_origin_hint(&c, &err, tilde);
}

/// Only a relative local path needs the working directory, so `--remote` with a URL works from one
/// that was removed.
#[test]
fn a_url_remote_works_from_a_removed_working_directory() {
    let env = TestEnv::new();
    let gone = env.root().join("gone");
    fs::create_dir(&gone).expect("create the working directory");
    let script = format!(
        "cd {} && rmdir {} && exec \"$0\" init --no-import --no-key --remote {}",
        shell_words::quote(&gone.to_string_lossy()),
        shell_words::quote(&gone.to_string_lossy()),
        shell_words::quote(&env.remote_url())
    );
    let out = env
        .process_for(Path::new("sh"))
        .args(["-c", &script, env!("CARGO_BIN_EXE_dotman")])
        .output()
        .expect("run sh");
    exits(&out, 0);
    assert_eq!(
        repo_git(&env, &["config", "remote.origin.url"]).trim(),
        env.remote_url()
    );
}

/// git stores a relative source made absolute, so the rerun of the same command line compares the
/// directories, not the text.
#[test]
fn a_rerun_with_the_same_relative_source_finishes_the_machine_setup() {
    let b = published().second_device();
    let up: PathBuf = b.home().components().skip(1).map(|_| "..").collect();
    let remote = b.remote_path();
    let relative = up.join(remote.strip_prefix("/").expect("an absolute remote"));
    let relative = relative.to_string_lossy().into_owned();
    b.ok(&["init", &relative]);

    let args = ["init", &relative, "--host", "box"];
    let before = (b.four_trees(), tree_checksum(&b.xdg_config()));
    exits(&b.run(&[&args[..], &["--dry-run"]].concat()), 0);
    assert_eq!((b.four_trees(), tree_checksum(&b.xdg_config())), before);
    let err = exits(&b.run(&args), 0);
    assert!(err.contains("already holds the clone"), "{err}");
    assert_eq!(machine_config(&b).identity.host.as_deref(), Some("box"));

    // The stored form still matches when the source cannot be resolved.
    fs::rename(&remote, remote.with_extension("moved")).expect("move the remote away");
    exits(&b.run(&["init", &relative, "--host", "den"]), 0);
    assert_eq!(machine_config(&b).identity.host.as_deref(), Some("den"));
}

#[test]
fn a_rerun_refuses_a_source_that_only_resembles_the_origin() {
    let b = published().second_device();
    let url = b.remote_url();
    let path = b.remote_path().to_string_lossy().into_owned();
    b.ok(&["init", &url]);
    let other = b.root().to_string_lossy().into_owned();
    let refused = |origin: &str, sources: &[&str]| {
        repo_git(&b, &["remote", "set-url", "origin", origin]);
        for source in sources {
            let err = exits(&b.run(&["init", source, "--host", "box"]), 1);
            assert!(err.contains("a repo already exists"), "{source}: {err}");
        }
    };
    refused(&url, &[&path, &format!("{url}/")]);
    refused(&path, &[&url, &other]);
    assert_ne!(machine_config(&b).identity.host.as_deref(), Some("box"));
}

/// An origin stored relative, by hand or by an older dotman, names what git reads from the repo,
/// and the source what it names from the working directory: the same text typed where it names
/// another repo is refused, and any spelling of the origin's directory is accepted.
#[test]
fn a_rerun_compares_a_relative_origin_by_the_directory_it_names() {
    let b = published().second_device();
    b.ok(&["init", &b.remote_url()]);
    // Three levels up from the repo, xdg/data/dotman, is the test root.
    let root = b.root();
    std::os::unix::fs::symlink(b.remote_path(), root.join("a.git")).expect("link to A");
    repo_git(&b, &["remote", "set-url", "origin", "../../../a.git"]);
    let elsewhere = root.join("z/p/q/r");
    fs::create_dir_all(&elsewhere).expect("create the other cwd");
    b.git(&[
        "init",
        "-q",
        "--bare",
        &root.join("z/a.git").to_string_lossy(),
    ]);
    let rerun = |cwd: &Path, source: &str| {
        b.dot()
            .current_dir(cwd)
            .args(["init", source, "--host", "box"])
            .output()
            .expect("run dot")
    };

    let err = exits(&rerun(&elsewhere, "../../../a.git"), 1);
    assert!(err.contains("a repo already exists"), "{err}");
    assert_ne!(machine_config(&b).identity.host.as_deref(), Some("box"));

    let err = exits(&rerun(&b.home(), "../a.git"), 0);
    assert!(err.contains("already holds the clone"), "{err}");
    assert_eq!(machine_config(&b).identity.host.as_deref(), Some("box"));

    // git reads a colon before any `/` as ssh, so that origin matches only itself.
    repo_git(&b, &["remote", "set-url", "origin", "gh:me/dots.git"]);
    exits(&b.run(&["init", "gh:me/dots.git", "--dry-run"]), 0);
    let err = exits(&b.run(&["init", "./gh:me/dots.git", "--dry-run"]), 1);
    assert!(err.contains("a repo already exists"), "{err}");
}

#[test]
fn init_never_prints_the_credentials_in_its_source() {
    let a = published();
    let b = a.second_device();
    let url = format!("file://user:s3cr3t@localhost{}", b.remote_path().display());
    let first = exits(&b.run(&["init", &url]), 0);
    assert!(first.contains("cloned file://***@localhost/"), "{first}");
    let again = b.run(&["init", &url]);
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&again.stdout),
        String::from_utf8_lossy(&again.stderr)
    );
    assert!(
        said.contains("already holds the clone of file://***@localhost/"),
        "{said}"
    );
    let c = a.second_device();
    let dry = exits(&c.run(&["init", &url, "--dry-run"]), 0);
    assert!(dry.contains("would clone file://***@localhost/"), "{dry}");
    for text in [first, said, dry] {
        assert!(!text.contains("s3cr3t"), "{text}");
    }
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
    env.git(&["-C", &work_arg, "init", "-q"]);
    env.git(&["-C", &work_arg, "symbolic-ref", "HEAD", "refs/heads/main"]);
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
