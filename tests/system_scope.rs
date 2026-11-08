mod common;

use common::{Entry, TestEnv, tree_checksum};
use dotman::config::Config;
use dotman::state::State;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};
use std::process::Output;

const KEYD: &str = "/etc/keyd/default.conf";

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn exits(env: &TestEnv, code: i32, args: &[&str]) -> String {
    let out = env.run(args);
    assert_eq!(
        out.status.code(),
        Some(code),
        "dot {args:?}: {}",
        stderr(&out)
    );
    stderr(&out)
}

fn machine_config(env: &TestEnv) -> std::path::PathBuf {
    env.xdg_config().join("dotman/config.toml")
}

/// An initialised repo with the system root in use and `roots` configured.
fn setup(roots: &[&str]) -> TestEnv {
    let mut env = TestEnv::new();
    env.use_system_root();
    env.ok(&["init"]);
    if !roots.is_empty() {
        let list: Vec<String> = roots.iter().map(|r| format!("\"{r}\"")).collect();
        env.write_machine_config(&format!("[system]\nroots = [{}]\n", list.join(", ")));
    }
    env
}

type Snapshot = (
    [BTreeMap<String, Entry>; 4],
    BTreeMap<String, Entry>,
    Option<Vec<u8>>,
);

fn snapshot(env: &TestEnv) -> Snapshot {
    (
        env.four_trees(),
        tree_checksum(&env.system_root()),
        fs::read(machine_config(env)).ok(),
    )
}

// reason: a shared helper, and clippy.toml exempts `expect` only inside `#[test]` functions.
#[allow(clippy::expect_used)]
fn state(env: &TestEnv) -> State {
    State::read(&env.state_dir().join("state.toml"))
        .expect("read state")
        .expect("a state file")
}

// reason: a shared helper, and clippy.toml exempts `expect` only inside `#[test]` functions.
#[allow(clippy::expect_used)]
fn user_and_group(env: &TestEnv, key: &str) -> (String, String) {
    let meta = fs::metadata(env.system_path(key)).expect("stat");
    let owner = dotman::sys::passwd_by_uid(meta.uid()).expect("passwd entry");
    let group = dotman::sys::group_by_gid(meta.gid()).expect("group entry");
    (owner.name, group.name)
}

fn not_root() -> bool {
    dotman::sys::geteuid() != 0
}

#[test]
fn add_works_once_the_root_is_configured() {
    let env = setup(&["/etc"]);
    env.write_system(KEYD, b"[ids]\n*\n", 0o644);
    let err = env.ok(&["add", "--system", KEYD]);
    assert!(err.contains("added 1 system files"), "{err}");

    let source = env.repo().join(".dotman/system/etc/keyd/default.conf");
    assert_eq!(fs::read(source).expect("source"), b"[ids]\n*\n");
    assert_eq!(env.token_of("status", KEYD).as_deref(), Some("clean"));
    assert!(
        env.porcelain("list")
            .contains(&("system".to_owned(), KEYD.to_owned()))
    );
    let recorded = state(&env).system.get(KEYD).cloned().expect("S entry");
    assert_eq!(recorded.source, ".dotman/system/etc/keyd/default.conf");
}

#[test]
fn owner_group_and_a_masked_mode_are_recorded_by_name() {
    let env = setup(&["/etc"]);
    env.write_system(KEYD, b"x", 0o4755);
    env.ok(&["add", "--system", KEYD]);
    let (owner, group) = user_and_group(&env, KEYD);
    let config = env.repo_config();
    let file = config.system.get(KEYD).expect("a [system] table");
    assert_eq!((file.owner.clone(), file.group.clone()), (owner, group));
    assert_eq!(file.mode, "0755");
    assert!(!file.package_owned);
}

#[test]
fn a_missing_root_exits_3_with_the_config_line_and_add_root_succeeds() {
    let env = setup(&[]);
    env.write_system(KEYD, b"x", 0o644);
    let before = snapshot(&env);
    let err = exits(&env, 3, &["add", "--system", KEYD]);
    assert!(err.contains("[system]"), "{err}");
    assert!(err.contains("roots = [\"/etc\"]"), "{err}");
    assert_eq!(snapshot(&env), before);

    env.ok(&["add", "--system", "--add-root", KEYD]);
    let (config, _) = Config::parse(&fs::read_to_string(machine_config(&env)).expect("config"))
        .expect("parse config");
    assert_eq!(config.system.roots, [std::path::PathBuf::from("/etc")]);
    assert!(
        env.repo()
            .join(".dotman/system/etc/keyd/default.conf")
            .exists()
    );
}

#[test]
fn the_config_line_extends_existing_roots() {
    let env = setup(&["/opt"]);
    env.write_system(KEYD, b"x", 0o644);
    let err = exits(&env, 3, &["add", "--system", KEYD]);
    assert!(err.contains("roots = [\"/opt\", \"/etc\"]"), "{err}");
}

#[test]
fn a_file_with_mode_0000_is_refused_as_a_probable_secret() {
    let env = setup(&["/etc"]);
    env.write_system(KEYD, b"secret", 0o000);
    // The checksum walk cannot read the 0000 file itself.
    let before = (env.four_trees(), fs::read(machine_config(&env)).ok());
    let err = exits(&env, 1, &["add", "--system", KEYD]);
    assert!(err.contains("secret"), "{err}");
    assert!(err.contains("{age}"), "{err}");
    assert_eq!(
        (env.four_trees(), fs::read(machine_config(&env)).ok()),
        before
    );
}

#[test]
fn a_package_owned_file_needs_the_flag() {
    let env = setup(&["/etc"]);
    env.stub_package_manager(&[KEYD]);
    env.write_system(KEYD, b"x", 0o644);
    let before = snapshot(&env);
    let err = exits(&env, 1, &["add", "--system", KEYD]);
    assert!(err.contains("--package-owned"), "{err}");
    assert!(err.contains("keyd"), "{err}");
    assert_eq!(snapshot(&env), before);

    env.ok(&["add", "--system", "--package-owned", KEYD]);
    assert!(
        env.repo_config()
            .system
            .get(KEYD)
            .expect("table")
            .package_owned
    );
}

#[test]
fn a_pacnew_sibling_is_reported_by_status_and_doctor() {
    let env = setup(&["/etc"]);
    env.write_system(KEYD, b"x", 0o644);
    env.ok(&["add", "--system", KEYD]);
    assert!(!env.ok(&["status"]).contains(".pacnew"));
    env.write_system("/etc/keyd/default.conf.pacnew", b"new", 0o644);
    let err = env.ok(&["status"]);
    assert!(err.contains("default.conf.pacnew"), "{err}");
    let doctor = env.run(&["doctor"]);
    let out = stdout(&doctor);
    assert!(out.contains("default.conf.pacnew"), "{out}");
    assert!(out.contains("DOTMAN_SYSTEM_ROOT"), "{out}");
}

#[test]
fn without_roots_status_says_system_off_and_apply_never_touches_the_system() {
    let env = setup(&[]);
    env.write_repo(".dotman/system/etc/x.conf", b"from the repo");
    assert_eq!(
        env.token_of("status", "/etc/x.conf").as_deref(),
        Some("system-off")
    );
    let before = tree_checksum(&env.system_root());
    env.ok(&["apply"]);
    assert_eq!(tree_checksum(&env.system_root()), before);
    assert!(!env.system_path("/etc/x.conf").exists());
}

#[test]
fn apply_never_writes_the_system_tree_even_with_roots() {
    let env = setup(&["/etc"]);
    env.write_system(KEYD, b"old", 0o644);
    env.ok(&["add", "--system", KEYD]);
    env.write_repo(".dotman/system/etc/keyd/default.conf", b"new");
    assert_eq!(
        env.token_of("status", KEYD).as_deref(),
        Some("changed-in-repo")
    );
    let before = tree_checksum(&env.system_root());
    let err = env.ok(&["apply"]);
    assert!(err.contains("apply --system"), "{err}");
    assert_eq!(tree_checksum(&env.system_root()), before);
    assert_eq!(fs::read(env.system_path(KEYD)).expect("read"), b"old");
}

#[test]
fn a_symlink_above_the_prefix_is_ignored() {
    let env = setup(&["/etc"]);
    env.write_system(KEYD, b"x", 0o644);
    let link = env.root().join("link");
    symlink(env.system_root(), &link).expect("symlink");
    let link = link.to_string_lossy().into_owned();
    let out = env.run_with(&[("DOTMAN_SYSTEM_ROOT", &link)], &["add", "--system", KEYD]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn a_symlink_below_the_prefix_is_rejected() {
    let env = setup(&["/etc"]);
    env.write_system("/elsewhere/y.conf", b"y", 0o644);
    fs::create_dir_all(env.system_path("/etc")).expect("mkdir");
    symlink(env.system_path("/elsewhere"), env.system_path("/etc/sub")).expect("symlink");
    let err = exits(&env, 1, &["add", "--system", "/etc/sub/y.conf"]);
    assert!(err.contains("/etc/sub"), "{err}");
    assert!(err.contains("symlink"), "{err}");

    env.write_system(KEYD, b"x", 0o644);
    env.ok(&["add", "--system", KEYD]);
    fs::remove_dir_all(env.system_path("/etc/keyd")).expect("remove");
    symlink(env.system_path("/elsewhere"), env.system_path("/etc/keyd")).expect("symlink");
    assert_eq!(env.token_of("status", KEYD).as_deref(), Some("blocked"));
}

#[test]
fn a_symlink_file_is_refused() {
    let env = setup(&["/etc"]);
    env.write_system("/etc/real.conf", b"x", 0o644);
    symlink(
        env.system_path("/etc/real.conf"),
        env.system_path("/etc/link.conf"),
    )
    .expect("symlink");
    let err = exits(&env, 1, &["add", "--system", "/etc/link.conf"]);
    assert!(err.contains("symlink"), "{err}");
}

#[test]
fn an_age_system_source_is_invalid_and_never_applied() {
    let env = setup(&["/etc"]);
    env.write_repo(".dotman/system/etc/x{age}.conf", b"ciphertext");
    assert_eq!(
        env.token_of("status", "/etc/x.conf").as_deref(),
        Some("invalid")
    );
    env.ok(&["apply"]);
    assert!(!env.system_path("/etc/x.conf").exists());
    assert!(!env.system_path("/etc/x{age}.conf").exists());
}

#[test]
fn age_and_encrypt_are_refused_for_system_files() {
    let env = setup(&["/etc"]);
    env.write_system("/etc/x{age}.conf", b"x", 0o644);
    exits(&env, 1, &["add", "--system", "/etc/x{age}.conf"]);
    env.write_system(KEYD, b"x", 0o644);
    let err = exits(&env, 1, &["add", "--system", "--encrypt", KEYD]);
    assert!(err.contains("--encrypt"), "{err}");
}

#[test]
fn never_allowed_keys_are_refused() {
    let env = setup(&["/etc", "/tmp", "/proc"]);
    env.write_system("/tmp/x", b"x", 0o644);
    let before = snapshot(&env);
    for key in ["/proc/x", "/tmp/x"] {
        let err = exits(&env, 1, &["add", "--system", "--add-root", key]);
        assert!(err.contains("never-allowed"), "{key}: {err}");
    }
    assert_eq!(snapshot(&env), before);
}

#[test]
fn a_root_of_slash_is_reported_by_doctor_and_never_used() {
    let env = setup(&["/"]);
    env.write_system(KEYD, b"x", 0o644);
    let doctor = env.run(&["doctor"]);
    let out = stdout(&doctor);
    assert!(out.contains("never-allowed"), "{out}");
    assert_eq!(doctor.status.code(), Some(1));
    exits(&env, 3, &["add", "--system", KEYD]);
    assert!(!env.repo().join(".dotman/system").exists());
}

#[test]
fn for_places_the_variant_block_on_the_source_name() {
    let env = setup(&["/etc"]);
    env.write_system("/etc/pacman.conf", b"x", 0o644);
    env.ok(&[
        "add",
        "--system",
        "--for",
        "host=elsewhere",
        "/etc/pacman.conf",
    ]);
    assert!(
        env.repo()
            .join(".dotman/system/etc/pacman{host=elsewhere}.conf")
            .exists()
    );
    assert!(env.repo_config().system.contains_key("/etc/pacman.conf"));
}

#[test]
fn sync_captures_a_local_edit_and_its_mode() {
    let env = setup(&["/etc"]);
    env.write_system(KEYD, b"one", 0o644);
    env.ok(&["add", "--system", KEYD]);
    env.ok(&["sync"]);

    env.write_system(KEYD, b"two", 0o600);
    assert_eq!(
        env.token_of("status", KEYD).as_deref(),
        Some("changed-locally")
    );
    env.ok(&["sync"]);
    let source = env.repo().join(".dotman/system/etc/keyd/default.conf");
    assert_eq!(fs::read(source).expect("source"), b"two");
    assert_eq!(
        env.repo_config().system.get(KEYD).expect("table").mode,
        "0600"
    );
    assert_eq!(env.token_of("status", KEYD).as_deref(), Some("clean"));
    let log = env.git(&["-C", &env.repo().to_string_lossy(), "log", "--stat", "-1"]);
    assert!(
        log.contains(".dotman/system/etc/keyd/default.conf"),
        "{log}"
    );
}

#[test]
fn sync_captures_even_when_the_roots_are_gone() {
    let env = setup(&["/etc"]);
    env.write_system(KEYD, b"one", 0o644);
    env.ok(&["add", "--system", KEYD]);
    env.write_machine_config("");
    env.write_system(KEYD, b"two", 0o644);
    assert_eq!(env.token_of("status", KEYD).as_deref(), Some("system-off"));
    env.ok(&["sync"]);
    let source = env.repo().join(".dotman/system/etc/keyd/default.conf");
    assert_eq!(fs::read(source).expect("source"), b"two");
}

#[test]
fn sync_holds_an_edit_that_holds_an_age_secret_key() {
    let env = setup(&["/etc"]);
    env.write_system(KEYD, b"one", 0o644);
    env.ok(&["add", "--system", KEYD]);
    env.write_system(KEYD, b"AGE-SECRET-KEY-1ABC", 0o644);
    let err = exits(&env, 3, &["sync"]);
    assert!(err.contains("held"), "{err}");
    let source = env.repo().join(".dotman/system/etc/keyd/default.conf");
    assert_eq!(fs::read(source).expect("source"), b"one");
}

#[test]
fn an_unreadable_managed_system_file_is_reported_and_skipped() {
    if !not_root() {
        return;
    }
    let env = setup(&["/etc"]);
    env.write_system(KEYD, b"one", 0o644);
    env.ok(&["add", "--system", KEYD]);
    env.write_system("/etc/other.conf", b"x", 0o644);
    env.ok(&["add", "--system", "/etc/other.conf"]);
    fs::set_permissions(
        env.system_path(KEYD),
        std::os::unix::fs::PermissionsExt::from_mode(0o000),
    )
    .expect("chmod");
    assert_eq!(env.token_of("status", KEYD).as_deref(), Some("unreadable"));
    assert_eq!(
        env.token_of("status", "/etc/other.conf").as_deref(),
        Some("clean")
    );
    let err = exits(&env, 0, &["sync"]);
    assert!(err.contains("cannot be read"), "{err}");
}

#[test]
fn dry_runs_of_add_leave_everything_unchanged() {
    let env = setup(&["/etc"]);
    env.write_system(KEYD, b"x", 0o644);
    let before = snapshot(&env);
    let err = env.ok(&["add", "--system", "--dry-run", KEYD]);
    assert!(err.contains("would add"), "{err}");
    assert_eq!(snapshot(&env), before);

    let bare = setup(&[]);
    bare.write_system(KEYD, b"x", 0o644);
    let before = snapshot(&bare);
    let err = bare.ok(&["add", "--system", "--dry-run", "--add-root", KEYD]);
    assert!(err.contains("would add"), "{err}");
    assert_eq!(snapshot(&bare), before);
    exits(&bare, 3, &["add", "--system", "--dry-run", KEYD]);
    assert_eq!(snapshot(&bare), before);
}

#[test]
fn diff_includes_system_entries() {
    let env = setup(&["/etc"]);
    env.write_system(KEYD, b"old\n", 0o644);
    env.ok(&["add", "--system", KEYD]);
    env.write_repo(".dotman/system/etc/keyd/default.conf", b"new\n");
    let out = env.run(&["diff"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("system/etc/keyd/default.conf"), "{text}");
    assert!(text.contains("-old") && text.contains("+new"), "{text}");
}

#[test]
fn paths_outside_home_and_inside_it_are_told_apart() {
    let env = setup(&["/etc"]);
    env.write_system(KEYD, b"x", 0o644);
    let err = exits(&env, 1, &["add", "--system", "../../../.bashrc"]);
    assert!(err.contains("relative"), "{err}");
    env.write(".bashrc", b"x", 0o644);
    let home_file = env.home().join(".bashrc");
    let err = exits(&env, 1, &["add", "--system", &home_file.to_string_lossy()]);
    assert!(err.contains("$HOME"), "{err}");
}
