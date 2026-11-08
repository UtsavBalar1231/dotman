mod common;

use common::{
    Entry, TestEnv, configure_machine as configure, machine_config, mode_of, plan_entry as entry,
    plan_entry_hashed as entry_hashed, plan_json, system_config, system_setup as setup,
    system_setup_none as setup_none, system_write, tree_checksum, write_plan, xxh3,
};
use dotman::config::Config;
use dotman::repo_config::SystemFile;
use dotman::state::State;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
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

// reason: a shared helper, and clippy.toml exempts `expect` only inside `#[test]` functions.
#[allow(clippy::expect_used)]
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The plan directories left under the state dir.
fn plans_left(env: &TestEnv) -> Vec<String> {
    let parent = env.state_dir().join("system-plan");
    if parent.exists() {
        names_in(&parent)
    } else {
        Vec::new()
    }
}

#[test]
fn a_valid_plan_writes_the_file_with_its_mode_and_skips_the_owner() {
    let env = setup_none(&["/etc"]);
    env.write_system(KEYD, b"old\n", 0o600);
    let plan = plan_json(&env, &["/etc"], &[entry(KEYD, b"[ids]\n", "0640")]);
    let out = system_write(&env, &write_plan(&env, &plan));
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), format!("skipped-owner {KEYD}\n"));
    assert_eq!(fs::read(env.system_path(KEYD)).expect("read"), b"[ids]\n");
    assert_eq!(mode_of(&env.system_path(KEYD)), 0o640);
    assert_eq!(names_in(&env.system_path("/etc/keyd")), ["default.conf"]);
}

#[test]
fn hand_made_plans_are_rejected_with_no_write() {
    let env = setup_none(&["/etc"]);
    env.write_system("/etc/keep.conf", b"keep", 0o644);
    env.write_system("/elsewhere/y.conf", b"y", 0o644);
    env.write_system("/opt/x.conf", b"x", 0o644);
    symlink(env.system_path("/elsewhere"), env.system_path("/etc/sub")).expect("symlink");
    let good = entry("/etc/new.conf", b"new", "0644");
    let cases = [
        (
            "/etc/../etc/dotdot.conf",
            entry("/etc/../etc/dotdot.conf", b"x", "0644"),
            "`..` component",
        ),
        (
            "/etc/sub/y.conf",
            entry("/etc/sub/y.conf", b"x", "0644"),
            "symlink",
        ),
        ("/opt/x.conf", entry("/opt/x.conf", b"x", "0644"), "outside"),
        ("/proc/x", entry("/proc/x", b"x", "0644"), "never-allowed"),
        (
            "/etc/hash.conf",
            entry_hashed("/etc/hash.conf", b"one", "0644", &xxh3(b"two")),
            "xxh3",
        ),
    ];
    let before = tree_checksum(&env.system_root());
    for (target, bad, why) in cases {
        let plan = plan_json(&env, &["/etc"], &[good.clone(), bad]);
        let out = system_write(&env, &write_plan(&env, &plan));
        assert_eq!(out.status.code(), Some(1), "{target}: {}", stderr(&out));
        let text = stdout(&out);
        assert!(
            text.contains("not-written /etc/new.conf\n"),
            "{target}: {text}"
        );
        let failed = format!("failed {target}: ");
        let reason = text
            .lines()
            .find_map(|line| line.strip_prefix(&failed))
            .unwrap_or_default();
        assert!(reason.contains(why), "{target}: {text}");
        assert!(stderr(&out).contains("nothing was written"), "{target}");
        assert_eq!(tree_checksum(&env.system_root()), before, "{target}");
    }

    let plan = plan_json(&env, &["/etc", "/opt"], &[good]);
    let out = system_write(&env, &write_plan(&env, &plan));
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("differ from [system] roots"),
        "{}",
        stderr(&out)
    );
    assert_eq!(stdout(&out), "");
    assert_eq!(tree_checksum(&env.system_root()), before);
}

#[test]
fn a_mode_of_4755_is_written_as_0755() {
    let env = setup_none(&["/etc"]);
    fs::create_dir_all(env.system_path("/etc")).expect("mkdir");
    let plan = plan_json(
        &env,
        &["/etc"],
        &[entry("/etc/tool", b"#!/bin/sh\n", "4755")],
    );
    let out = system_write(&env, &write_plan(&env, &plan));
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(mode_of(&env.system_path("/etc/tool")), 0o755);
}

#[test]
fn a_plan_at_0644_or_behind_a_symlink_is_refused() {
    let env = setup_none(&["/etc"]);
    fs::create_dir_all(env.system_path("/etc")).expect("mkdir");
    let plan = write_plan(
        &env,
        &plan_json(&env, &["/etc"], &[entry("/etc/x.conf", b"x", "0644")]),
    );
    fs::set_permissions(&plan, fs::Permissions::from_mode(0o644)).expect("chmod");
    let out = system_write(&env, &plan);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("not 0600"), "{}", stderr(&out));

    fs::set_permissions(&plan, fs::Permissions::from_mode(0o600)).expect("chmod");
    let link = plan.with_file_name("link.json");
    symlink(&plan, &link).expect("symlink");
    let out = system_write(&env, &link);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("is a symlink"), "{}", stderr(&out));
    assert!(!env.system_path("/etc/x.conf").exists());
}

#[test]
fn a_group_writable_config_is_refused() {
    let env = setup_none(&["/etc"]);
    fs::create_dir_all(env.system_path("/etc")).expect("mkdir");
    let plan = write_plan(
        &env,
        &plan_json(&env, &["/etc"], &[entry("/etc/x.conf", b"x", "0644")]),
    );
    fs::set_permissions(machine_config(&env), fs::Permissions::from_mode(0o664)).expect("chmod");
    let out = system_write(&env, &plan);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("writable by group or others"),
        "{}",
        stderr(&out)
    );
    assert!(!env.system_path("/etc/x.conf").exists());
}

#[test]
fn under_sudo_the_helper_refuses_to_run_as_a_user() {
    if !not_root() {
        return;
    }
    let env = setup(&[]);
    configure(&env, &system_config(&["/etc"], "sudo"));
    fs::create_dir_all(env.system_path("/etc")).expect("mkdir");
    let plan = write_plan(
        &env,
        &plan_json(&env, &["/etc"], &[entry("/etc/x.conf", b"x", "0644")]),
    );
    let out = system_write(&env, &plan);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("runs as root"), "{}", stderr(&out));
    assert!(!env.system_path("/etc/x.conf").exists());
}

/// A stub escalator named `name`: it appends its arguments to the returned log, then runs what
/// follows its options, unprivileged.
// reason: a shared helper, and clippy.toml exempts `expect` only inside `#[test]` functions.
#[allow(clippy::expect_used)]
fn stub_escalator(env: &TestEnv, name: &str) -> PathBuf {
    let log = env.root().join(format!("{name}.log"));
    let body = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    --) shift; exec \"$@\" ;;\n    -*) shift ;;\n    *) exec \"$@\" ;;\n  esac\ndone\n",
        log.display()
    );
    let stub = env.root().join("stubs").join(name);
    fs::write(&stub, body).expect("write the stub");
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).expect("chmod the stub");
    log
}

/// A managed system file whose repo side changed, so `apply --system` has one write to do.
fn one_pending_write(env: &TestEnv) {
    env.write_system(KEYD, b"old\n", 0o644);
    env.ok(&["add", "--system", KEYD]);
    env.write_repo(".dotman/system/etc/keyd/default.conf", b"new\n");
}

#[test]
fn each_escalator_gets_its_non_interactive_flags_and_the_exe() {
    let exe = fs::canonicalize(env!("CARGO_BIN_EXE_dotman")).expect("the dotman binary");
    let run = format!("{} system-write --plan ", exe.display());
    let cases: [(&str, Vec<String>); 3] = [
        ("sudo", vec!["-n -v".to_owned(), format!("-n -- {run}")]),
        ("doas", vec![format!("-n -- {run}")]),
        ("run0", vec![format!("--no-ask-password -- {run}")]),
    ];
    for (tool, expected) in cases {
        let env = setup(&[]);
        configure(&env, &system_config(&["/etc"], tool));
        let log = stub_escalator(&env, tool);
        one_pending_write(&env);
        let before = tree_checksum(&env.system_root());

        let out = env.run(&["apply", "--system", "--yes"]);
        assert_eq!(out.status.code(), Some(1), "{tool}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("runs as root"), "{tool}: {err}");
        assert!(
            err.contains(&format!("{tool} did not run system-write")),
            "{tool}: {err}"
        );
        let calls = fs::read_to_string(&log).expect("the stub log");
        let calls: Vec<&str> = calls.lines().collect();
        assert_eq!(calls.len(), expected.len(), "{tool}: {calls:?}");
        for (call, prefix) in calls.iter().zip(&expected) {
            assert!(
                call.starts_with(prefix.as_str()),
                "{tool}: {call} vs {prefix}"
            );
        }
        let last = calls.last().copied().unwrap_or_default();
        assert!(last.contains("/system-plan/"), "{tool}: {last}");
        assert!(last.ends_with("/plan.json"), "{tool}: {last}");
        assert_eq!(plans_left(&env), Vec::<String>::new(), "{tool}");
        assert_eq!(tree_checksum(&env.system_root()), before, "{tool}");
        assert!(
            state(&env)
                .system
                .get(KEYD)
                .is_some_and(|s| s.hash == xxh3(b"old\n"))
        );
    }
}

#[test]
fn with_no_escalator_on_path_the_run_exits_1_with_the_hint() {
    let env = setup(&["/etc"]);
    one_pending_write(&env);
    let bin = env.root().join("gitbin");
    fs::create_dir_all(&bin).expect("mkdir");
    let git = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH");
    symlink(git, bin.join("git")).expect("symlink git");
    let path = format!("{}:{}", env.root().join("stubs").display(), bin.display());
    let before = tree_checksum(&env.system_root());

    let out = env.run_with(&[("PATH", &path)], &["apply", "--system", "--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let err = stderr(&out);
    assert!(err.contains("no sudo, doas or run0 is on PATH"), "{err}");
    assert!(err.contains("escalate"), "{err}");
    assert_eq!(tree_checksum(&env.system_root()), before);
    assert_eq!(plans_left(&env), Vec::<String>::new());
}

#[test]
fn apply_system_yes_writes_backs_up_records_and_status_turns_clean() {
    let env = setup_none(&["/etc"]);
    one_pending_write(&env);
    let mut config = env.repo_config();
    if let Some(file) = config.system.get_mut(KEYD) {
        file.mode = "0640".to_owned();
    }
    config
        .save(&env.repo().join(".dotman.toml"), false)
        .expect("save .dotman.toml");

    let before = tree_checksum(&env.system_root());
    env.ok(&["apply"]);
    assert_eq!(tree_checksum(&env.system_root()), before);

    let err = env.ok(&["apply", "--system", "--yes"]);
    assert!(err.contains("owner and group left as they are"), "{err}");
    assert_eq!(fs::read(env.system_path(KEYD)).expect("read"), b"new\n");
    assert_eq!(mode_of(&env.system_path(KEYD)), 0o640);
    assert_eq!(
        env.backups(".dotman/system/etc/keyd/default.conf"),
        [(b"old\n".to_vec(), 0o600)]
    );
    let recorded = state(&env).system.get(KEYD).cloned().expect("S entry");
    assert_eq!(recorded.hash, xxh3(b"new\n"));
    assert_eq!(recorded.mode, "0640");
    assert_eq!(env.token_of("status", KEYD).as_deref(), Some("clean"));
    assert_eq!(plans_left(&env), Vec::<String>::new());
}

#[test]
fn apply_system_without_yes_exits_3_when_it_cannot_ask_and_writes_nothing() {
    let env = setup_none(&["/etc"]);
    one_pending_write(&env);
    let before = snapshot(&env);
    let err = exits(&env, 3, &["apply", "--system"]);
    assert!(err.contains("--yes"), "{err}");
    assert_eq!(snapshot(&env), before);
    assert_eq!(fs::read(env.system_path(KEYD)).expect("read"), b"old\n");
}

#[test]
fn a_dry_run_of_apply_system_changes_nothing() {
    let env = setup_none(&["/etc"]);
    one_pending_write(&env);
    let before = snapshot(&env);
    let err = env.ok(&["apply", "--system", "--dry-run", "--yes"]);
    assert!(err.contains(&format!("would write {KEYD}")), "{err}");
    assert_eq!(snapshot(&env), before);
    exits(&env, 3, &["apply", "--system", "--dry-run"]);
    assert_eq!(snapshot(&env), before);
}

#[test]
fn sync_system_yes_writes_what_the_pull_brought() {
    let a = setup_none(&["/etc"]);
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write_system(KEYD, b"one\n", 0o644);
    a.ok(&["add", "--system", KEYD]);
    a.ok(&["sync"]);
    let remote = a.remote_path().to_string_lossy().into_owned();
    a.git(&[
        "--git-dir",
        &remote,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);

    let mut b = a.second_device();
    b.use_system_root();
    configure(&b, &system_config(&["/etc"], "none"));
    fs::create_dir_all(b.system_path("/etc/keyd")).expect("mkdir");
    b.ok(&["init", &b.remote_url(), "--take-repo"]);
    assert!(
        !b.system_path(KEYD).exists(),
        "init <url> applied a system file"
    );

    a.write_system(KEYD, b"two\n", 0o644);
    a.ok(&["sync"]);
    b.ok(&["sync", "--system", "--yes"]);
    assert_eq!(fs::read(b.system_path(KEYD)).expect("read"), b"two\n");
    assert_eq!(b.token_of("status", KEYD).as_deref(), Some("clean"));
}

/// The one test that uses real sudo. It runs only inside a container that sets
/// `DOTMAN_REAL_SUDO_TEST=1`, so it can never run on a developer machine or a CI host runner.
/// A root-only container, where `dotman apply --system --allow-root` starts `system-write` without
/// an escalator, reaches the helper through the same root path.
#[test]
fn real_sudo_writes_a_root_owned_file() {
    const TARGET: &str = "/etc/dotman-ci/test.conf";
    let wanted = std::env::var_os("DOTMAN_REAL_SUDO_TEST").is_some_and(|v| v == "1");
    let container = ["/.dockerenv", "/run/.containerenv"]
        .iter()
        .any(|marker| Path::new(marker).exists());
    if !wanted || !container {
        println!(
            "skipped: the real-sudo test needs DOTMAN_REAL_SUDO_TEST=1 ({}) and a container marker, /.dockerenv or /run/.containerenv ({})",
            if wanted { "set" } else { "not set" },
            if container { "found" } else { "not found" }
        );
        return;
    }
    let sudo = |args: &[&str]| {
        let status = std::process::Command::new("sudo")
            .arg("-n")
            .args(args)
            .status()
            .expect("run sudo");
        assert!(status.success(), "sudo -n {args:?}: {status}");
    };
    let env = TestEnv::new();
    env.ok(&["init"]);
    configure(
        &env,
        "[system]\nroots = [\"/etc/dotman-ci\"]\nescalate = \"sudo\"\n",
    );
    sudo(&["mkdir", "-p", "/etc/dotman-ci"]);
    env.write_repo(".dotman/system/etc/dotman-ci/test.conf", b"ci\n");
    let mut config = env.repo_config();
    config.system.insert(
        TARGET.to_owned(),
        SystemFile {
            owner: "root".to_owned(),
            group: "root".to_owned(),
            mode: "0640".to_owned(),
            package_owned: false,
        },
    );
    config
        .save(&env.repo().join(".dotman.toml"), false)
        .expect("save .dotman.toml");

    let out = env.run(&["apply", "--system", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let stat = std::process::Command::new("stat")
        .args(["-c", "%U:%G %a", TARGET])
        .output()
        .expect("run stat");
    assert_eq!(
        String::from_utf8_lossy(&stat.stdout).trim(),
        "root:root 640"
    );
    sudo(&["rm", "-rf", "/etc/dotman-ci"]);
}

#[test]
fn an_existing_system_file_needs_take_repo_like_a_home_file() {
    let env = setup_none(&["/etc"]);
    env.write_system("/etc/x.conf", b"local\n", 0o644);
    env.write_repo(".dotman/system/etc/x.conf", b"repo\n");
    assert_eq!(
        env.token_of("status", "/etc/x.conf").as_deref(),
        Some("existing")
    );
    let err = exits(&env, 3, &["apply", "--system", "--yes"]);
    assert!(err.contains("--take-repo"), "{err}");
    assert_eq!(
        fs::read(env.system_path("/etc/x.conf")).expect("read"),
        b"local\n"
    );

    env.ok(&["apply", "--system", "--yes", "--take-repo"]);
    assert_eq!(
        fs::read(env.system_path("/etc/x.conf")).expect("read"),
        b"repo\n"
    );
    assert_eq!(
        env.backups(".dotman/system/etc/x.conf"),
        [(b"local\n".to_vec(), 0o600)]
    );
}

#[test]
fn a_system_path_argument_limits_the_write() {
    let env = setup_none(&["/etc"]);
    fs::create_dir_all(env.system_path("/etc")).expect("mkdir");
    env.write_repo(".dotman/system/etc/a.conf", b"a\n");
    env.write_repo(".dotman/system/etc/b.conf", b"b\n");
    env.ok(&["apply", "--system", "--yes", "/etc/a.conf"]);
    assert!(env.system_path("/etc/a.conf").exists());
    assert!(!env.system_path("/etc/b.conf").exists());
    let err = exits(&env, 1, &["apply", "--system", "--yes", "/etc/none.conf"]);
    assert!(err.contains("not managed"), "{err}");
}
