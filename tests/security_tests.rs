//! The security regression suite, through the `dotman` binary. AGENTS.md points auditors here.
//!
//! Each test's doc comment states what must hold for it to pass. Cases that are decided by a
//! value the binary cannot be given live in unit tests, and are named where they apply:
//!
//! - Running as root is refused (`paths::tests::root_needs_flag_or_env`, and
//!   `running_as_root_is_refused_unless_allowed` below through the library). The `dotman` binary as
//!   uid 0 runs in the T29 container job.
//! - As root, `system-write` ignores `DOTMAN_SYSTEM_ROOT` and refuses a plan that carries a
//!   prefix (`system::helper::tests::a_prefix_never_reaches_root`).

mod common;

use common::{
    LEGACY_REPO, R, Setup, TestEnv, age_installed, archive_name, curl_available, files_containing,
    have_chezmoi, legacy_fixture, mode_of, overlay_source, plan_entry, plan_json, release,
    source_tree, stow, stub_chezmoi, system_setup, system_setup_none, system_write, text,
    tree_checksum, write_plan, yadm_fixture,
};
use dotman::paths::{Env, Paths, ensure_not_root};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Output;

const FAKE_KEY: &str =
    "AGE-SECRET-KEY-1QQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQQ";
const KEYD: &str = "/etc/keyd/default.conf";

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn exits(out: &Output, code: i32) -> String {
    let err = stderr(out);
    assert_eq!(out.status.code(), Some(code), "{err}");
    err
}

fn is_root() -> bool {
    dotman::sys::geteuid() == 0
}

fn names(dir: &Path) -> Vec<String> {
    let mut found: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

// Path escapes

/// `add ../../etc/passwd`, run from a directory inside HOME, exits 1 and adds nothing, and the
/// file it names outside HOME is unchanged.
#[test]
fn add_refuses_a_path_that_climbs_out_of_home() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let outside = env.root().join("etc/passwd");
    fs::create_dir_all(outside.parent().unwrap()).unwrap();
    fs::write(&outside, b"root:x:0:0:::\n").unwrap();
    let cwd = env.home().join("sub");
    fs::create_dir_all(&cwd).unwrap();
    let repo = tree_checksum(&env.repo());

    let home = env.home().join("../etc/passwd");
    for path in ["../../etc/passwd", "../../etc", &home.to_string_lossy()] {
        let out = env
            .dot()
            .current_dir(&cwd)
            .args(["add", path])
            .output()
            .unwrap();
        exits(&out, 1);
    }
    assert_eq!(tree_checksum(&env.repo()), repo);
    assert_eq!(env.porcelain("list"), []);
    assert_eq!(fs::read(&outside).unwrap(), b"root:x:0:0:::\n");
}

/// `add /etc/passwd` without `--system` exits 1 and adds nothing, whether the file exists below
/// the system root or not, and the system file is unchanged.
#[test]
fn add_refuses_a_system_path_without_the_system_flag() {
    let mut env = TestEnv::new();
    env.use_system_root();
    env.ok(&["init"]);
    env.write_system("/etc/passwd", b"root:x:0:0:::\n", 0o644);
    let system = tree_checksum(&env.system_root());
    let repo = tree_checksum(&env.repo());

    exits(&env.run(&["add", "/etc/passwd"]), 1);
    exits(&env.run(&["add", "/etc/shadow"]), 1);
    assert_eq!(tree_checksum(&env.repo()), repo);
    assert_eq!(tree_checksum(&env.system_root()), system);
    assert_eq!(env.porcelain("list"), []);
}

/// A clone source that carries `.config/key`, applied into a HOME whose `~/.config` is a symlink
/// to a directory outside HOME, writes nothing outside HOME, reports the key as blocked and exits 3.
#[test]
fn a_hostile_home_symlink_never_carries_a_clone_outside_home() {
    let a = TestEnv::new();
    a.ok(&["init"]);
    a.write_repo(".config/key", b"planted\n");
    a.write_repo(".config/other/deep", b"planted\n");
    a.push_repo();

    let b = a.second_device();
    let outside = b.root().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("kept"), b"kept\n").unwrap();
    b.symlink(".config", &outside);
    let before = tree_checksum(&outside);

    let out = b.run(&["init", &a.remote_url(), "--take-repo"]);
    let err = exits(&out, 3);
    assert!(
        err.contains("`.config` is a symlink or not a directory"),
        "{err}"
    );
    assert_eq!(tree_checksum(&outside), before);
    assert_eq!(fs::read_link(b.home().join(".config")).unwrap(), outside);
    assert_eq!(
        b.token_of("status", ".config/key").as_deref(),
        Some("blocked")
    );
    exits(&b.run(&["apply", "--take-repo"]), 3);
    assert_eq!(tree_checksum(&outside), before);
}

/// A symlink at a HOME target (`~/.bashrc` pointing outside HOME) is never written through by
/// `init --take-repo`, `apply`, `sync` or `restore`: the file it points at keeps its bytes and the
/// link stays.
#[test]
fn a_target_symlink_is_never_written_through() {
    let a = TestEnv::new();
    a.ok(&["init"]);
    a.write(".bashrc", b"from the repo\n", 0o644);
    a.ok(&["add", ".bashrc"]);
    a.push_repo();

    let b = a.second_device();
    let victim = b.root().join("victim");
    fs::write(&victim, b"victim\n").unwrap();
    b.symlink(".bashrc", &victim);

    exits(&b.run(&["init", &a.remote_url(), "--take-repo"]), 3);
    for args in [
        &["apply", "--take-repo"][..],
        &["apply"],
        &["sync"],
        &["sync", "--take-repo"],
        &["restore", ".bashrc"],
    ] {
        let out = b.run(args);
        assert_ne!(out.status.code(), Some(0), "{args:?}: {}", stderr(&out));
        assert_eq!(fs::read(&victim).unwrap(), b"victim\n", "{args:?}");
        assert_eq!(
            fs::read_link(b.home().join(".bashrc")).unwrap(),
            victim,
            "{args:?}"
        );
    }
    assert_eq!(
        b.token_of("status", ".bashrc").as_deref(),
        Some("symlink-at-target")
    );
}

// Permissions

/// A `[modes]` entry of 4755, 2755, 6755 or 1755 on a file applies as 0755; an entry of 2755 on a
/// directory creates it at 0755 and 1777 at 0777, so the setuid, setgid and sticky bits are
/// never set.
#[test]
fn special_mode_bits_are_stripped_on_apply() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let files = [
        ("suid", "4755"),
        ("sgid", "2755"),
        ("both", "6755"),
        ("sticky", "1755"),
    ];
    let mut modes = String::from("[modes]\n");
    for (name, mode) in files {
        env.write_repo(&format!("bin/{name}"), b"#!/bin/sh\n");
        writeln!(modes, "\"bin/{name}\" = \"{mode}\"").unwrap();
    }
    env.write_repo("sgid-dir/file", b"x\n");
    env.write_repo("sticky-dir/file", b"x\n");
    modes.push_str("\"sgid-dir/\" = \"2755\"\n\"sticky-dir/\" = \"1777\"\n");
    env.write_repo(".dotman.toml", modes.as_bytes());

    exits(&env.run(&["apply"]), 0);
    for (name, _) in files {
        assert_eq!(mode_of(&env.home().join("bin").join(name)), 0o755, "{name}");
    }
    assert_eq!(mode_of(&env.home().join("sgid-dir")), 0o755);
    assert_eq!(mode_of(&env.home().join("sticky-dir")), 0o777);
    assert_eq!(env.token_of("status", "bin/both").as_deref(), Some("clean"));
}

/// A `[system."<key>"]` mode of 6755 is written as 0755 by `apply --system` under
/// `escalate = "none"`, and a hand-made plan with mode 6755 is written as 0755 by the helper.
#[test]
fn a_system_mode_of_6755_is_written_as_0755() {
    let env = system_setup_none(&["/etc"]);
    env.write_system("/etc/tool", b"old\n", 0o644);
    env.ok(&["add", "--system", "/etc/tool"]);
    env.write_repo(".dotman/system/etc/tool", b"#!/bin/sh\n");
    let mut config = env.repo_config();
    config.system.get_mut("/etc/tool").unwrap().mode = "6755".to_owned();
    config
        .save(&env.repo().join(".dotman.toml"), false)
        .unwrap();

    exits(&env.run(&["apply", "--system", "--yes"]), 0);
    assert_eq!(
        fs::read(env.system_path("/etc/tool")).unwrap(),
        b"#!/bin/sh\n"
    );
    assert_eq!(mode_of(&env.system_path("/etc/tool")), 0o755);

    let plan = plan_json(
        &env,
        &["/etc"],
        &[plan_entry("/etc/by-hand", b"x\n", "6755")],
    );
    let out = system_write(&env, &write_plan(&env, &plan));
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(mode_of(&env.system_path("/etc/by-hand")), 0o755);
}

// Root and home

/// The root check refuses euid 0 unless `--allow-root` or `DOTMAN_ALLOW_ROOT=1`, and `$HOME`
/// resolution refuses a HOME that the effective user does not own, so a run as uid 0 never
/// accepts a user's HOME.
#[test]
fn running_as_root_is_refused_unless_allowed() {
    assert!(ensure_not_root(0, false, None).is_err());
    assert!(ensure_not_root(0, false, Some(&OsString::from("0"))).is_err());
    assert!(ensure_not_root(0, false, Some(&OsString::from("true"))).is_err());
    assert!(ensure_not_root(0, true, None).is_ok());
    assert!(ensure_not_root(1000, false, None).is_ok());

    if is_root() {
        return;
    }
    let env = TestEnv::new();
    let home = env.home().into_os_string();
    let vars = Env::from_fn(|name| (name == "HOME").then(|| home.clone()));
    assert!(Paths::resolve(&vars, dotman::sys::geteuid()).is_ok());
    let err = Paths::resolve(&vars, 0).unwrap_err().to_string();
    assert!(err.contains("not by the current user"), "{err}");
}

/// `HOME=/` and other directories that another user owns are refused with exit 1 before anything
/// is created, by every command that resolves paths.
#[test]
fn a_home_that_another_user_owns_is_refused() {
    if is_root() {
        return;
    }
    let env = TestEnv::new();
    let before = tree_checksum(env.root());
    for home in ["/", "/usr"] {
        for args in [&["init"][..], &["status"], &["add", "x"], &["doctor"]] {
            let out = env.run_with(&[("HOME", home)], args);
            assert_eq!(out.status.code(), Some(1), "{home} {args:?}");
            let all = text(&out);
            assert!(all.contains("is owned by uid 0"), "{home} {args:?}: {all}");
        }
    }
    assert_eq!(tree_checksum(env.root()), before);
}

// Code execution

fn template_env(source: &str) -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".config/app/conf{tmpl}", source.as_bytes());
    env
}

/// A template that calls `run`, `exec`, `system` or `shell` fails with exit 1 and starts no
/// process: the marker file it asks `touch` for does not exist, and nothing is applied.
#[test]
fn a_template_cannot_run_a_command() {
    let env = TestEnv::new();
    let marker = env.root().join("marker");
    for call in [
        format!("run(\"touch\", \"{}\")", marker.display()),
        "run(\"id\")".to_owned(),
        format!("exec(\"touch\", \"{}\")", marker.display()),
        format!("system(\"touch {}\")", marker.display()),
        format!("shell(\"touch {}\")", marker.display()),
        format!("\"\" | run(\"touch\", \"{}\")", marker.display()),
    ] {
        let env = template_env(&format!("x {{{{ {call} }}}}\n"));
        let err = exits(&env.run(&["apply"]), 1);
        assert!(
            err.contains("conf{tmpl}:1: ") && err.contains("is unknown"),
            "{call}: {err}"
        );
        assert!(!env.home().join(".config/app/conf").exists(), "{call}");
        assert!(!marker.exists(), "{call}");
    }
}

/// `{% include %}`, `{% import %}` and `{% extends %}` of a file outside the repo fail with exit 1
/// because the loader knows no template, and the file's bytes reach neither the applied file nor stderr.
#[test]
fn a_template_cannot_read_a_file() {
    let env = TestEnv::new();
    let secret = env.root().join("secret.txt");
    fs::write(&secret, b"top-secret-bytes\n").unwrap();
    for tag in [
        "{% include \"/etc/passwd\" %}".to_owned(),
        format!("{{% include \"{}\" %}}", secret.display()),
        "{% include \"../../../secret.txt\" %}".to_owned(),
        format!("{{% import \"{}\" as s %}}{{{{ s }}}}", secret.display()),
        format!("{{% extends \"{}\" %}}", secret.display()),
    ] {
        let env = template_env(&format!("{tag}\n"));
        fs::write(env.root().join("secret.txt"), b"top-secret-bytes\n").unwrap();
        let out = env.run(&["apply"]);
        let err = exits(&out, 1);
        assert!(
            err.contains("non-existing template") || err.contains("does not exist"),
            "{tag}: {err}"
        );
        assert!(
            !err.contains("top-secret-bytes") && !err.contains("root:x"),
            "{tag}: {err}"
        );
        assert!(!env.home().join(".config/app/conf").exists(), "{tag}");
    }
}

fn hooked(env: &TestEnv) -> PathBuf {
    let marker = env.root().join("hook-marker");
    env.write_repo(".config/app/a.conf", b"a\n");
    env.write_repo(
        ".dotman.toml",
        format!(
            "[hooks]\n\".config/app/**\" = \"echo ran >> '{}'\"\n",
            marker.display()
        )
        .as_bytes(),
    );
    marker
}

/// A hook table nobody approved on this machine never runs in a non-interactive `apply` or
/// `sync`, nor in `init <url>` without `--approve-hooks`: the marker file stays absent while the
/// files are still applied.
#[test]
fn hooks_never_run_without_approval() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let marker = hooked(&env);
    exits(&env.run(&["apply"]), 0);
    exits(&env.run(&["sync"]), 0);
    assert_eq!(env.read(".config/app/a.conf"), b"a\n");
    assert!(!marker.exists());

    let source = TestEnv::new();
    source.ok(&["init"]);
    let marker = hooked(&source);
    source.push_repo();
    let other = source.second_device();
    exits(&other.run(&["init", &source.remote_url()]), 0);
    assert_eq!(other.read(".config/app/a.conf"), b"a\n");
    exits(&other.run(&["sync"]), 0);
    exits(&other.run(&["apply"]), 0);
    assert!(!marker.exists());
}

/// An approved table that is edited upstream needs approval again: a non-interactive `sync` that
/// pulls the edit applies the files and runs neither the old nor the new command.
#[test]
fn an_edited_hook_table_is_not_run_until_approved_again() {
    let a = TestEnv::new();
    a.ok(&["init", "--remote", &a.remote_url()]);
    let marker = hooked(&a);
    exits(&a.run(&["apply", "--approve-hooks"]), 0);
    assert_eq!(fs::read_to_string(&marker).unwrap().lines().count(), 1);
    a.ok(&["sync"]);
    a.git(&[
        "--git-dir",
        &a.remote_path().to_string_lossy(),
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);

    let evil = a.root().join("evil-marker");
    a.write_repo(
        ".dotman.toml",
        format!(
            "[hooks]\n\".config/app/**\" = \"echo pwned >> '{}'\"\n",
            evil.display()
        )
        .as_bytes(),
    );
    a.write_repo(".config/app/a.conf", b"a2\n");
    a.ok(&["sync"]);

    let b = a.second_device();
    exits(&b.run(&["init", &a.remote_url()]), 0);
    assert_eq!(b.read(".config/app/a.conf"), b"a2\n");
    a.write_repo(".config/app/a.conf", b"a3\n");
    a.ok(&["sync"]);
    exits(&b.run(&["sync"]), 0);
    assert_eq!(b.read(".config/app/a.conf"), b"a3\n");
    assert!(!evil.exists());
    assert_eq!(fs::read_to_string(&marker).unwrap().lines().count(), 1);
}

// Imports

/// Importing the legacy repo, with or without `--dry-run`, and a refused second import leave the
/// legacy fixture byte for byte as it was, and HOME unchanged.
#[test]
fn a_legacy_import_leaves_its_source_unchanged() {
    let fixture = legacy_fixture();
    let env = TestEnv::new();
    let home = tree_checksum(&env.home());
    exits(&env.run(&["import", "--dry-run", LEGACY_REPO]), 0);
    fixture.check();
    exits(&env.run(&["import", LEGACY_REPO]), 0);
    fixture.check();
    let err = exits(&env.run(&["import", LEGACY_REPO]), 1);
    assert!(err.contains("--overlay"), "{err}");
    fixture.check();
    assert_eq!(tree_checksum(&env.home()), home);
}

/// Importing a yadm repo (alternates, templates, a submodule), a refused second import and a
/// dry run leave the bare repo's files unchanged, and HOME unchanged.
#[test]
fn a_yadm_import_leaves_its_source_unchanged() {
    let env = TestEnv::new();
    let source = yadm_fixture(&env);
    let home = tree_checksum(&env.home());
    exits(&env.run(&["import", "--dry-run"]), 3);
    source.check();
    exits(&env.run(&["import"]), 3);
    source.check();
    let err = exits(&env.run(&["import"]), 1);
    assert!(err.contains("--overlay"), "{err}");
    source.check();
    assert_eq!(tree_checksum(&env.home()), home);
}

/// Importing a chezmoi source whose template and script would touch marker files leaves the
/// source unchanged and runs neither, with chezmoi doing the dump.
#[test]
fn a_chezmoi_import_leaves_its_source_unchanged_and_runs_nothing() {
    if !have_chezmoi() {
        return;
    }
    let env = TestEnv::new();
    let markers = env.root().join("markers");
    fs::create_dir(&markers).unwrap();
    let touch = |name: &str| markers.join(name).display().to_string();
    let source = source_tree(
        &env,
        "cz",
        &[
            ("dot_bashrc", "export A=1\n"),
            (
                "dot_gitconfig.tmpl",
                &format!("{{{{ output \"touch\" \"{}\" }}}}x\n", touch("template")),
            ),
            (
                "run_once_x.sh",
                &format!("#!/bin/sh\ntouch '{}'\n", touch("script")),
            ),
            (
                "modify_dot_y",
                &format!("#!/bin/sh\ntouch '{}'\n", touch("modify")),
            ),
        ],
    );
    let home = tree_checksum(&env.home());
    exits(
        &env.run(&["import", "--dry-run", &source.to_string_lossy()]),
        3,
    );
    exits(&env.run(&["import", &source.to_string_lossy()]), 3);
    source.check();
    assert_eq!(names(&markers), Vec::<String>::new());
    assert_eq!(
        fs::read(env.repo().join(".bashrc")).unwrap(),
        b"export A=1\n"
    );
    let err = exits(&env.run(&["import", &source.to_string_lossy()]), 1);
    assert!(err.contains("--overlay"), "{err}");
    source.check();
    assert_eq!(tree_checksum(&env.home()), home);
}

/// A chezmoi import whose second write fails (a symlink target longer than `PATH_MAX`) leaves
/// no repo directory and no staging directory beside it and the source unchanged, and the next
/// import works.
#[test]
fn a_failed_chezmoi_import_leaves_no_partial_repo() {
    let mut env = TestEnv::new();
    let long = format!("/{}", "x".repeat(5000));
    stub_chezmoi(&mut env, &[".a", ".l"], &[(".a", "file\n"), (".l", &long)]);
    let source = source_tree(&env, "cz", &[("dot_a", "file\n"), ("symlink_dot_l", &long)]);
    let data = names(&env.xdg_data());
    let err = exits(&env.run(&["import", &source.to_string_lossy()]), 1);
    assert!(err.contains("cannot create the symlink .l"), "{err}");
    source.check();
    assert!(!env.repo().exists(), "a partial repo was left behind");
    assert_eq!(
        names(&env.xdg_data()),
        data,
        "a staging directory was left behind"
    );

    fs::remove_file(source.join("symlink_dot_l")).unwrap();
    source.seal();
    exits(&env.run(&["import", &source.to_string_lossy()]), 0);
    assert_eq!(fs::read(env.repo().join(".a")).unwrap(), b"file\n");
}

/// Importing a symlink farm, then swapping its links for files, leaves every file of the farm
/// unchanged, and a refused second import changes nothing.
#[test]
fn a_farm_import_and_swap_leave_the_farm_unchanged() {
    let env = TestEnv::new();
    stow(&env);
    let farm = common::Source::new(env.home().join("dotfiles"));
    exits(&env.run(&["import", "--dry-run"]), 0);
    farm.check();
    exits(&env.run(&["import"]), 0);
    farm.check();
    let err = exits(&env.run(&["import"]), 1);
    assert!(err.contains("--overlay"), "{err}");
    farm.check();
    exits(&env.run(&["import", "--swap"]), 0);
    farm.check();
    assert!(
        fs::symlink_metadata(env.home().join(".zshrc"))
            .unwrap()
            .is_file()
    );
}

/// `import --overlay` of the legacy repo, with and without `--include-missing`, in dry-run
/// mode and refused for a dirty repo, leaves the legacy fixture unchanged.
#[test]
fn an_overlay_import_leaves_its_source_unchanged() {
    let fixture = legacy_fixture();
    let env = TestEnv::new();
    let source = overlay_source(&env, 0o644, b"# bashrc v2\n");
    env.ok(&["init", &source.to_string_lossy(), "--take-repo"]);
    exits(
        &env.run(&["import", "--dry-run", "--overlay", LEGACY_REPO]),
        3,
    );
    fixture.check();
    exits(&env.run(&["import", "--overlay", LEGACY_REPO]), 3);
    fixture.check();
    exits(
        &env.run(&["import", "--overlay", LEGACY_REPO, "--include-missing"]),
        3,
    );
    fixture.check();

    env.write_repo("stray", b"uncommitted\n");
    let err = exits(&env.run(&["import", "--overlay", LEGACY_REPO]), 1);
    assert!(err.contains("has uncommitted changes"), "{err}");
    fixture.check();
}

// System root confinement

/// `add --system` of a key outside `[system] roots` is refused with nothing recorded, and a plan
/// the helper is given for a path outside the roots, or with roots that differ from the machine
/// config, is refused whole: no file is written under the system root.
#[test]
fn a_system_path_outside_the_roots_is_refused() {
    let env = system_setup_none(&["/etc"]);
    env.write_system("/opt/x.conf", b"x\n", 0o644);
    env.write_system("/etc/keep.conf", b"keep\n", 0o644);
    let before = tree_checksum(&env.system_root());
    let repo = tree_checksum(&env.repo());
    let out = env.run(&["add", "--system", "/opt/x.conf"]);
    assert_ne!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(tree_checksum(&env.repo()), repo);

    let plan = plan_json(
        &env,
        &["/etc"],
        &[
            plan_entry("/etc/new.conf", b"new\n", "0644"),
            plan_entry("/opt/x.conf", b"evil\n", "0644"),
        ],
    );
    let out = system_write(&env, &write_plan(&env, &plan));
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("outside"));

    let plan = plan_json(
        &env,
        &["/etc", "/opt"],
        &[plan_entry("/opt/y.conf", b"y\n", "0644")],
    );
    let out = system_write(&env, &write_plan(&env, &plan));
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("differ from [system] roots"),
        "{}",
        stderr(&out)
    );
    assert_eq!(tree_checksum(&env.system_root()), before);
}

/// A symlinked directory below `DOTMAN_SYSTEM_ROOT` is refused by `add --system`, by the helper
/// and by `apply --system` after a managed directory is swapped for a link: the directory the
/// link points at stays unchanged.
#[test]
fn a_symlinked_component_below_the_system_root_is_refused() {
    let env = system_setup_none(&["/etc"]);
    env.write_system("/elsewhere/y.conf", b"y\n", 0o644);
    fs::create_dir_all(env.system_path("/etc")).unwrap();
    symlink(env.system_path("/elsewhere"), env.system_path("/etc/sub")).unwrap();
    let err = exits(&env.run(&["add", "--system", "/etc/sub/y.conf"]), 1);
    assert!(err.contains("symlink"), "{err}");

    let plan = plan_json(
        &env,
        &["/etc"],
        &[plan_entry("/etc/sub/z.conf", b"z\n", "0644")],
    );
    let out = system_write(&env, &write_plan(&env, &plan));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("symlink"));

    env.write_system(KEYD, b"old\n", 0o644);
    env.ok(&["add", "--system", KEYD]);
    env.write_repo(".dotman/system/etc/keyd/default.conf", b"new\n");
    fs::remove_dir_all(env.system_path("/etc/keyd")).unwrap();
    symlink(env.system_path("/elsewhere"), env.system_path("/etc/keyd")).unwrap();
    let elsewhere = tree_checksum(&env.system_path("/elsewhere"));
    let out = env.run(&["apply", "--system", "--yes", "--take-repo"]);
    assert_ne!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(tree_checksum(&env.system_path("/elsewhere")), elsewhere);
    assert!(!env.system_path("/elsewhere/default.conf").exists());
}

/// With `DOTMAN_SYSTEM_ROOT` set, `add --system` reads only below it and `apply --system` writes
/// only below it: a file the host has but the system root lacks is not found, and a key written
/// below the root does not appear at its real path.
#[test]
fn the_system_root_is_the_only_place_system_files_are_read_and_written() {
    let env = system_setup_none(&["/etc"]);
    let host = fs::read("/etc/hostname").ok();
    let err = exits(&env.run(&["add", "--system", "/etc/hostname"]), 1);
    assert!(err.contains("does not exist"), "{err}");
    assert_eq!(env.porcelain("list"), []);

    let real = Path::new("/etc/dotman-security-test.conf");
    env.write_system("/etc/dotman-security-test.conf", b"old\n", 0o644);
    env.ok(&["add", "--system", "/etc/dotman-security-test.conf"]);
    env.write_repo(".dotman/system/etc/dotman-security-test.conf", b"new\n");
    exits(&env.run(&["apply", "--system", "--yes"]), 0);
    assert_eq!(
        fs::read(env.system_path("/etc/dotman-security-test.conf")).unwrap(),
        b"new\n"
    );
    assert!(!real.exists(), "{} was written for real", real.display());
    assert_eq!(fs::read("/etc/hostname").ok(), host);
}

/// A user other than root is refused by the helper when `escalate` is anything but `none`, and
/// a `/` root is never used: no file is written under the system root in either case.
#[test]
fn the_helper_refuses_a_user_unless_escalate_is_none() {
    if is_root() {
        return;
    }
    let env = system_setup(&[]);
    common::configure_machine(&env, &common::system_config(&["/etc"], "sudo"));
    fs::create_dir_all(env.system_path("/etc")).unwrap();
    let plan = plan_json(
        &env,
        &["/etc"],
        &[plan_entry("/etc/x.conf", b"x\n", "0644")],
    );
    let out = system_write(&env, &write_plan(&env, &plan));
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("escalate"), "{}", stderr(&out));

    common::configure_machine(&env, &common::system_config(&["/"], "none"));
    env.write_system("/etc/y.conf", b"y\n", 0o644);
    let before = tree_checksum(&env.system_root());
    let out = env.run(&["add", "--system", "/etc/y.conf"]);
    assert_ne!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(tree_checksum(&env.system_root()), before);
}

// Secrets

const FIRST: &[u8] = b"oauth_token: gho_first_plaintext_marker\n";
const SECOND: &[u8] = b"oauth_token: gho_second_plaintext_marker\n";

fn remote_history(env: &TestEnv) -> String {
    env.git(&[
        "--git-dir",
        &env.remote_path().to_string_lossy(),
        "log",
        "--all",
        "-p",
        "--format=",
    ])
}

/// `add` of a plain file that holds `AGE-SECRET-KEY-` is refused with exit 1, with and without
/// `--yes`, alone or among other paths; the key reaches neither the repo nor the list of managed
/// files.
#[test]
fn add_refuses_a_plain_file_that_holds_an_age_secret_key() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let keys = ".config/sops/age/keys.txt";
    env.write(keys, format!("# created\n{FAKE_KEY}\n").as_bytes(), 0o600);
    env.write(".bashrc", b"# rc\n", 0o644);
    for args in [
        &["add", keys][..],
        &["add", "--yes", keys],
        &["add", ".bashrc", keys],
    ] {
        let err = exits(&env.run(args), 1);
        assert!(err.contains("AGE-SECRET-KEY-"), "{args:?}: {err}");
    }
    assert_eq!(
        files_containing(&env.repo(), b"AGE-SECRET-KEY-"),
        Vec::<String>::new()
    );
    assert!(!env.repo().join(keys).exists());
    let listed: Vec<String> = env
        .porcelain("list")
        .into_iter()
        .map(|(_, key)| key)
        .collect();
    assert!(!listed.iter().any(|key| key == keys), "{listed:?}");
}

/// A file that holds `AGE-SECRET-KEY-` and appears under a managed directory, or in a managed
/// file after an edit, is `held` by `sync` capture: sync exits 3, and the key reaches neither the
/// repo worktree, any commit, nor the remote.
#[test]
fn sync_holds_a_plain_file_that_holds_an_age_secret_key() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".config/app/a.conf", b"a\n", 0o644);
    env.ok(&["add", ".config"]);
    env.ok(&["sync"]);

    let keys = ".config/app/keys.txt";
    env.write(keys, format!("{FAKE_KEY}\n").as_bytes(), 0o600);
    env.write(
        ".config/app/a.conf",
        format!("a\n{FAKE_KEY}\n").as_bytes(),
        0o644,
    );
    assert_eq!(env.token_of("status", keys).as_deref(), Some("held"));
    for _ in 0..2 {
        exits(&env.run(&["sync"]), 3);
    }
    assert_eq!(
        files_containing(&env.repo(), b"AGE-SECRET-KEY-"),
        Vec::<String>::new()
    );
    let repo = env.repo().to_string_lossy().into_owned();
    let history = env.git(&["-C", &repo, "log", "--all", "-p", "--format="]);
    assert!(!history.contains("AGE-SECRET-KEY-"));
    assert!(!remote_history(&env).contains("AGE-SECRET-KEY-"));
    assert_eq!(env.git(&["-C", &repo, "status", "--porcelain"]), "");
}

// Credential content guard

/// A GitHub classic token shape, built here so no token-shaped literal sits in the source.
fn github_token() -> String {
    format!("ghp_{}", "a1B2".repeat(9))
}

fn npm_token() -> String {
    format!("npm_{}", "Zy9x".repeat(9))
}

fn remote_tree(env: &TestEnv) -> String {
    let remote = env.remote_path().to_string_lossy().into_owned();
    env.git(&["--git-dir", &remote, "ls-tree", "-r", "--name-only", "main"])
}

fn allow_entries(env: &TestEnv) -> Vec<(String, String)> {
    env.repo_config()
        .guard
        .allow
        .into_iter()
        .map(|entry| (entry.path, entry.rule))
        .collect()
}

/// `add <file>` of a file that holds a credential exits 1 with the rule and the line, never the
/// value, alone or among other paths, dry run or not, and adds nothing.
#[test]
fn add_refuses_a_named_file_that_holds_a_credential() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let token = github_token();
    let local = ".config/zsh/local.zsh";
    env.write(
        local,
        format!("# local\nexport GH={token}\n").as_bytes(),
        0o600,
    );
    env.write(".bashrc", b"# rc\n", 0o644);
    for args in [
        &["add", local][..],
        &["add", "--dry-run", local],
        &["add", ".bashrc", local],
    ] {
        let err = exits(&env.run(args), 1);
        assert!(
            err.contains(
                ".config/zsh/local.zsh: line 2 holds a github-token; --encrypt stores it encrypted, --allow-plain stores it as is"
            ),
            "{args:?}: {err}"
        );
        assert!(!err.contains(&token), "the value is never printed");
    }
    assert_eq!(env.porcelain("list"), []);
    assert_eq!(
        files_containing(&env.repo(), token.as_bytes()),
        Vec::<String>::new()
    );
}

/// `add --allow-plain` stores the file and records `[guard] allow` by key and rule; later edits,
/// which move the line, sync without a hold, while a credential of another rule is held.
#[test]
fn allow_plain_is_recorded_by_path_and_rule_never_by_line() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    let (token, npm) = (github_token(), npm_token());
    let local = ".config/zsh/local.zsh";
    env.write(local, format!("export GH={token}\n").as_bytes(), 0o600);
    let err = exits(&env.run(&["add", "--allow-plain", local]), 0);
    assert!(
        err.contains("line 1 holds a github-token; stored as it is"),
        "{err}"
    );
    assert!(!err.contains(&token), "{err}");
    assert_eq!(
        allow_entries(&env),
        [(local.to_owned(), "github-token".to_owned())]
    );
    exits(&env.run(&["sync"]), 0);
    assert!(remote_tree(&env).contains(local));

    env.write(
        local,
        format!("# moved down\n\nexport GH={token}\n").as_bytes(),
        0o600,
    );
    exits(&env.run(&["sync"]), 0);
    assert!(remote_history(&env).contains("# moved down"));

    env.write(
        local,
        format!("# moved down\n\nexport GH={token}\nexport NPM={npm}\n").as_bytes(),
        0o600,
    );
    let err = exits(&env.run(&["sync"]), 3);
    assert!(
        err.contains(".config/zsh/local.zsh: held, line 4 holds a npm-token"),
        "{err}"
    );
    assert!(!err.contains(&npm) && !err.contains(&token), "{err}");
    assert!(!remote_history(&env).contains(&npm));
    assert!(
        !fs::read_to_string(env.repo().join(local))
            .unwrap()
            .contains(&npm)
    );
}

/// No flag and no `[guard] allow` entry lets an age identity in as plain text.
#[test]
fn an_age_secret_key_is_never_overridable() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    let keys = ".config/app/keys.txt";
    env.write(keys, format!("{FAKE_KEY}\n").as_bytes(), 0o600);
    let err = exits(&env.run(&["add", "--allow-plain", keys]), 1);
    assert!(
        err.contains("line 1 holds an age secret key (AGE-SECRET-KEY-), which dotman never stores in plain text"),
        "{err}"
    );

    env.write(".config/app/a.conf", b"a\n", 0o644);
    exits(
        &env.run(&["add", "--allow-plain", "--yes", ".config/app"]),
        3,
    );
    let mut config = env.repo_config();
    config.guard.allow.push(dotman::repo_config::Allow {
        path: keys.to_owned(),
        rule: "age-secret-key".to_owned(),
    });
    config
        .save(&env.repo().join(".dotman.toml"), false)
        .unwrap();
    assert_eq!(env.token_of("status", keys).as_deref(), Some("held"));
    exits(&env.run(&["sync"]), 3);
    assert!(!remote_history(&env).contains("AGE-SECRET-KEY-"));
}

/// The age key rule needs the whole key shape: an alias that names the `AGE-SECRET-KEY-` prefix
/// is managed and pushed like any file, and a real key from `age-keygen` is held.
#[test]
fn only_a_whole_age_key_is_held_and_a_real_one_is() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    let alias = b"alias agekeys='grep AGE-SECRET-KEY- ~/.config/age/keys.txt'\n";
    env.write(".config/app/aliases.zsh", alias, 0o644);
    exits(&env.run(&["add", "--yes", ".config/app"]), 0);
    exits(&env.run(&["sync"]), 0);
    assert!(remote_tree(&env).contains(".config/app/aliases.zsh"));

    if !age_installed() {
        return;
    }
    let generated = env.root().join("generated.txt");
    let out = std::process::Command::new("age-keygen")
        .arg("-o")
        .arg(&generated)
        .output()
        .unwrap();
    assert!(out.status.success(), "age-keygen failed");
    let key = fs::read_to_string(&generated).unwrap();
    fs::remove_file(&generated).unwrap();
    let secret = key
        .lines()
        .find(|line| line.starts_with("AGE-SECRET-KEY-1"))
        .unwrap()
        .to_owned();
    env.write(".config/app/notes.txt", key.as_bytes(), 0o600);
    assert_eq!(
        env.token_of("status", ".config/app/notes.txt").as_deref(),
        Some("held")
    );
    let err = exits(&env.run(&["sync"]), 3);
    assert!(
        err.contains("line 3 holds an age secret key"),
        "the reason is named"
    );
    assert!(!err.contains(&secret), "the key is never printed");
    assert!(!remote_history(&env).contains(&secret));
    assert!(!env.repo().join(".config/app/notes.txt").exists());
}

/// `add <dir>` holds each file that holds a credential, lists it with the commands that resolve
/// it, adds the rest and exits 3; sync holds it too, and `add --allow-plain <dir>` takes it.
#[test]
fn a_directory_holds_a_file_that_holds_a_credential() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    let pat = format!("glpat-{}", "Q7w".repeat(8));
    env.write(".config/app/a.conf", b"a\n", 0o644);
    env.write(
        ".config/app/remote.conf",
        format!("token={pat}\n").as_bytes(),
        0o600,
    );
    let err = exits(&env.run(&["add", "--yes", ".config/app"]), 3);
    let path = env.home().join(".config/app/remote.conf");
    assert!(
        err.contains(&format!(
            "held .config/app/remote.conf: line 1 holds a gitlab-pat; `dotman add --encrypt {}` stores it encrypted",
            path.display()
        )),
        "{err}"
    );
    assert!(!err.contains(&pat), "{err}");
    let listed: Vec<String> = env.porcelain("list").into_iter().map(|(_, k)| k).collect();
    assert_eq!(listed, [".config/app/a.conf"]);
    exits(&env.run(&["sync"]), 3);
    assert!(!remote_history(&env).contains(&pat));

    exits(
        &env.run(&["add", "--allow-plain", "--yes", ".config/app"]),
        0,
    );
    assert_eq!(
        allow_entries(&env),
        [(
            ".config/app/remote.conf".to_owned(),
            "gitlab-pat".to_owned()
        )]
    );
    exits(&env.run(&["sync"]), 0);
    assert!(remote_tree(&env).contains(".config/app/remote.conf"));
}

/// A local edit that adds a credential to a managed file is held by the sync capture, at every
/// sync: exit 3, the repo copy unchanged, nothing pushed.
#[test]
fn a_local_edit_that_adds_a_credential_is_held() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".config/app/a.conf", b"a\n", 0o644);
    env.ok(&["add", ".config/app/a.conf"]);
    env.ok(&["sync"]);
    let slack = format!("xoxb-123456789012-{}", "AbC1".repeat(6));
    env.write(
        ".config/app/a.conf",
        format!("a\nhook = {slack}\n").as_bytes(),
        0o644,
    );
    for _ in 0..2 {
        let err = exits(&env.run(&["sync"]), 3);
        assert!(
            err.contains(".config/app/a.conf: held, line 2 holds a slack-token"),
            "{err}"
        );
        assert!(!err.contains(&slack), "{err}");
    }
    assert_eq!(
        fs::read(env.repo().join(".config/app/a.conf")).unwrap(),
        b"a\n"
    );
    assert!(!remote_history(&env).contains(&slack));
}

/// A credential written into the repo itself, as `dotman edit` or a hand edit does, reaches no
/// capture; the sync staging guard keeps it out of the commit and names the rule and the line.
#[test]
fn the_staging_guard_keeps_a_credential_out_of_the_commit() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".a", b"a\n", 0o644);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    let token = github_token();
    let editor = env.write_script(
        "ed.sh",
        &format!("#!/bin/sh\necho 'GH={token}' >> \"$1\"\n"),
    );
    let editor = editor.to_string_lossy().into_owned();
    env.ok_with(&[("EDITOR", &editor), ("VISUAL", "")], &["edit", ".a"]);
    let err = exits(&env.run(&["sync"]), 3);
    assert!(
        err.contains("\n  .a: line 2 holds a github-token\n"),
        "{err}"
    );
    assert!(!err.contains(&token), "{err}");
    assert!(!remote_history(&env).contains(&token));
}

/// Every importer leaves out a file that holds a credential, reports it as needing action and
/// exits 3: a git tree's conversion commit removes it, and chezmoi and farm imports never write it.
#[test]
fn imports_leave_out_files_that_hold_a_credential() {
    let token = github_token();
    let text = format!("export GH={token}\n");

    let env = TestEnv::new();
    let source = source_tree(&env, "dots", &[(".bashrc", "# rc\n"), (".zlocal", &text)]);
    common::git_in(&env, &source, &["init", "-q"]);
    common::git_in(&env, &source, &["add", "-A"]);
    common::git_in(&env, &source, &["commit", "-q", "-m", "dots"]);
    source.seal();
    let err = exits(&env.run(&["import", &source.to_string_lossy()]), 3);
    assert!(
        err.contains(".zlocal: line 1 holds a github-token"),
        "{err}"
    );
    assert!(!err.contains(&token), "{err}");
    let repo = env.repo().to_string_lossy().into_owned();
    let tree = env.git(&["-C", &repo, "ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(
        tree.contains(".bashrc") && !tree.contains(".zlocal"),
        "{tree}"
    );
    assert!(!env.repo().join(".zlocal").exists());

    if have_chezmoi() {
        let env = TestEnv::new();
        let source = source_tree(
            &env,
            "cz",
            &[("dot_bashrc", "# rc\n"), ("dot_zlocal", &text)],
        );
        let err = exits(&env.run(&["import", &source.to_string_lossy()]), 3);
        assert!(
            err.contains(".zlocal: line 1 holds a github-token"),
            "{err}"
        );
        assert!(!env.repo().join(".zlocal").exists());
        assert_eq!(
            files_containing(&env.repo(), token.as_bytes()),
            Vec::<String>::new()
        );
    }

    let env = TestEnv::new();
    stow(&env);
    env.write("dotfiles/zsh/.zlocal", text.as_bytes(), 0o600);
    env.symlink(".zlocal", "dotfiles/zsh/.zlocal");
    let err = exits(&env.run(&["import", "--yes"]), 3);
    assert!(
        err.contains(".zlocal: line 1 holds a github-token"),
        "{err}"
    );
    assert_eq!(
        files_containing(&env.repo(), token.as_bytes()),
        Vec::<String>::new()
    );
}

/// The age identity file named by `[age] identity`, and the file named by `DOTMAN_AGE_KEY_FILE`,
/// are refused by `add`, plain or encrypted, and a directory add skips the identity: nothing is
/// copied.
#[test]
fn add_refuses_the_identity_files() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let own = env.home().join(".age/id.txt");
    env.write(".age/id.txt", FAKE_KEY.as_bytes(), 0o600);
    env.write_machine_config(&format!("[age]\nidentity = \"{}\"\n", own.display()));
    for args in [
        &["add", ".age/id.txt"][..],
        &["add", "--encrypt", ".age/id.txt"],
    ] {
        let out = env.run(args);
        assert_ne!(out.status.code(), Some(0), "{args:?}: {}", stderr(&out));
    }

    let err = exits(&env.run(&["add", "--yes", ".age"]), 0);
    assert!(
        err.contains("dotman's own files are never managed"),
        "{err}"
    );

    env.write(".keys/age.txt", FAKE_KEY.as_bytes(), 0o600);
    let file = env.home().join(".keys/age.txt");
    let vars = [("DOTMAN_AGE_KEY_FILE", file.to_str().unwrap())];
    for args in [
        &["add", ".keys/age.txt"][..],
        &["add", "--encrypt", ".keys/age.txt"],
    ] {
        let err = exits(&env.run_with(&vars, args), 1);
        assert!(
            err.contains("DOTMAN_AGE_KEY_FILE") || err.contains("never manages"),
            "{err}"
        );
    }
    assert_eq!(
        files_containing(&env.repo(), b"AGE-SECRET-KEY-"),
        Vec::<String>::new()
    );
    assert_eq!(env.porcelain("list"), []);
}

/// After `add --encrypt` and `sync`, and a second edit and sync, the plaintext appears in no
/// worktree file of the repo or HOME's copy of the repo, no commit (`git grep` over every
/// revision), and no object of the repo's or the remote's `.git` (`cat-file --batch-all-objects`).
#[test]
fn the_plaintext_of_an_encrypted_file_never_reaches_the_repo() {
    if !age_installed() {
        return;
    }
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.ok(&["key", "init"]);
    env.write(".config/gh/hosts.yml", FIRST, 0o600);
    env.ok(&["add", "--encrypt", ".config/gh/hosts.yml"]);
    env.ok(&["sync"]);
    env.write(".config/gh/hosts.yml", SECOND, 0o600);
    env.ok(&["sync"]);
    assert_eq!(env.read(".config/gh/hosts.yml"), SECOND);

    let repo = env.repo();
    let repo_arg = repo.to_string_lossy().into_owned();
    let remote = env.remote_path();
    assert!(
        repo.join(".config/gh/hosts{age}.yml").is_file(),
        "the ciphertext is in the repo"
    );
    for needle in [
        "gho_first_plaintext_marker",
        "gho_second_plaintext_marker",
        "oauth_token",
    ] {
        assert_eq!(
            files_containing(&repo, needle.as_bytes()),
            Vec::<String>::new(),
            "{needle}"
        );
        let revs = env.git(&["-C", &repo_arg, "rev-list", "--all"]);
        let mut grep = vec!["-C", &repo_arg, "grep", "-l", "-e", needle];
        grep.extend(revs.split_whitespace());
        let found = env.git_raw(&grep);
        assert_eq!(found.status.code(), Some(1), "{needle} in a commit");
        for git_dir in [repo.join(".git"), remote.clone()] {
            let dir = git_dir.to_string_lossy().into_owned();
            let objects = env.git_raw(&[
                "--git-dir",
                &dir,
                "cat-file",
                "--batch-all-objects",
                "--batch",
            ]);
            assert!(objects.status.success());
            assert!(
                !objects
                    .stdout
                    .windows(needle.len())
                    .any(|w| w == needle.as_bytes()),
                "{needle} in an object of {dir}"
            );
        }
    }
    assert!(!remote_history(&env).contains("oauth_token"));
}

// Bootstrap

/// `install.sh` in the repo is never a managed file: `status` and `list` do not show it and
/// `apply` does not write `~/install.sh`, whether it is the generated script or an edited one.
#[test]
fn install_sh_is_never_applied() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    assert!(env.repo().join("install.sh").is_file());
    env.write(".bashrc", b"x\n", 0o644);
    env.ok(&["add", ".bashrc"]);
    for edited in [false, true] {
        if edited {
            fs::write(
                env.repo().join("install.sh"),
                b"#!/bin/sh\ntouch /tmp/never\n",
            )
            .unwrap();
        }
        for command in ["status", "list"] {
            let keys: Vec<String> = env
                .porcelain(command)
                .into_iter()
                .map(|(_, key)| key)
                .collect();
            assert_eq!(keys, [".bashrc"], "{command} edited={edited}");
            let human = String::from_utf8_lossy(&env.run(&[command]).stdout).into_owned();
            assert!(!human.contains("install.sh"), "{command}: {human}");
        }
        exits(&env.run(&["apply"]), 0);
        exits(&env.run(&["apply", "--take-repo"]), 0);
        assert!(!env.home().join("install.sh").exists(), "edited={edited}");
    }
}

/// A checksum that does not match the downloaded archive, a missing checksum entry and an
/// archive swapped after its checksum was published each make `install.sh` exit 1 with no `dotman`
/// installed and nothing applied.
#[test]
fn a_bootstrap_checksum_mismatch_installs_nothing() -> R {
    if !curl_available() {
        return Ok(());
    }
    let setup = Setup::new()?;
    let name = archive_name();
    let tampered = setup.machine.root().join("tampered");
    fs::create_dir_all(&tampered)?;
    let good = release()?.join(&name);
    symlink(&good, tampered.join(&name))?;
    let swapped = setup.machine.root().join("swapped");
    fs::create_dir_all(&swapped)?;
    fs::write(swapped.join(&name), b"not the archive the checksum is for")?;
    fs::copy(
        release()?.join("checksums.txt"),
        swapped.join("checksums.txt"),
    )?;
    for (dir, checksums, why) in [
        (
            &tampered,
            Some(format!("{}  {name}\n", "0".repeat(64))),
            "NOT",
        ),
        (&tampered, Some(String::new()), "no entry"),
        (&swapped, None, "swapped archive"),
    ] {
        if let Some(checksums) = checksums {
            fs::write(dir.join("checksums.txt"), checksums)?;
        }
        let out = setup.command(dir, &[]).output()?;
        assert_eq!(out.status.code(), Some(1), "{why}: {}", text(&out));
        assert!(!setup.installed(), "{why}");
        assert!(!setup.applied(), "{why}");
        assert!(
            !setup.home().join(".local/bin").exists()
                || names(&setup.home().join(".local/bin")).is_empty(),
            "{why}"
        );
    }
    Ok(())
}

// Hostile repos

/// A repo file whose variant block leaves `..` (`..{os=linux}`, also below a directory) is
/// invalid: `status` says so, `apply` and `sync` exit 3 and write nothing, neither at HOME's
/// parent nor in HOME.
#[test]
fn a_repo_file_named_for_dotdot_is_invalid_and_never_applied() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo("..{os=linux}", b"evil\n");
    env.write_repo(".config/..{os=linux}", b"evil\n");
    env.write_repo(".config/..{host=nowhere}", b"evil\n");
    let root = tree_checksum(env.root());
    let tokens = env.porcelain("status");
    for key in ["..{os=linux}", ".config/..{os=linux}"] {
        assert!(
            tokens.contains(&("invalid".to_owned(), key.to_owned())),
            "{tokens:?}"
        );
    }
    exits(&env.run(&["apply"]), 3);
    exits(&env.run(&["apply", "--take-repo"]), 3);
    exits(&env.run(&["sync"]), 3);
    let home = names(&env.home());
    assert_eq!(home, [".gitconfig"]);
    let after = tree_checksum(env.root());
    let changed: Vec<&String> = after
        .keys()
        .filter(|path| !path.starts_with("xdg/"))
        .filter(|path| root.get(*path) != after.get(*path))
        .collect();
    assert_eq!(changed, Vec::<&String>::new());
}

/// A repo path with a `.git` component, planted in the worktree because git refuses to track
/// one, is never listed, applied or committed: `~/.config/x/.git` does not exist after `apply`
/// and `sync`, while its sibling files apply, and `add` of such a path is refused. `git` itself
/// rejects a `.GIT` path, so `sync` is not run with one planted.
#[test]
fn a_repo_path_with_a_git_component_is_never_applied() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write_repo(".config/x/real", b"ok\n");
    env.write_repo(
        ".config/x/.git/config",
        b"[core]\n\tfsmonitor = touch /tmp/pwned\n",
    );
    env.write_repo(
        ".config/y/.GIT/hooks/pre-commit",
        b"#!/bin/sh\ntouch /tmp/pwned\n",
    );
    env.write_repo(".config/z/.github/ci.yml", b"on: push\n");

    let keys: Vec<String> = env
        .porcelain("list")
        .into_iter()
        .map(|(_, key)| key)
        .collect();
    assert_eq!(keys, [".config/x/real", ".config/z/.github/ci.yml"]);
    exits(&env.run(&["apply"]), 0);
    assert!(!env.home().join(".config/y").exists());
    fs::remove_dir_all(env.repo().join(".config/y")).unwrap();
    exits(&env.run(&["sync"]), 0);
    assert_eq!(env.read(".config/x/real"), b"ok\n");
    assert_eq!(env.read(".config/z/.github/ci.yml"), b"on: push\n");
    assert!(!env.home().join(".config/x/.git").exists());

    env.write(".config/w/.git/config", b"[core]\n", 0o644);
    let err = exits(&env.run(&["add", ".config/w/.git/config"]), 1);
    assert!(err.contains("`.git` component"), "{err}");
}

/// Hooks planted in the dotman repo's `.git/hooks`, and in a template directory that git copies
/// into every new repo, never run during `init <path>`, `init <url>`, `add`, `sync` (commit,
/// pull and push), `apply` or `restore`: the marker file stays absent.
#[test]
fn planted_git_hooks_never_run() {
    let a = TestEnv::new();
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write(".bashrc", b"one\n", 0o644);
    a.ok(&["add", ".bashrc"]);
    a.ok(&["sync"]);
    a.git(&[
        "--git-dir",
        &a.remote_path().to_string_lossy(),
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    let marker = a.root().join("hook-marker");

    let b = a.second_device();
    let template = b.root().join("template/hooks");
    common::plant_git_hooks(&template, &marker);
    let templated = [(
        "GIT_TEMPLATE_DIR",
        b.root().join("template").to_string_lossy().into_owned(),
    )];
    let vars: Vec<(&str, &str)> = templated.iter().map(|(k, v)| (*k, v.as_str())).collect();
    exits(&b.run_with(&vars, &["init", &a.remote_url()]), 0);
    common::plant_git_hooks(&b.repo().join(".git/hooks"), &marker);
    assert_eq!(b.read(".bashrc"), b"one\n");

    b.write(".zshrc", b"zsh\n", 0o644);
    exits(&b.run_with(&vars, &["add", ".zshrc"]), 0);
    a.write(".bashrc", b"two\n", 0o644);
    a.ok(&["sync"]);
    exits(&b.run_with(&vars, &["sync"]), 0);
    assert_eq!(b.read(".bashrc"), b"two\n");
    exits(&b.run_with(&vars, &["apply"]), 0);
    exits(
        &b.run_with(&vars, &["restore", ".bashrc", "--from", "HEAD~1"]),
        0,
    );

    let c = TestEnv::new();
    common::plant_git_hooks(&a.repo().join(".git/hooks"), &marker);
    let path = a.repo().to_string_lossy().into_owned();
    exits(&c.run_with(&vars, &["init", &path]), 0);
    assert_eq!(c.read(".bashrc"), b"two\n");
    assert!(
        !marker.exists(),
        "{}",
        fs::read_to_string(&marker).unwrap_or_default()
    );
}

/// `DOTMAN_AGE_KEY` is in no hook's environment and no editor's: both write their `env` to a
/// file that lacks the variable and has `HOME`.
#[test]
fn the_age_key_never_reaches_a_hook_or_the_editor() {
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
    exits(
        &env.run_with(
            &[("DOTMAN_AGE_KEY", FAKE_KEY)],
            &["apply", "--approve-hooks"],
        ),
        0,
    );
    let seen = fs::read_to_string(&hook_env).unwrap();
    assert!(
        seen.contains("HOME=") && !seen.contains("DOTMAN_AGE_KEY") && !seen.contains(FAKE_KEY),
        "{seen}"
    );

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
    assert!(
        seen.contains("HOME=") && !seen.contains("DOTMAN_AGE_KEY") && !seen.contains(FAKE_KEY),
        "{seen}"
    );
}

/// A template reads only the variables in `[template] env`: an unlisted one fails the file and
/// its value is never applied, `DOTMAN_AGE_KEY` is refused in the list, and it is unreadable
/// even when it is set.
#[test]
fn templates_read_only_the_listed_environment() {
    let env = template_env("token {{ env.SECRET }}\n");
    let secret = [("SECRET", "hunter2")];
    let err = exits(&env.run_with(&secret, &["apply"]), 1);
    assert!(err.contains("env.SECRET"), "{err}");
    assert!(!env.home().join(".config/app/conf").exists());

    let env = template_env("key {{ env.DOTMAN_AGE_KEY }}\n");
    let key = [("DOTMAN_AGE_KEY", FAKE_KEY)];
    let err = exits(&env.run_with(&key, &["apply"]), 1);
    assert!(!err.contains(FAKE_KEY), "{err}");
    assert!(!env.home().join(".config/app/conf").exists());

    env.write_machine_config("[template]\nenv = [\"HOME\", \"DOTMAN_AGE_KEY\"]\n");
    for args in [&["status"][..], &["apply"]] {
        let err = exits(&env.run_with(&key, args), 1);
        assert!(
            err.contains("`[template] env` lists DOTMAN_AGE_KEY"),
            "{err}"
        );
        assert!(!err.contains(FAKE_KEY), "{err}");
    }
    assert!(!env.home().join(".config/app/conf").exists());
}

/// The repo path is a symlink to `~/dotfiles`, `$HOME` is itself a git repo with a remote, and the
/// dotman repo has lost `.git/HEAD`. Every command fails naming the missing HEAD, and the outer
/// repo's HEAD, index and remote are as they were: git never searches above the dotman repo.
#[test]
fn a_broken_repo_behind_a_symlinked_path_never_reaches_a_home_repo() {
    let env = TestEnv::new();
    let home = env.home().to_string_lossy().into_owned();
    let outer_remote = env.root().join("outer.git").to_string_lossy().into_owned();
    env.git(&["init", "-q", "--bare", &outer_remote]);
    env.write(".bashrc", b"home rc\n", 0o644);
    env.git(&["-C", &home, "init", "-q"]);
    env.git(&["-C", &home, "add", ".bashrc"]);
    env.git(&["-C", &home, "commit", "-q", "-m", "outer"]);
    env.git(&["-C", &home, "remote", "add", "origin", &outer_remote]);
    env.git(&["-C", &home, "push", "-q", "origin", "HEAD:refs/heads/main"]);
    fs::create_dir(env.home().join("dotfiles")).unwrap();
    fs::create_dir_all(env.xdg_data()).unwrap();
    symlink(env.home().join("dotfiles"), env.repo()).unwrap();
    env.ok(&["init", "--no-import", "--remote", &env.remote_url()]);
    env.ok(&["add", ".bashrc"]);
    env.ok(&["sync"]);

    fs::remove_file(env.home().join("dotfiles/.git/HEAD")).unwrap();
    env.write(".bashrc", b"home rc, edited\n", 0o644);
    let outer = |args: &[&str]| {
        let mut full = vec!["-C", home.as_str()];
        full.extend_from_slice(args);
        env.git(&full)
    };
    let head = outer(&["rev-parse", "HEAD"]);
    let index = fs::read(env.home().join(".git/index")).unwrap();
    let published = env.git(&["ls-remote", &outer_remote]);
    for args in [&["sync"][..], &["status"], &["apply"], &["add", ".bashrc"]] {
        let err = exits(&env.run(args), 1);
        assert!(
            err.contains(".git/HEAD is missing or corrupt"),
            "{args:?}: {err}"
        );
    }
    assert_eq!(outer(&["rev-parse", "HEAD"]), head);
    assert_eq!(fs::read(env.home().join(".git/index")).unwrap(), index);
    assert_eq!(env.git(&["ls-remote", &outer_remote]), published);
    assert_eq!(outer(&["stash", "list"]), "");
}

// Network and gh

/// `init --create-github` with the harness's refusing `gh` exits 1 and creates nothing, and a
/// `--remote https://…` URL is recorded with its visibility `unknown` while git, traced, starts
/// no HTTP transport: the harness's `insteadOf` rewrite sends every https URL to a missing local path.
#[test]
fn gh_and_the_network_are_never_reached() {
    let env = TestEnv::new();
    let err = exits(&env.run(&["init", "--create-github", "me/dotfiles"]), 1);
    assert!(err.contains("gh auth login"), "{err}");
    assert!(!env.repo().exists());
    assert!(!env.state_dir().join("state.toml").exists());

    let out = env.run_with(
        &[("GIT_TRACE", "1")],
        &["init", "--remote", "https://example.invalid/me/dots.git"],
    );
    let err = exits(&out, 0);
    assert!(
        !err.contains("remote-http"),
        "git started an HTTP transport: {err}"
    );
    let state = dotman::state::State::read(&env.state_dir().join("state.toml"))
        .unwrap()
        .unwrap();
    assert_eq!(
        state.remote_public,
        Some(dotman::state::RemotePublic::Unknown)
    );

    let out = env.run_with(&[("GIT_TRACE", "1")], &["sync"]);
    let err = stderr(&out);
    assert!(
        !err.contains("remote-http"),
        "git started an HTTP transport: {err}"
    );
}

// Silent file loss

/// Two index keys that differ only in case make `apply` and `sync` exit 1 before anything is
/// written or staged, so a case-insensitive checkout never stages the lost twin's deletion.
#[test]
fn colliding_keys_stop_apply_and_sync_before_any_write() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let repo = env.repo().to_string_lossy().into_owned();
    let blob = env.git(&["-C", &repo, "hash-object", "-w", "/dev/null"]);
    for key in ["x/A.conf", "x/a.conf"] {
        let info = format!("100644,{},{key}", blob.trim());
        env.git(&["-C", &repo, "update-index", "--add", "--cacheinfo", &info]);
    }
    let before = env.four_trees();
    for args in [["apply"], ["sync"]] {
        let err = exits(&env.run(&args), 1);
        assert!(err.contains("x/A.conf and x/a.conf"), "{err}");
    }
    assert_eq!(env.four_trees(), before);
}
