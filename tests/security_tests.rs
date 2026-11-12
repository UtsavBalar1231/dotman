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
    git_in, have_chezmoi, legacy_fixture, mode_of, overlay_source, plan_entry, plan_json, release,
    source_tree, stow, stub_chezmoi, system_setup, system_setup_none, system_write, text,
    tree_checksum, write_plan, yadm_fixture,
};
use dotman::paths::{Env, Paths, ensure_not_root};
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::fs;
use std::os::unix::ffi::OsStrExt;
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

/// An upstream file whose name holds a C1 control character, such as the 8-bit CSI U+009B that
/// some terminals read as ESC `[`, is no key: a clone applies the rest and never writes it into
/// HOME, and nothing prints the character raw.
#[test]
fn an_upstream_name_with_a_c1_control_is_never_applied() {
    let a = TestEnv::new();
    a.ok(&["init"]);
    a.write_repo(".config/app/ok.conf", b"ok\n");
    a.write_repo(".config/app/csi\u{9b}2J.conf", b"planted\n");
    a.push_repo();

    let b = a.second_device();
    let out = b.run(&["init", &a.remote_url(), "--take-repo"]);
    let err = exits(&out, 0);
    assert_eq!(b.read(".config/app/ok.conf"), b"ok\n");
    assert_eq!(names(&b.home().join(".config/app")), ["ok.conf"]);
    for args in [&["status"][..], &["list"], &["sync"]] {
        let out = b.run(args);
        let all = format!("{err}{}", text(&out));
        assert!(!all.contains('\u{9b}'), "{args:?}: {all:?}");
    }
    assert_eq!(names(&b.home().join(".config/app")), ["ok.conf"]);
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

/// The machine config's `[guard] allow`, where `--allow-plain` records entries.
fn allow_entries(env: &TestEnv) -> Vec<(String, String)> {
    let text = fs::read_to_string(env.xdg_config().join("dotman/config.toml")).unwrap_or_default();
    dotman::config::Config::parse(&text)
        .map(|(config, _)| config)
        .unwrap_or_default()
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
                ".config/zsh/local.zsh: line 2 holds a github-token; --encrypt stores it encrypted; or remove the secret from the file"
            ),
            "{args:?}: {err}"
        );
        assert!(!err.contains("--allow-plain"), "{args:?}: {err}");
        assert!(!err.contains(&token), "the value is never printed");
    }
    assert_eq!(env.porcelain("list"), []);
    assert_eq!(
        files_containing(&env.repo(), token.as_bytes()),
        Vec::<String>::new()
    );
}

/// A file that `add <dir>` holds by its name is scanned too. When it holds a credential, the hint
/// names the finding and offers `--encrypt` only, never `--allow-plain`, and running every command
/// the run printed, as printed, never sends the token to the public remote.
#[test]
fn a_name_hold_hint_never_offers_allow_plain_for_a_file_that_holds_a_credential() {
    let env = TestEnv::new();
    let origin = env.remote_url();
    env.ok(&["init", "--remote", &origin]);
    let state_file = env.state_dir().join("state.toml");
    let mut state = dotman::state::State::read(&state_file)
        .expect("read state")
        .expect("state");
    state.remote_public = Some(dotman::state::RemotePublic::True);
    state.remote_url = Some(origin);
    state.remote_checked_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs());
    state.save(&state_file, false).expect("save state");

    let token = github_token();
    env.write(".config/zsh/.zshrc", b"source secrets.zsh\n", 0o644);
    let secrets = ".config/zsh/secrets.zsh";
    env.write(
        secrets,
        format!("# tokens\nexport GH={token}\n").as_bytes(),
        0o600,
    );
    let err = exits(&env.run(&["add", ".config/zsh"]), 3);
    let held = err
        .lines()
        .find(|line| line.contains(&format!("held {secrets}")))
        .expect("the hold line");
    assert!(held.contains("line 2 holds a github-token"), "{err}");
    assert!(!held.contains("--allow-plain"), "{err}");
    assert!(!err.contains(&token), "the value is never printed: {err}");

    let printed: Vec<Vec<String>> = err
        .split('`')
        .skip(1)
        .step_by(2)
        .map(|command| shell_words::split(command).expect("a pasteable command"))
        .collect();
    assert!(!printed.is_empty(), "{err}");
    for command in printed {
        let args: Vec<&str> = command.iter().skip(1).map(String::as_str).collect();
        env.run(&args);
    }
    env.run(&["sync"]);
    assert!(!remote_history(&env).contains(&token));
    assert!(!files_containing(&env.repo(), token.as_bytes()).contains(&secrets.to_owned()));
}

/// Every object reachable in the remote, as `git cat-file -p` prints it.
fn remote_objects(env: &TestEnv) -> String {
    let remote = env.remote_path().to_string_lossy().into_owned();
    env.git(&["--git-dir", &remote, "rev-list", "--all", "--objects"])
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(|id| env.git(&["--git-dir", &remote, "cat-file", "-p", id]))
        .collect()
}

/// A new file under `[dirs]` whose name matches `[guard] secret_names` and which holds a
/// credential is listed by its finding. `status`, `sync --dry-run` and `sync` never offer
/// `--allow-plain` for it, and running every command they print for it, as printed, then `sync`,
/// never sends the token to the remote.
#[test]
fn a_secret_named_dirs_file_that_holds_a_credential_is_never_offered_allow_plain() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    let keyed = age_installed();
    if keyed {
        env.ok(&["key", "init"]);
    }
    env.write(".config/zsh/.zshrc", b"source secrets.zsh\n", 0o644);
    env.ok(&["add", "--yes", ".config/zsh"]);
    env.ok(&["sync"]);

    let token = github_token();
    let secrets = ".config/zsh/secrets.zsh";
    env.write(
        secrets,
        format!("# tokens\nexport GH={token}\n").as_bytes(),
        0o600,
    );
    let mut printed = std::collections::BTreeSet::new();
    for (args, code) in [
        (&["status"][..], 0),
        (&["sync", "--dry-run"], 3),
        (&["sync"], 3),
    ] {
        let out = env.run(args);
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr(&out));
        assert_eq!(out.status.code(), Some(code), "{args:?}: {text}");
        let lines: Vec<&str> = text.lines().filter(|line| line.contains(secrets)).collect();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("line 2 holds a github-token")),
            "{args:?}: {text}"
        );
        assert!(!text.contains("--allow-plain"), "{args:?}: {text}");
        assert!(!text.contains(&token), "the value is never printed");
        printed.extend(
            lines
                .iter()
                .flat_map(|line| line.split('`').skip(1).step_by(2))
                .map(str::to_owned),
        );
    }
    assert!(!printed.is_empty());

    // A sync after each, so no later command can hide what an earlier one let through.
    for command in &printed {
        let words = shell_words::split(command).expect("a pasteable command");
        let args: Vec<&str> = words.iter().skip(1).map(String::as_str).collect();
        env.run(&args);
        env.run(&["sync"]);
        assert!(!remote_objects(&env).contains(&token), "{command}");
    }
    assert!(!files_containing(&env.repo(), token.as_bytes()).contains(&secrets.to_owned()));
    if keyed {
        assert!(remote_tree(&env).contains(".config/zsh/secrets{age}.zsh"));
    }
}

/// The content guard does not scan a binary file, so a secret-named one with a token in it is
/// offered `--encrypt` only, by `add <dir>`, `status`, `sync`, and `add <file>` on a public
/// remote. Running every command they print for it, as printed, encrypts it and never sends the
/// token to the remote.
#[test]
fn a_binary_secret_named_file_is_never_offered_allow_plain() {
    if !age_installed() {
        return;
    }
    let env = TestEnv::new();
    let origin = env.remote_url();
    env.ok(&["init", "--remote", &origin]);
    env.ok(&["key", "init"]);
    let state_file = env.state_dir().join("state.toml");
    let mut state = dotman::state::State::read(&state_file)
        .expect("read state")
        .expect("state");
    state.remote_public = Some(dotman::state::RemotePublic::True);
    state.remote_url = Some(origin);
    state.remote_checked_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs());
    state.save(&state_file, false).expect("save state");

    let token = github_token();
    let key = ".config/s/id.key";
    env.write(".config/s/rc", b"rc\n", 0o644);
    env.write(key, format!("bin\0t={token}\n").as_bytes(), 0o600);
    let mut printed = std::collections::BTreeSet::new();
    for (args, code) in [
        (&["add", ".config/s"][..], 3),
        (&["status"], 0),
        (&["sync", "--dry-run"], 3),
        (&["sync"], 3),
        (&["add", key], 1),
    ] {
        let out = env.run(args);
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), stderr(&out));
        assert_eq!(out.status.code(), Some(code), "{args:?}: {text}");
        let lines: Vec<&str> = text.lines().filter(|line| line.contains(key)).collect();
        assert!(
            lines.iter().any(|line| line.contains("binary")),
            "{args:?}: {text}"
        );
        assert!(!text.contains("--allow-plain"), "{args:?}: {text}");
        printed.extend(
            lines
                .iter()
                .flat_map(|line| line.split('`').skip(1).step_by(2))
                .map(str::to_owned),
        );
    }
    assert!(!printed.is_empty());

    for command in &printed {
        let words = shell_words::split(command).expect("a pasteable command");
        let args: Vec<&str> = words.iter().skip(1).map(String::as_str).collect();
        env.run(&args);
        env.run(&["sync"]);
        assert!(!remote_objects(&env).contains(&token), "{command}");
    }
    assert!(remote_tree(&env).contains(".config/s/id{age}.key"));
    assert!(!env.repo().join(key).exists());
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

/// `add --allow-plain` adds its `[guard] allow` entry to the machine config in place: comments,
/// a key dotman does not know and the entry already there stay as written.
#[test]
fn allow_plain_keeps_the_machine_config_as_written() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let text = "# this machine\nprofiles = [\"laptop\"] # mine\nfuture_key = 1\n\n[guard]\n# checked by hand\nallow = [{ path = \".config/old.zsh\", rule = \"npm-token\" }]\n";
    env.write_machine_config(text);
    let local = ".config/zsh/local.zsh";
    env.write(
        local,
        format!("export GH={}\n", github_token()).as_bytes(),
        0o600,
    );
    exits(&env.run(&["add", "--allow-plain", local]), 0);
    let saved = fs::read_to_string(env.xdg_config().join("dotman/config.toml")).unwrap();
    for kept in [
        "# this machine\nprofiles = [\"laptop\"] # mine\nfuture_key = 1\n",
        "# checked by hand\nallow = [{ path = \".config/old.zsh\", rule = \"npm-token\" }, ",
    ] {
        assert!(saved.contains(kept), "{kept}: {saved}");
    }
    assert!(
        !saved.contains("[identity]"),
        "no default is written: {saved}"
    );
    assert_eq!(
        allow_entries(&env),
        [
            (".config/old.zsh".to_owned(), "npm-token".to_owned()),
            (local.to_owned(), "github-token".to_owned())
        ]
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
    env.write_machine_config(&format!(
        "[[guard.allow]]\npath = \"{keys}\"\nrule = \"age-secret-key\"\n"
    ));
    assert_eq!(env.token_of("status", keys).as_deref(), Some("held"));
    exits(&env.run(&["sync"]), 3);
    assert!(!remote_history(&env).contains("AGE-SECRET-KEY-"));
}

/// The allow list is per machine. An entry pushed in `.dotman.toml` by another machine lets no
/// credential out of this one, and says it is ignored; this machine's own `--allow-plain` does,
/// and a machine that pulls the file still holds its own edits until it passes `--allow-plain`.
#[test]
fn a_pushed_allow_entry_never_lets_a_credential_out_of_another_machine() {
    let a = TestEnv::new();
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write(".bashrc", b"alias ll=ls\n", 0o644);
    a.ok(&["add", ".bashrc"]);
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
    b.ok(&["init", &b.remote_url()]);
    let toml = b.repo().join(".dotman.toml");
    let text = fs::read_to_string(&toml).unwrap();
    fs::write(
        &toml,
        format!("{text}\n[[guard.allow]]\npath = \".bashrc\"\nrule = \"github-token\"\n"),
    )
    .unwrap();
    b.ok(&["sync"]);

    let err = a.ok(&["sync"]);
    assert!(
        err.contains("[guard] allow is ignored (.bashrc github-token)"),
        "{err}"
    );
    let token = github_token();
    a.write(
        ".bashrc",
        format!("alias ll=ls\nexport GH={token}\n").as_bytes(),
        0o644,
    );
    let err = exits(&a.run(&["sync"]), 3);
    assert!(
        err.contains(".bashrc: held, line 2 holds a github-token"),
        "{err}"
    );
    assert!(!remote_history(&a).contains(&token));

    exits(&a.run(&["add", "--allow-plain", ".bashrc"]), 0);
    assert_eq!(
        allow_entries(&a),
        [(".bashrc".to_owned(), "github-token".to_owned())]
    );
    exits(&a.run(&["sync"]), 0);
    assert!(remote_history(&a).contains(&token));

    exits(&b.run(&["sync"]), 0);
    b.write(
        ".bashrc",
        format!("alias ll=ls\nexport GH={token}\nalias la=ls\n").as_bytes(),
        0o644,
    );
    let err = exits(&b.run(&["sync"]), 3);
    assert!(
        err.contains(".bashrc: held, line 2 holds a github-token"),
        "{err}"
    );
    assert!(!remote_history(&a).contains("alias la=ls"));
    exits(&b.run(&["add", "--allow-plain", ".bashrc"]), 0);
    exits(&b.run(&["sync"]), 0);
    assert!(remote_history(&a).contains("alias la=ls"));
}

/// An age key inside another rule's match, a `glpat-` run or a PEM `Comment:` header, is still
/// an age key: `--allow-plain` refuses the file, and nothing reaches the repo.
#[test]
fn an_age_key_nested_in_another_credential_is_never_stored() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    let pem_body = "b3BlbnNzaC1rZXktdjEAAAAA".repeat(3);
    for (key, text) in [
        (".glrc", format!("export GL=glpat-{FAKE_KEY}\n")),
        (
            ".pemrc",
            format!(
                "-----BEGIN OPENSSH PRIVATE KEY-----\nComment: {FAKE_KEY}\n{pem_body}\n-----END OPENSSH PRIVATE KEY-----\n"
            ),
        ),
    ] {
        env.write(key, text.as_bytes(), 0o600);
        let err = exits(&env.run(&["add", "--allow-plain", key]), 1);
        assert!(err.contains("holds an age secret key"), "{key}: {err}");
        assert!(!env.repo().join(key).exists(), "{key}");
    }
    assert_eq!(allow_entries(&env), []);
    assert_eq!(
        files_containing(&env.repo(), b"AGE-SECRET-KEY-"),
        Vec::<String>::new()
    );
}

/// A file with a NUL in its first 8000 bytes is binary, as git counts it, and the content guard
/// skips it: a tool that embeds a key shape, as age-keygen does, can be stored and synced.
#[test]
fn a_binary_file_that_embeds_a_key_shape_is_stored() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    let mut tool = b"\x7fELF\x02\x01\x01\0\0\0".to_vec();
    tool.extend_from_slice(format!("usage: {FAKE_KEY}\n").as_bytes());
    env.write(".local/bin/tool", &tool, 0o755);
    exits(&env.run(&["add", ".local/bin/tool"]), 0);
    exits(&env.run(&["sync"]), 0);
    assert!(remote_tree(&env).contains(".local/bin/tool"));
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
/// it, adds the rest and exits 3; sync holds it too. `add --allow-plain <dir>` still holds it, so
/// one flag never takes every credential under a directory; naming the file takes it.
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

    let err = exits(
        &env.run(&["add", "--allow-plain", "--yes", ".config/app"]),
        3,
    );
    assert!(
        err.contains(&format!(
            "held .config/app/remote.conf: line 1 holds a gitlab-pat; `dotman add --encrypt {path}` stores it encrypted; or remove the secret from the file",
            path = path.display()
        )),
        "{err}"
    );
    assert_eq!(allow_entries(&env), []);
    assert!(!env.repo().join(".config/app/remote.conf").exists());
    exits(
        &env.run(&["add", "--allow-plain", ".config/app/remote.conf"]),
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

/// A tracked `.gitignore` in a managed directory, as nvim's, is the user's file and not dotman's
/// ignore list: what it matches in the repo goes through every check before sync commits it. A
/// credential, in a matched file or under a matched directory, is held and never pushed, and
/// status names it; a name that is not UTF-8 stays out; `[ignore]` and the temp names apply. A
/// file the user added that the `.gitignore` matches is committed all the same.
#[test]
fn a_tracked_gitignore_never_lets_a_repo_file_skip_the_checks() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".config/app/a.conf", b"a\n", 0o644);
    env.write(".config/app/.gitignore", b"*.log\ndata\n", 0o644);
    env.ok(&["add", ".config/app"]);
    let mut config = env.repo_config();
    config.ignore.patterns.push("**/debug.log".to_owned());
    config
        .save(&env.repo().join(".dotman.toml"), false)
        .expect("save .dotman.toml");
    env.ok(&["sync"]);
    env.write(".config/app/keep.log", b"kept\n", 0o644);
    env.ok(&["add", ".config/app/keep.log"]);

    let token = github_token();
    let app = env.repo().join(".config/app");
    let unnamed = app.join(OsStr::from_bytes(b"z\xff.log"));
    let temp = app.join("data/.dotman-tmp-99999-a1B2c3");
    let held = [".config/app/creds.log", ".config/app/data/tok"];
    for key in held {
        env.write_repo(key, format!("token: {token}\n").as_bytes());
    }
    env.write_repo(".config/app/debug.log", b"junk\n");
    fs::write(&unnamed, b"junk\n").expect("write the unnamed file");
    fs::write(&temp, b"junk\n").expect("write the temp file");

    let listed: Vec<String> = env
        .porcelain("status")
        .into_iter()
        .filter(|(token, _)| token == "held-in-repo")
        .map(|(_, path)| path)
        .collect();
    assert_eq!(listed, held, "{listed:?}");
    let err = exits(&env.run(&["status"]), 0);
    assert!(
        err.contains("z\u{fffd}.log: in the repo, never committed"),
        "{err}"
    );
    let err = exits(&env.run(&["sync"]), 3);
    for key in held {
        assert!(
            err.contains(&format!("  {key}: line 1 holds a github-token")),
            "{err}"
        );
    }
    assert!(!err.contains(&token), "{err}");

    for key in held {
        fs::remove_file(env.repo().join(key)).expect("drop the credential");
    }
    exits(&env.run(&["sync"]), 0);
    let tree = remote_tree(&env);
    assert_eq!(
        tree.lines().collect::<Vec<_>>(),
        [
            ".config/app/.gitignore",
            ".config/app/a.conf",
            ".config/app/keep.log",
            ".dotman.toml",
            "install.sh"
        ]
    );
    assert!(!remote_history(&env).contains(&token));
    assert!(unnamed.exists() && temp.exists());
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

/// Every subject and body in the remote's history, and its trees and patches.
fn remote_everything(env: &TestEnv) -> String {
    let remote = env.remote_path().to_string_lossy().into_owned();
    let messages = env.git(&["--git-dir", &remote, "log", "--all", "--format=%s%n%b"]);
    format!("{messages}{}{}", remote_tree(env), remote_history(env))
}

/// A credential in a path's name or in a symlink's target is held like one in a file, and no flag
/// lets it through: `add` refuses the file and the link, `--allow-plain` and `--encrypt` too, and
/// a directory whose name holds one, which `[dirs]` would record; `add <dir>` and
/// `sync` hold each new one under a managed directory and exit 3, and neither the remote's trees,
/// patches and messages nor `dotman log` ever show it. Renamed, the file is captured.
#[test]
fn a_credential_in_a_name_or_a_link_target_never_reaches_the_remote() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    let token = github_token();
    let named = format!(".config/d/{token}");
    let link = ".config/d/current";
    env.write(".config/d/rc", b"safe=1\n", 0o644);
    env.write(&named, b"x=1\n", 0o644);
    env.write(format!(".cache/x/{token}.json"), b"y=1\n", 0o644);
    env.symlink(link, format!("../../.cache/x/{token}.json"));
    for (args, found) in [
        (&["add", &named][..], "the name holds a github-token"),
        (
            &["add", "--allow-plain", &named],
            "the name holds a github-token",
        ),
        (&["add", link], "the link target holds a github-token"),
    ] {
        let err = exits(&env.run(args), 1);
        assert!(err.contains(found), "{args:?}: {err}");
        assert!(!err.contains("--encrypt stores"), "{args:?}: {err}");
    }
    let dir = format!(".config/{token}");
    env.write(format!("{dir}/rc"), b"a=1\n", 0o644);
    let err = exits(&env.run(&["add", "--yes", &dir]), 1);
    assert!(err.contains("the name holds a github-token"), "{err}");
    fs::remove_dir_all(env.home().join(&dir)).unwrap();
    if age_installed() {
        env.ok(&["key", "init"]);
        let err = exits(&env.run(&["add", "--encrypt", &named]), 1);
        assert!(err.contains("encryption cannot hide a name"), "{err}");
    }
    assert_eq!(env.porcelain("list"), []);
    assert_eq!(
        files_containing(&env.repo(), token.as_bytes()),
        Vec::<String>::new()
    );

    let err = exits(&env.run(&["add", "--yes", ".config/d"]), 3);
    assert!(
        err.contains(&format!(
            "held {named}: the name holds a github-token; encryption cannot hide a name, so rename it"
        )),
        "{err}"
    );
    assert!(
        err.contains("held .config/d/current: the link target holds a github-token; --encrypt takes regular files only, so point the link elsewhere"),
        "{err}"
    );
    env.write(format!(".config/d/sub/{token}.conf"), b"z=1\n", 0o644);
    for _ in 0..2 {
        let err = exits(&env.run(&["sync"]), 3);
        for key in [named.as_str(), link] {
            assert!(err.contains(&format!("  {key} (")), "{err}");
        }
        assert!(
            err.contains(&format!(".config/d/sub/{token}.conf (the name holds")),
            "{err}"
        );
    }
    assert_eq!(
        remote_tree(&env).lines().collect::<Vec<_>>(),
        [".config/d/rc", ".dotman.toml", "install.sh"]
    );
    let log = env.ok(&["log"]);
    assert!(!log.contains(&token), "{log}");
    assert!(!remote_everything(&env).contains(&token));

    fs::rename(
        env.home().join(&named),
        env.home().join(".config/d/renamed"),
    )
    .unwrap();
    fs::remove_dir_all(env.home().join(".config/d/sub")).unwrap();
    fs::remove_file(env.home().join(link)).unwrap();
    env.symlink(link, "rc");
    exits(&env.run(&["sync"]), 0);
    let tree = remote_tree(&env);
    assert!(
        tree.contains(".config/d/renamed") && tree.contains(link),
        "{tree}"
    );
    assert!(!remote_everything(&env).contains(&token));
}

/// `add --for` with a value that holds a credential, which would name the repo file, is refused,
/// in a dry run too, with the `--for` value as the cause; another value adds the file.
#[test]
fn add_refuses_a_for_value_that_holds_a_credential() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.write(".zshrc", b"x=1\n", 0o644);
    let condition = format!("host={}", github_token());
    for dry in [true, false] {
        let mut args = vec!["add", "--for", &condition, ".zshrc"];
        if dry {
            args.push("--dry-run");
        }
        let err = exits(&env.run(&args), 1);
        assert!(
            err.contains("the --for value holds a credential (github-token), which would be in the repo's file name; give a different --for value"),
            "{err}"
        );
        assert!(!err.contains("rename"), "{err}");
    }
    let named: Vec<String> = tree_checksum(&env.repo())
        .into_keys()
        .filter(|name| name.contains(".zshrc"))
        .collect();
    assert_eq!(named, Vec::<String>::new());
    env.ok(&["add", "--for", "host=work", ".zshrc"]);
}

/// A managed symlink pointed again at a target that holds a credential is held at capture, so
/// the repo keeps the old link, and the `forget` the hold prints, run as printed, stops managing
/// it: the next sync commits that, and the target never reaches the remote.
#[test]
fn a_managed_link_pointed_at_a_credential_is_held_with_a_forget_that_works() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".zshrc", b"zsh\n", 0o644);
    env.symlink(".zlink", ".zshrc");
    env.ok(&["add", ".zlink"]);
    env.ok(&["sync"]);
    let token = github_token();
    env.write(format!(".cache/{token}"), b"x\n", 0o644);
    fs::remove_file(env.home().join(".zlink")).unwrap();
    env.symlink(".zlink", format!(".cache/{token}"));

    let err = exits(&env.run(&["sync"]), 3);
    assert!(
        err.contains(".zlink: held, the link target holds a github-token"),
        "{err}"
    );
    assert_eq!(
        fs::read_link(env.repo().join(".zlink")).unwrap(),
        PathBuf::from(".zshrc")
    );
    let out = common::run_hint(&env, &err, "forget");
    assert!(out.status.success(), "{}", stderr(&out));
    exits(&env.run(&["sync"]), 0);
    assert!(!remote_tree(&env).contains(".zlink"));
    assert!(!remote_everything(&env).contains(&token));
}

/// A symlink or a file made in the repo by hand reaches no capture: the staging guard holds one
/// whose target or name holds a credential, sync stops before the pull and commits nothing, and
/// status lists both. Moved out of the repo, as the hold says, they leave nothing behind.
#[test]
fn the_staging_guard_holds_a_credential_in_a_repo_name_or_link_target() {
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".a", b"a\n", 0o644);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    let token = github_token();
    let link = env.repo().join(".config/l");
    fs::create_dir_all(env.repo().join(".config")).unwrap();
    symlink(format!("../.cache/{token}"), &link).unwrap();
    let named = format!(".x/{token}");
    env.write_repo(&named, b"1\n");

    let listed: Vec<String> = env
        .porcelain("status")
        .into_iter()
        .filter(|(token, _)| token == "held-in-repo")
        .map(|(_, path)| path)
        .collect();
    assert_eq!(listed, [".config/l".to_owned(), named.clone()]);
    let err = exits(&env.run(&["sync"]), 3);
    assert!(
        err.contains("  .config/l: the link target holds a github-token\n    not tracked; move "),
        "{err}"
    );
    assert!(
        err.contains(&format!("  {named}: the name holds a github-token\n")),
        "{err}"
    );
    assert!(!remote_everything(&env).contains(&token));

    fs::remove_file(&link).unwrap();
    fs::remove_dir_all(env.repo().join(".x")).unwrap();
    exits(&env.run(&["sync"]), 0);
    assert!(!remote_everything(&env).contains(&token));
}

/// A file that a commit made by hand already holds, under a name with a credential, is held when
/// edited, and the `forget` the hold prints, run as printed, works: the deletion it leaves is
/// never held, so the next sync commits it.
#[test]
fn a_committed_name_with_a_credential_is_held_until_forgotten() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let token = github_token();
    let named = format!(".y/{token}");
    env.write_repo(&named, b"1\n");
    env.ok(&["git", "add", "--", &named]);
    env.ok(&["git", "commit", "-q", "-m", "by hand"]);
    env.ok(&["apply"]);
    env.write_repo(&named, b"2\n");

    let err = exits(&env.run(&["sync"]), 3);
    assert!(
        err.contains(&format!("  {named}: the name holds a github-token\n")),
        "{err}"
    );
    let out = common::run_hint(&env, &err, "forget");
    assert!(out.status.success(), "{}", stderr(&out));
    exits(&env.run(&["sync"]), 0);
    let repo = env.repo().to_string_lossy().into_owned();
    let tree = env.git(&["-C", &repo, "ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!tree.contains(&token), "{tree}");
}

/// A git tree whose current tree or history holds a credential in a name or a link target is
/// imported as one commit, with the file and the link left out and reported, root files such as
/// a README and a LICENSE link among them, and a symlink farm
/// leaves them out too: nothing in the repo names the token.
#[test]
fn imports_leave_out_a_credential_in_a_name_or_a_link_target() {
    let token = github_token();
    let commit = |env: &TestEnv, source: &Path, message: &str| {
        common::git_in(env, source, &["add", "-A"]);
        common::git_in(env, source, &["commit", "-q", "-m", message]);
    };
    // The old file has `.bashrc`'s bytes, so only the walk over the history's paths finds it.
    let env = TestEnv::new();
    let source = source_tree(&env, "dots", &[(".bashrc", "# rc\n")]);
    common::git_in(&env, &source, &["init", "-q"]);
    fs::write(source.join(format!(".old-{token}")), "# rc\n").unwrap();
    commit(&env, &source, "old");
    fs::remove_file(source.join(format!(".old-{token}"))).unwrap();
    commit(&env, &source, "gone");
    source.seal();
    let err = exits(&env.run(&["import", &source.to_string_lossy()]), 0);
    assert!(err.contains(": github-token, added in "), "{err}");
    let repo = env.repo().to_string_lossy().into_owned();
    let history = env.git(&["-C", &repo, "log", "--all", "-p", "--format=%s%n%b"]);
    assert!(history.contains(".bashrc"), "{history}");
    assert!(!history.contains(&token), "{history}");

    let env = TestEnv::new();
    let source = source_tree(&env, "dots", &[(".bashrc", "# rc\n")]);
    common::git_in(&env, &source, &["init", "-q"]);
    fs::write(source.join(format!(".{token}")), "1\n").unwrap();
    symlink(format!(".cache/{token}"), source.join(".link")).unwrap();
    fs::write(source.join(format!("README-{token}")), "1\n").unwrap();
    symlink(format!("../{token}"), source.join("LICENSE")).unwrap();
    commit(&env, &source, "dots");
    source.seal();
    let err = exits(&env.run(&["import", &source.to_string_lossy()]), 3);
    assert!(err.contains("the name holds a github-token"), "{err}");
    assert!(
        err.contains(".link: the link target holds a github-token"),
        "{err}"
    );
    let repo = env.repo().to_string_lossy().into_owned();
    let history = env.git(&["-C", &repo, "log", "--all", "-p", "--format=%s%n%b"]);
    assert!(history.contains(".bashrc"), "{history}");
    assert!(!history.contains(&token), "{history}");
    assert_eq!(
        files_containing(&env.repo(), token.as_bytes()),
        Vec::<String>::new()
    );

    let env = TestEnv::new();
    stow(&env);
    env.write(format!("dotfiles/zsh/.z-{token}"), b"1\n", 0o644);
    env.symlink(format!(".z-{token}"), format!("dotfiles/zsh/.z-{token}"));
    env.symlink(
        "dotfiles/nvim/.config/nvim/current",
        format!("../../../../.cache/{token}"),
    );
    env.write(format!(".cache/{token}"), b"1\n", 0o644);
    let err = exits(&env.run(&["import", "--yes"]), 3);
    assert!(err.contains("the name holds a github-token"), "{err}");
    assert!(
        err.contains(".config/nvim/current: the link target holds a github-token"),
        "{err}"
    );
    let repo = env.repo().to_string_lossy().into_owned();
    let tree = env.git(&["-C", &repo, "ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(tree.contains(".config/nvim/init.lua"), "{tree}");
    assert!(
        !tree.contains(&token) && !tree.contains("nvim/current"),
        "{tree}"
    );
}

fn commit_all(env: &TestEnv, source: &Path, message: &str) {
    common::git_in(env, source, &["add", "-A"]);
    common::git_in(env, source, &["commit", "-q", "-m", message]);
}

/// Every path of every kind, at any depth, in any commit of the repo's history.
fn repo_names(env: &TestEnv) -> String {
    let repo = env.repo().to_string_lossy().into_owned();
    let commits = env.git(&["-C", &repo, "rev-list", "--all"]);
    commits
        .lines()
        .map(|commit| {
            let names = [
                "-C",
                &repo,
                "ls-tree",
                "-r",
                "-t",
                "-z",
                "--name-only",
                commit,
            ];
            env.git(&names)
        })
        .collect()
}

/// Commits a submodule at `.vim/<token>`, which no import fetches.
fn tip_submodule(env: &TestEnv, source: &Path, token: &str) {
    let head = common::git_in(env, source, &["rev-parse", "HEAD"]);
    let entry = format!("160000,{},.vim/{token}", head.trim());
    common::git_in(
        env,
        source,
        &["update-index", "--add", "--cacheinfo", &entry],
    );
    common::git_in(env, source, &["commit", "-q", "-m", "submodule"]);
}

/// Commits a file whose name dotman cannot manage, as it holds a newline.
fn tip_newline(env: &TestEnv, source: &Path, token: &str) {
    env.write(source.join(format!(".config/a\n{token}")), b"x\n", 0o644);
    commit_all(env, source, "newline");
}

/// Commits a file whose name dotman cannot manage, as it is not UTF-8.
fn tip_not_utf8(env: &TestEnv, source: &Path, token: &str) {
    let mut name = b".x\xff-".to_vec();
    name.extend_from_slice(token.as_bytes());
    env.write(source.join(OsStr::from_bytes(&name)), b"x\n", 0o644);
    commit_all(env, source, "bytes");
}

/// Commits an empty directory beside a `.dotman.toml` of the source's own, so that the import has
/// nothing else to convert.
fn tip_empty_dir(env: &TestEnv, source: &Path, token: &str) {
    env.write(source.join(".dotman.toml"), b"", 0o644);
    commit_all(env, source, "config");
    let commit = with_empty_dir(env, source, &format!(".e-{token}"));
    common::git_in(env, source, &["update-ref", "HEAD", &commit]);
}

/// An entry of a git tree's current tree whose name holds a credential is left out of what the
/// import commits, whatever its kind: a submodule, a name dotman cannot manage, with a newline in
/// it or bytes that are not UTF-8, and an empty directory. It is reported, and nothing in the repo
/// names the token: no commit when the history is squashed, the tip with `--keep-history`, and
/// the work tree either way.
#[test]
fn an_import_commits_no_entry_whose_name_holds_a_credential() {
    let token = github_token();
    let tips: [(&str, Tip); 4] = [
        ("submodule", tip_submodule),
        ("newline", tip_newline),
        ("not UTF-8", tip_not_utf8),
        ("empty dir", tip_empty_dir),
    ];
    for (label, tip) in tips {
        for keep in [false, true] {
            let env = TestEnv::new();
            let source = source_tree(&env, "dots", &[(".bashrc", "# rc\n")]);
            common::git_in(&env, &source, &["init", "-q"]);
            commit_all(&env, &source, "base");
            tip(&env, &source, &token);
            source.seal();
            let path = source.to_string_lossy().into_owned();
            let mut args = vec!["import", path.as_str()];
            if keep {
                args.push("--keep-history");
            }
            let err = exits(&env.run(&args), 3);
            assert!(
                err.contains("the name holds a github-token"),
                "{label}: {err}"
            );
            let repo = env.repo().to_string_lossy().into_owned();
            let head = [
                "-C",
                &repo,
                "ls-tree",
                "-r",
                "-t",
                "-z",
                "--name-only",
                "HEAD",
            ];
            let head = env.git(&head);
            assert!(
                head.contains(".bashrc") && !head.contains(&token),
                "{label}: {head}"
            );
            if !keep {
                let names = repo_names(&env);
                assert!(!names.contains(&token), "{label}: {names}");
            }
            let status = [
                "-C",
                &repo,
                "status",
                "--porcelain",
                "-z",
                "--untracked-files=all",
            ];
            let status = env.git(&status);
            assert!(!status.contains(&token), "{label}: {status}");
        }
    }
}

/// A held name on a tree entry that no checkout makes, an empty directory under `..`, which would
/// name a directory beside the import's clone, is left out of the import, and nothing outside the
/// clone is removed.
#[test]
fn a_held_name_that_leads_out_of_the_import_clone_removes_nothing_there() {
    let token = github_token();
    let env = TestEnv::new();
    let source = source_tree(&env, "dots", &[(".bashrc", "# rc\n")]);
    common::git_in(&env, &source, &["init", "-q"]);
    commit_all(&env, &source, "base");
    let commit = with_empty_dir(&env, &source, &format!("../../{token}"));
    common::git_in(&env, &source, &["update-ref", "HEAD", &commit]);
    source.seal();
    let sentinel = env.xdg_data().join(&token).join("keep");
    env.write(&sentinel, b"keep\n", 0o644);
    let err = exits(&env.run(&["import", &source.to_string_lossy()]), 3);
    assert!(err.contains("the name holds a github-token"), "{err}");
    assert!(sentinel.exists(), "{err}");
    let names = repo_names(&env);
    assert!(
        names.contains(".bashrc") && !names.contains(&token),
        "{names}"
    );
}

/// A legacy store with one commit, which `HEAD` names, holding each of `keys` with the same bytes.
// reason: clippy.toml exempts unwrap only inside #[test] fns
#[allow(clippy::unwrap_used)]
fn legacy_store(env: &TestEnv, keys: &[&str]) -> PathBuf {
    let root = env.root().join("legacy");
    let id = "c1".repeat(16);
    let object = "ab".repeat(16);
    let put = |out: &mut Vec<u8>, bytes: &[u8]| {
        out.extend(u64::try_from(bytes.len()).unwrap().to_le_bytes());
        out.extend_from_slice(bytes);
    };
    // bincode's legacy encoding of the old `Snapshot`: the commit's id, parents, message, author,
    // timestamp and tree hash, then each path with its hash, mode and content hash.
    let mut snapshot = Vec::new();
    put(&mut snapshot, id.as_bytes());
    snapshot.extend(1_u64.to_le_bytes());
    put(&mut snapshot, "0".repeat(32).as_bytes());
    put(&mut snapshot, b"initial");
    put(&mut snapshot, b"legacyuser");
    snapshot.extend(1_700_000_000_i64.to_le_bytes());
    put(&mut snapshot, "0".repeat(32).as_bytes());
    snapshot.extend(u64::try_from(keys.len()).unwrap().to_le_bytes());
    for key in keys {
        put(&mut snapshot, key.as_bytes());
        put(&mut snapshot, object.as_bytes());
        snapshot.extend(0o644_u32.to_le_bytes());
        put(&mut snapshot, object.as_bytes());
    }
    let packed = |bytes: &[u8]| zstd::encode_all(bytes, 1).unwrap();
    let commit = root.join(format!("commits/{id}.zst"));
    env.write(commit, &packed(&snapshot), 0o644);
    let object = root.join(format!("objects/{object}.zst"));
    env.write(object, &packed(b"x=1\n"), 0o644);
    env.write(root.join("refs/heads/main"), id.as_bytes(), 0o644);
    env.write(root.join("HEAD"), b"ref: refs/heads/main", 0o644);
    root
}

/// A legacy import, and an overlay from a legacy store or a git tree with `--include-missing`,
/// commit no key whose name holds a credential, and report it; a key with a newline, which the
/// path rules refuse, and a submodule are not committed either. The other files are.
#[test]
fn legacy_imports_and_overlays_commit_no_name_that_holds_a_credential() {
    let token = github_token();
    let token_key = format!(".config/{token}");
    let refused = format!(".config/a\n{token}");

    let env = TestEnv::new();
    let store = legacy_store(&env, &[".bashrc", &token_key, &refused]);
    let err = exits(&env.run(&["import", &store.to_string_lossy()]), 3);
    assert!(err.contains("the name holds a github-token"), "{err}");
    let names = repo_names(&env);
    assert!(
        names.contains(".bashrc") && !names.contains(&token),
        "{names}"
    );

    let env = TestEnv::new();
    let source = overlay_source(&env, 0o644, b"# bashrc\n");
    env.ok(&["init", &source.to_string_lossy(), "--take-repo"]);
    let store = legacy_store(&env, &[".zprofile", &token_key, &refused]);
    let store = store.to_string_lossy().into_owned();
    let err = exits(
        &env.run(&["import", "--overlay", &store, "--include-missing"]),
        3,
    );
    assert!(err.contains("the name holds a github-token"), "{err}");

    let other = source_tree(&env, "dots", &[(".zlogin", "x\n"), (&token_key, "x\n")]);
    common::git_in(&env, &other, &["init", "-q"]);
    commit_all(&env, &other, "dots");
    tip_newline(&env, &other, &token);
    tip_submodule(&env, &other, &token);
    other.seal();
    let other = other.to_string_lossy().into_owned();
    let err = exits(
        &env.run(&["import", "--overlay", &other, "--include-missing"]),
        3,
    );
    assert!(err.contains("the name holds a github-token"), "{err}");
    let names = repo_names(&env);
    assert!(
        names.contains(".zprofile") && names.contains(".zlogin") && !names.contains(&token),
        "{names}"
    );
}

/// An overlay's commit names the kind of source, not its path or URL, which may hold a token.
#[test]
fn an_overlay_commit_names_neither_the_source_path_nor_its_url() {
    let token = github_token();
    let env = TestEnv::new();
    let base = overlay_source(&env, 0o644, b"# bashrc\n");
    env.ok(&["init", &base.to_string_lossy(), "--take-repo"]);
    let dir = env.root().join(format!("dots-{token}"));
    for (name, file, url) in [("path", ".vimrc", false), ("url", ".zshrc", true)] {
        let source = dir.join(name);
        env.write(source.join(file), b"x\n", 0o644);
        common::git_in(&env, &source, &["init", "-q"]);
        commit_all(&env, &source, "dots");
        let source = source.to_string_lossy().into_owned();
        let arg = if url {
            format!("file://{source}")
        } else {
            source
        };
        exits(
            &env.run(&["import", "--overlay", &arg, "--include-missing"]),
            0,
        );
    }
    let repo = env.repo().to_string_lossy().into_owned();
    let messages = env.git(&["-C", &repo, "log", "--all", "--format=%B"]);
    assert_eq!(
        messages.matches("import: overlay from a git repo").count(),
        2,
        "{messages}"
    );
    assert!(!messages.contains(&token), "{messages}");
}

/// Commits `name` with `bytes` on top of the source, then its removal.
fn added_and_removed(env: &TestEnv, source: &Path, name: &str, bytes: &str) {
    env.write(source.join(name), bytes.as_bytes(), 0o644);
    commit_all(env, source, "add");
    common::git_in(env, source, &["rm", "-q", "--", name]);
    commit_all(env, source, "gone");
}

/// Like [`added_and_removed`], where `name` comes only in a merge commit, which neither parent has.
fn merged_and_removed(env: &TestEnv, source: &Path, name: &str, bytes: &str) {
    common::git_in(env, source, &["checkout", "-q", "-b", "side"]);
    env.write(source.join(".side"), b"s\n", 0o644);
    commit_all(env, source, "side");
    common::git_in(env, source, &["checkout", "-q", "-"]);
    env.write(source.join(".main"), b"m\n", 0o644);
    commit_all(env, source, "main");
    let merge = ["merge", "-q", "--no-ff", "--no-commit", "side"];
    common::git_in(env, source, &merge);
    added_and_removed(env, source, name, bytes);
}

/// A commit of HEAD's tree, which holds only files at its root, with the directory `path` added as
/// an empty one, which no work tree can make. Each directory on the way holds only the next, and
/// `--literally` lets one be named `..`, which git never checks out.
fn with_empty_dir(env: &TestEnv, source: &Path, path: &str) -> String {
    // A bad digit makes a tree that `hash-object` refuses, which fails the test.
    let raw = |hex: &str| -> Vec<u8> {
        hex.trim()
            .as_bytes()
            .chunks(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair).unwrap_or_default();
                u8::from_str_radix(pair, 16).unwrap_or_default()
            })
            .collect()
    };
    let file = env.root().join("tree.bin");
    let write = |bytes: &[u8]| -> String {
        env.write(&file, bytes, 0o644);
        let file = file.to_string_lossy();
        let args = ["hash-object", "--literally", "-t", "tree", "-w", &file];
        common::git_in(env, source, &args).trim().to_owned()
    };
    let mut parts = path.split('/');
    let top = parts.next().unwrap_or_default();
    let mut dir = write(b"");
    for part in parts.rev() {
        let mut entry = format!("40000 {part}\0").into_bytes();
        entry.extend(raw(&dir));
        dir = write(&entry);
    }
    let listed = common::git_in(env, source, &["ls-tree", "HEAD"]);
    // Git orders a tree's entries by name, a directory's name as if it ended in `/`.
    let mut entries: Vec<(String, Vec<u8>)> = listed
        .lines()
        .filter_map(|line| {
            let (meta, file) = line.split_once('\t')?;
            let oid = meta.split(' ').nth(2)?;
            let mut entry = format!("100644 {file}\0").into_bytes();
            entry.extend(raw(oid));
            Some((file.to_owned(), entry))
        })
        .collect();
    let mut entry = format!("40000 {top}\0").into_bytes();
    entry.extend(raw(&dir));
    entries.push((format!("{top}/"), entry));
    entries.sort();
    let tree = write(
        &entries
            .into_iter()
            .flat_map(|(_, entry)| entry)
            .collect::<Vec<u8>>(),
    );
    let commit = ["commit-tree", &tree, "-p", "HEAD", "-m", "empty dir"];
    common::git_in(env, source, &commit).trim().to_owned()
}

/// A commit whose tree also holds `name` as an empty directory, then one of the source's tree
/// again.
fn empty_dir_and_removed(env: &TestEnv, source: &Path, name: &str, _: &str) {
    let evil = with_empty_dir(env, source, name);
    let clean = common::git_in(env, source, &["rev-parse", "HEAD:"]);
    let gone = ["commit-tree", clean.trim(), "-p", &evil, "-m", "gone"];
    let gone = common::git_in(env, source, &gone);
    common::git_in(env, source, &["update-ref", "HEAD", gone.trim()]);
}

type History = fn(&TestEnv, &Path, &str, &str);
type Tip = fn(&TestEnv, &Path, &str);

/// A git tree whose clean tip hides a name with a credential in its history is imported as one
/// commit of the current tree, whatever the name or how it came: after a newline or a tab, in an
/// empty file, in a file only a merge adds, empty or with another file's bytes, and as an empty
/// directory. With `--keep-history` every commit is kept, with the warning.
#[test]
fn an_import_squashes_a_history_that_once_held_a_credential_in_a_name() {
    let token = github_token();
    let cases: [(&str, History, String, &str); 5] = [
        (
            "newline",
            added_and_removed,
            format!(".config/a\n{token}"),
            "x\n",
        ),
        (
            "tab, empty",
            added_and_removed,
            format!(".config/a\t{token}"),
            "",
        ),
        (
            "merge, empty",
            merged_and_removed,
            format!(".config/{token}"),
            "",
        ),
        (
            "merge, shared",
            merged_and_removed,
            format!(".config/{token}"),
            "# rc\n",
        ),
        (
            "empty dir",
            empty_dir_and_removed,
            format!(".e-{token}"),
            "",
        ),
    ];
    for (label, make, name, bytes) in cases {
        for keep in [false, true] {
            let env = TestEnv::new();
            let source = source_tree(&env, "dots", &[(".bashrc", "# rc\n")]);
            common::git_in(&env, &source, &["init", "-q"]);
            commit_all(&env, &source, "base");
            make(&env, &source, &name, bytes);
            source.seal();
            let tip = ["ls-tree", "-r", "-t", "--name-only", "HEAD"];
            let tip = common::git_in(&env, &source, &tip);
            assert!(!tip.contains(&token), "{label}: {tip}");
            let path = source.to_string_lossy().into_owned();
            let mut args = vec!["import", path.as_str()];
            if keep {
                args.push("--keep-history");
            }
            let err = exits(&env.run(&args), 0);
            assert!(err.contains(": github-token, added in "), "{label}: {err}");
            let repo = env.repo().to_string_lossy().into_owned();
            let count = env.git(&["-C", &repo, "rev-list", "--count", "--all"]);
            if keep {
                assert!(err.contains("--keep-history imports all"), "{label}: {err}");
                assert_ne!(count.trim(), "1", "{label}");
            } else {
                assert!(err.contains("the history is left out"), "{label}: {err}");
                assert_eq!(count.trim(), "1", "{label}");
            }
        }
    }
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

/// A device whose repo holds `.zshrc` and `.zlink`, a symlink to it, pushed to its remote.
fn device_with_a_symlink() -> TestEnv {
    let a = TestEnv::new();
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write(".zshrc", b"zsh\n", 0o644);
    a.symlink(".zlink", ".zshrc");
    a.ok(&["add", ".zshrc", ".zlink"]);
    a.ok(&["sync"]);
    a.git(&[
        "--git-dir",
        &a.remote_path().to_string_lossy(),
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    a
}

fn link_target(path: &Path) -> Option<PathBuf> {
    fs::read_link(path).ok()
}

/// `core.symlinks = false` in the user's git config never turns a managed symlink into a file
/// holding its target, and a template directory, from `GIT_TEMPLATE_DIR` here, configures
/// nothing in dotman's repos: not that, and not a sparse checkout that leaves files out.
#[test]
fn the_user_s_git_config_and_template_dir_never_shape_the_checkout() {
    let a = device_with_a_symlink();

    let b = a.second_device();
    let gitconfig = b.home().join(".gitconfig");
    let mut config = fs::read_to_string(&gitconfig).unwrap();
    config.push_str("[core]\n\tsymlinks = false\n");
    fs::write(&gitconfig, config).unwrap();
    exits(&b.run(&["init", &a.remote_url()]), 0);
    for dir in [b.home(), b.repo()] {
        assert_eq!(
            link_target(&dir.join(".zlink")),
            Some(PathBuf::from(".zshrc")),
            "{}",
            dir.display()
        );
    }
    assert_eq!(b.token_of("status", ".zlink").as_deref(), Some("clean"));

    let c = a.second_device();
    let template = c.root().join("template");
    fs::create_dir_all(template.join("info")).unwrap();
    fs::write(
        template.join("config"),
        "[core]\n\tsymlinks = false\n\tsparseCheckout = true\n",
    )
    .unwrap();
    fs::write(template.join("info/sparse-checkout"), "/.zshrc\n").unwrap();
    let vars = [("GIT_TEMPLATE_DIR", &*template.to_string_lossy())];
    exits(&c.run_with(&vars, &["init", &a.remote_url()]), 0);
    assert_eq!(
        link_target(&c.home().join(".zlink")),
        Some(PathBuf::from(".zshrc"))
    );
    assert!(c.repo().join(".dotman.toml").exists());

    let d = TestEnv::new();
    exits(&d.run_with(&vars, &["init"]), 0);
    let repo_config = fs::read_to_string(d.repo().join(".git/config")).unwrap();
    assert!(!repo_config.contains("sparse"), "{repo_config}");
    assert!(!d.repo().join(".git/info/sparse-checkout").exists());
}

/// A tracked `.gitattributes` with `* merge=union` never joins two machines' edits of one file
/// in silence: the second sync stops on a conflict, exit 4, and keeps its own edit.
#[test]
fn a_tracked_merge_driver_never_joins_a_conflict() {
    let a = device_with_a_symlink();
    a.write(".conf", b"line1\n", 0o644);
    a.write(".gitattributes", b"* merge=union\n", 0o644);
    a.ok(&["add", ".conf", ".gitattributes"]);
    a.ok(&["sync"]);
    let b = a.second_device();
    exits(&b.run(&["init", &a.remote_url()]), 0);

    a.write(".conf", b"from-one\n", 0o644);
    a.ok(&["sync"]);
    b.write(".conf", b"from-two\n", 0o644);
    let err = exits(&b.run(&["sync"]), 4);
    assert!(err.contains("conflicting paths:\n  .conf"), "{err}");
    assert_eq!(b.read(".conf"), b"from-two\n");
    assert_eq!(fs::read(b.repo().join(".conf")).unwrap(), b"from-two\n");
}

/// Git config that would settle every conflict without a word: a driver that exits 0, under the
/// name `merge=text` looks up, and `union` for files whose merge attribute is unspecified.
fn settle_conflicts_in_git_config(env: &TestEnv) -> std::io::Result<()> {
    let gitconfig = env.home().join(".gitconfig");
    let mut config = fs::read_to_string(&gitconfig)?;
    config.push_str(
        "[merge \"text\"]\n\tname = custom\n\tdriver = true\n[merge]\n\tdefault = union\n",
    );
    fs::write(&gitconfig, config)
}

/// A merge driver in the user's git config, named `text`, or `merge.default`, never settles a
/// conflict: with a tracked `* merge=union` as well, the second sync stops, exit 4, and keeps its
/// own edit, and the pull it prints stops on conflict markers.
#[test]
fn a_merge_driver_in_the_user_s_git_config_never_settles_a_conflict() {
    let a = device_with_a_symlink();
    a.write(".conf", b"line1\nshared=base\nline3\n", 0o644);
    a.write(".gitattributes", b"* merge=union\n", 0o644);
    a.ok(&["add", ".conf", ".gitattributes"]);
    a.ok(&["sync"]);
    let b = a.second_device();
    settle_conflicts_in_git_config(&b).unwrap();
    exits(&b.run(&["init", &a.remote_url()]), 0);

    a.write(".conf", b"line1\nshared=FROM-A\nline3\n", 0o644);
    a.ok(&["sync"]);
    b.write(".conf", b"line1\nshared=FROM-B\nline3\n", 0o644);
    let err = exits(&b.run(&["sync"]), 4);
    assert!(err.contains("conflicting paths:\n  .conf"), "{err}");
    assert_eq!(b.read(".conf"), b"line1\nshared=FROM-B\nline3\n");
    let remote = a.remote_path().to_string_lossy().into_owned();
    assert_eq!(
        a.git(&["--git-dir", &remote, "show", "main:.conf"]),
        "line1\nshared=FROM-A\nline3\n"
    );

    let pull = "dotman git pull --rebase origin main";
    assert!(err.contains(pull), "{err}");
    let args: Vec<&str> = pull.split(' ').skip(1).collect();
    let out = b.run(&args);
    assert!(!out.status.success(), "{}", stderr(&out));
    let merged = fs::read_to_string(b.repo().join(".conf")).unwrap();
    for marker in ["<<<<<<<", "shared=FROM-A", "shared=FROM-B", ">>>>>>>"] {
        assert!(merged.contains(marker), "{merged}");
    }
}

/// A clean and smudge filter and a merge driver in the user's git config, each leaving a marker
/// file when git runs it.
fn demo_drivers_in_git_config(env: &TestEnv) -> std::io::Result<PathBuf> {
    let marker = env.root().join("ran");
    let gitconfig = env.home().join(".gitconfig");
    let mut config = fs::read_to_string(&gitconfig)?;
    let touch = |what: &str| format!("echo {what} >> '{}'", marker.display());
    write!(
        config,
        "[filter \"demo\"]\n\tsmudge = \"{}; cat\"\n\tclean = \"{}; cat\"\n[merge \"demo\"]\n\tdriver = \"{}; cat %B > %A\"\n",
        touch("smudge"),
        touch("clean"),
        touch("driver"),
    )
    .map_err(std::io::Error::other)?;
    fs::write(&gitconfig, config)?;
    Ok(marker)
}

/// A tracked `.gitattributes` macro, named `merge` or after any attribute the pinned line sets or
/// git reads for line endings, never brings back `text`, `eol`, `filter`, `ident`,
/// `working-tree-encoding` or a merge driver: the clone, a sync of CRLF content with an expanded
/// `$Id$`, and the pull back are byte-exact, no filter runs, and a conflicting sync stops (exit 4)
/// with its printed pull stopping on conflict markers, not settled by the driver.
#[test]
fn a_tracked_attribute_macro_never_transforms_content_or_settles_a_conflict() {
    let all = "text eol=crlf ident filter=demo working-tree-encoding=UTF-16 merge=demo";
    let mut others = String::new();
    for name in [
        "text",
        "filter",
        "ident",
        "working-tree-encoding",
        "binary",
        "diff",
        "eol",
        "crlf",
    ] {
        writeln!(others, "[attr]{name} {all}").unwrap();
    }
    others.push_str("* binary diff eol crlf\n");
    let rounds = [
        "[attr]merge text eol=crlf\n".to_owned(),
        "[attr]merge filter=demo\n".to_owned(),
        "[attr]merge ident\n".to_owned(),
        "[attr]merge working-tree-encoding=UTF-16\n".to_owned(),
        "[attr]merge merge=demo\n".to_owned(),
        "[attr]merge merge=union\n".to_owned(),
        others,
    ];
    let base: &[u8] = b"line1\nid=$Id$\nline3\n";
    let crlf: &[u8] = b"line1\r\nid=$Id: 0123 $\r\nline3\r\n";
    for attributes in rounds {
        let a = device_with_a_symlink();
        let remote = a.remote_path().to_string_lossy().into_owned();
        let a_ran = demo_drivers_in_git_config(&a).unwrap();
        a.write(".conf", base, 0o644);
        a.write(".gitattributes", attributes.as_bytes(), 0o644);
        a.ok(&["add", ".conf", ".gitattributes"]);
        a.ok(&["sync"]);
        let b = a.second_device();
        let b_ran = demo_drivers_in_git_config(&b).unwrap();
        exits(&b.run(&["init", &a.remote_url()]), 0);
        assert_eq!(b.read(".conf"), base, "{attributes}");
        assert_eq!(fs::read(b.repo().join(".conf")).unwrap(), base);

        b.write(".conf", crlf, 0o644);
        b.ok(&["sync"]);
        let pushed = a.git_raw(&["--git-dir", &remote, "cat-file", "blob", "main:.conf"]);
        assert_eq!(pushed.stdout, crlf, "{attributes}");
        a.ok(&["sync"]);
        assert_eq!(a.read(".conf"), crlf, "{attributes}");

        a.write(".conf", b"line1\nid=$Id$\nFROM-A\n", 0o644);
        a.ok(&["sync"]);
        b.write(".conf", b"line1\nid=$Id$\nFROM-B\n", 0o644);
        let err = exits(&b.run(&["sync"]), 4);
        assert!(err.contains("conflicting paths:\n  .conf"), "{err}");
        let pull = "dotman git pull --rebase origin main";
        assert!(err.contains(pull), "{err}");
        let args: Vec<&str> = pull.split(' ').skip(1).collect();
        let out = b.run(&args);
        assert!(!out.status.success(), "{}", stderr(&out));
        let merged = fs::read_to_string(b.repo().join(".conf")).unwrap();
        for marker in ["<<<<<<<", "FROM-A", "FROM-B", ">>>>>>>"] {
            assert!(merged.contains(marker), "{attributes}{merged}");
        }
        for ran in [a_ran, b_ran] {
            assert!(!ran.exists(), "{attributes}{:?}", fs::read_to_string(&ran));
        }
    }
}

/// A repo an older dotman made holds an attributes line without the bare `merge`: none, or
/// `merge=text`, which the user's git config can redefine; or with `merge` last, where a tracked
/// `[attr]merge` macro wins over the rest of the line. A dry run leaves it; the next `apply`
/// writes the pinned line, and so does `sync`, before its pull, so neither a tracked `merge=union`
/// nor the user's drivers settle the conflict. A missing file is written too.
#[test]
fn an_attributes_line_an_older_dotman_wrote_is_rewritten_before_the_next_write() {
    const OLD: &[u8] = b"* -text -filter -ident -working-tree-encoding\n";
    const NAMED: &[u8] = b"* -text -filter -ident -working-tree-encoding merge=text\n";
    const PREVIOUS: &[u8] = b"* -text -filter -ident -working-tree-encoding merge\n";
    const PINNED: &[u8] = b"* merge -text -filter -ident -working-tree-encoding\n";
    let a = device_with_a_symlink();
    a.write(".conf", b"line1\n", 0o644);
    a.write(".gitattributes", b"* merge=union\n", 0o644);
    a.ok(&["add", ".conf", ".gitattributes"]);
    a.ok(&["sync"]);
    let device = || {
        let b = a.second_device();
        exits(&b.run(&["init", &a.remote_url()]), 0);
        let attributes = b.repo().join(".git/info/attributes");
        assert_eq!(fs::read(&attributes).unwrap(), PINNED);
        (b, attributes)
    };

    let (b, attributes) = device();
    fs::write(&attributes, OLD).unwrap();
    let before = b.four_trees();
    for dry_run in [&["apply", "--dry-run"][..], &["sync", "--dry-run"]] {
        exits(&b.run(dry_run), 0);
        assert_eq!(b.four_trees(), before, "{dry_run:?}");
    }
    let err = exits(&b.run(&["apply"]), 0);
    assert!(err.contains("rewrote "), "{err}");
    assert_eq!(fs::read(&attributes).unwrap(), PINNED);
    assert_eq!(mode_of(&attributes), 0o644);

    for (round, stale) in [Some(OLD), Some(NAMED), Some(PREVIOUS), None]
        .into_iter()
        .enumerate()
    {
        let (b, attributes) = device();
        settle_conflicts_in_git_config(&b).unwrap();
        match stale {
            Some(bytes) => fs::write(&attributes, bytes).unwrap(),
            None => fs::remove_file(&attributes).unwrap(),
        }
        a.write(".conf", format!("from-one {round}\n").as_bytes(), 0o644);
        a.ok(&["sync"]);
        b.write(".conf", b"from-two\n", 0o644);
        let err = exits(&b.run(&["sync"]), 4);
        assert!(err.contains("conflicting paths:\n  .conf"), "{err}");
        assert_eq!(fs::read(&attributes).unwrap(), PINNED);
        assert_eq!(b.read(".conf"), b"from-two\n");
    }
}

/// The refresh keeps the lines a user added to `.git/info/attributes` and moves the pinned line
/// below them, where it wins. When it cannot write the file, as with a read-only `.git/info` or a
/// directory in its place, a writing command warns once, naming the file and the risk, and goes
/// on.
#[test]
fn the_attributes_refresh_keeps_user_lines_and_never_stops_a_command() {
    use std::os::unix::fs::PermissionsExt;
    const PINNED: &str = "* merge -text -filter -ident -working-tree-encoding\n";
    let env = TestEnv::new();
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".conf", b"one\n", 0o644);
    env.ok(&["add", ".conf"]);
    env.ok(&["sync"]);
    let info = env.repo().join(".git/info");
    let attributes = info.join("attributes");

    fs::write(&attributes, format!("{PINNED}*.age diff=age\n")).unwrap();
    let err = exits(&env.run(&["apply"]), 0);
    assert!(err.contains("rewrote "), "{err}");
    assert_eq!(
        fs::read_to_string(&attributes).unwrap(),
        format!("*.age diff=age\n{PINNED}")
    );
    let err = exits(&env.run(&["apply"]), 0);
    assert!(!err.contains("rewrote "), "{err}");

    fs::write(&attributes, "* -text\n").unwrap();
    fs::set_permissions(&info, fs::Permissions::from_mode(0o555)).unwrap();
    let warned = |err: &str| {
        err.matches(&format!(
            "until {} ends with the line dotman pins, a tracked .gitattributes can choose the merge driver",
            attributes.display()
        ))
        .count()
    };
    let err = exits(&env.run(&["apply"]), 0);
    assert_eq!(warned(&err), 1, "{err}");
    env.write(".conf", b"two\n", 0o644);
    let err = exits(&env.run(&["sync"]), 0);
    fs::set_permissions(&info, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(warned(&err), 1, "{err}");
    assert!(err.contains("pushed 1 commit"), "{err}");
    assert_eq!(fs::read_to_string(&attributes).unwrap(), "* -text\n");

    fs::remove_file(&attributes).unwrap();
    fs::create_dir(&attributes).unwrap();
    let err = exits(&env.run(&["apply"]), 0);
    assert_eq!(warned(&err), 1, "{err}");
    assert!(attributes.is_dir());
}

/// Every object in the store at `git_dir`, reachable or not, as one text.
fn every_object(env: &TestEnv, git_dir: &Path) -> String {
    env.git(&[
        "--git-dir",
        &git_dir.to_string_lossy(),
        "cat-file",
        "--batch-all-objects",
        "--batch",
    ])
}

/// `init <path>` and `import <path>` clone a local path through git's transport, as they would a
/// URL, never by copying or hardlinking its object store: an object nothing in the source
/// reaches stays behind.
#[test]
fn a_local_path_is_cloned_through_git_s_transport() {
    let a = device_with_a_symlink();
    let marker = "unreachable-object-marker";
    let loose = a.root().join("loose.txt");
    fs::write(&loose, marker).unwrap();
    let repo = a.repo().to_string_lossy().into_owned();
    a.git(&["-C", &repo, "hash-object", "-w", &loose.to_string_lossy()]);
    assert!(every_object(&a, &a.repo().join(".git")).contains(marker));

    let b = TestEnv::new();
    exits(&b.run(&["init", &repo]), 0);
    assert_eq!(b.read(".zshrc"), b"zsh\n");
    assert!(!every_object(&b, &b.repo().join(".git")).contains(marker));

    let c = TestEnv::new();
    let remote = a.remote_path().to_string_lossy().into_owned();
    a.git(&[
        "--git-dir",
        &remote,
        "hash-object",
        "-w",
        &loose.to_string_lossy(),
    ]);
    exits(&c.run(&["import", "--yes", &remote]), 0);
    assert!(c.repo().join(".zshrc").exists());
    assert!(!every_object(&c, &c.repo().join(".git")).contains(marker));
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

/// Keys that a hostile upstream puts in the index reach `status` as collision rows. Its output
/// escapes their control characters, so a name can neither drive the terminal nor forge a line.
#[test]
fn status_escapes_the_keys_it_prints() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    let repo = env.repo().to_string_lossy().into_owned();
    let blob = env.git(&["-C", &repo, "hash-object", "-w", "/dev/null"]);
    for key in ["x/A\u{1b}[2J.conf", "x/a\u{1b}[2J.conf"] {
        let info = format!("100644,{},{key}", blob.trim());
        env.git(&["-C", &repo, "update-index", "--add", "--cacheinfo", &info]);
    }
    let out = env.run(&["status"]);
    let err = exits(&out, 0);
    let shown = String::from_utf8_lossy(&out.stdout);
    assert!(shown.contains("collision"), "{shown:?}");
    assert!(shown.contains("x/A\\u{1b}[2J.conf"), "{shown:?}");
    for text in [shown.as_ref(), err.as_str()] {
        assert!(!text.contains('\u{1b}'), "{text:?}");
    }
}

// Branch names a remote or an import source picks

/// Renames `repo`'s current branch to `name` with `update-ref`, as a hostile source makes it, since
/// `git branch` refuses such names. An unborn HEAD stays unborn.
fn put_on_branch(env: &TestEnv, repo: &Path, name: &str) {
    let refname = format!("refs/heads/{name}");
    let old = git_in(env, repo, &["symbolic-ref", "HEAD"]);
    if env
        .git_raw(&[
            "-C",
            &repo.to_string_lossy(),
            "rev-parse",
            "-q",
            "--verify",
            "HEAD",
        ])
        .status
        .success()
    {
        git_in(env, repo, &["update-ref", &refname, "HEAD"]);
        git_in(env, repo, &["update-ref", "-d", old.trim()]);
    }
    git_in(env, repo, &["symbolic-ref", "HEAD", &refname]);
}

/// Whether a file named `name` is anywhere under `dir`.
fn found_under(dir: &Path, name: &str) -> bool {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry.file_name() == name
                || (entry.file_type().is_ok_and(|kind| kind.is_dir())
                    && found_under(&entry.path(), name))
        })
}

/// A remote whose HEAD names a branch, made with `update-ref`, that git would read as an option is
/// cloned, and then sync, status and doctor refuse the branch before git sees it: no command in
/// the name runs, and `--mirror` deletes no branch of the remote. The way out, run as printed,
/// renames the branch to `main`, and sync then pushes it and nothing else.
#[test]
fn a_branch_name_that_reads_as_an_option_never_reaches_git() {
    for name in [
        "--upload-pack=touch${IFS}pwned;true",
        "--receive-pack=touch${IFS}pwned;true",
        "--mirror",
    ] {
        let a = TestEnv::new();
        let work = common::work_tree(&a, "src", &[(".bashrc", "# rc\n", false)], None);
        let remote = a.remote_path().to_string_lossy().into_owned();
        let refname = format!("refs/heads/{name}");
        let push = format!("HEAD:{refname}");
        git_in(
            &a,
            &work,
            &["push", "-q", &remote, &push, "HEAD:refs/heads/keep"],
        );
        a.git(&["--git-dir", &remote, "symbolic-ref", "HEAD", &refname]);
        let b = a.second_device();
        exits(&b.run(&["init", &a.remote_url()]), 0);
        assert_eq!(
            git_in(&b, &b.repo(), &["symbolic-ref", "HEAD"]).trim(),
            refname
        );

        let err = exits(&b.run(&["sync"]), 1);
        let why = "name starts with a dash, which git reads as an option";
        assert!(err.contains(why), "{err}");
        assert!(err.contains("`dotman git branch -m main`"), "{err}");
        assert!(exits(&b.run(&["status"]), 1).contains(why));
        let doctor = b.run(&["doctor"]);
        let report = String::from_utf8_lossy(&doctor.stdout);
        assert!(!doctor.status.success(), "{report}");
        assert!(
            report
                .lines()
                .any(|line| line.starts_with("fail  branch: ") && line.contains(why)),
            "{report}"
        );
        assert!(!found_under(a.root(), "pwned") && !found_under(b.root(), "pwned"));

        let out = common::run_hint(&b, &err, "git branch -m");
        assert!(out.status.success(), "{}", stderr(&out));
        exits(&b.run(&["sync"]), 0);
        let refs = a.remote_refs();
        for kept in ["refs/heads/keep", "refs/heads/main", &refname] {
            assert!(refs.contains(kept), "{kept}: {refs}");
        }
        assert!(!found_under(a.root(), "pwned") && !found_under(b.root(), "pwned"));
    }
}

/// A branch whose name holds a credential never reaches the remote as a ref: sync refuses it
/// without printing the name, and pushes `main` once the way out, run as printed, renames it.
#[test]
fn a_branch_name_that_holds_a_credential_never_reaches_the_remote() {
    let token = format!("ghp_{}", "x".repeat(36));
    let env = TestEnv::new();
    env.write(".bashrc", b"x\n", 0o644);
    env.ok(&["init"]);
    env.ok(&["add", ".bashrc"]);
    env.ok(&["sync"]);
    put_on_branch(&env, &env.repo(), &format!("wip-{token}"));
    env.ok(&["init", "--remote", &env.remote_url()]);

    let err = exits(&env.run(&["sync", "--dry-run"]), 1);
    assert!(err.contains("(github-token)"), "{err}");
    let err = exits(&env.run(&["sync"]), 1);
    assert!(!err.contains(&token), "{err}");
    assert_eq!(env.remote_refs(), "");
    let out = common::run_hint(&env, &err, "git branch -m");
    assert!(out.status.success(), "{}", stderr(&out));
    exits(&env.run(&["sync"]), 0);
    let refs = env.remote_refs();
    assert!(
        refs.contains("refs/heads/main") && !refs.contains(&token),
        "{refs}"
    );
}

/// An unborn branch has no ref to rename, so the way out doctor prints for one points `HEAD` at
/// `main` instead, and run as printed it ends the refusal.
#[test]
fn the_way_out_for_an_unborn_branch_dotman_refuses_runs_as_printed() {
    let env = TestEnv::new();
    env.ok(&["init"]);
    put_on_branch(&env, &env.repo(), "--mirror");
    let doctor = env.run(&["doctor"]);
    let report = String::from_utf8_lossy(&doctor.stdout).into_owned();
    assert!(report.contains("fail  branch: "), "{report}");
    let out = common::run_hint(&env, &report, "git symbolic-ref HEAD refs/heads/main");
    assert!(out.status.success(), "{}", stderr(&out));
    let report = String::from_utf8_lossy(&env.run(&["doctor"]).stdout).into_owned();
    assert!(!report.contains("fail  branch"), "{report}");
}

/// An import whose source's HEAD names a branch dotman refuses, by path or by URL, puts the commit
/// on `main` with a note that names the rule, not the credential, keeps no ref with the name, and,
/// by URL, sets no origin, as for a detached HEAD.
#[test]
fn an_import_puts_a_source_branch_dotman_refuses_on_main() {
    let token = format!("ghp_{}", "y".repeat(36));
    for name in [
        format!("wip-{token}"),
        "--upload-pack=touch${IFS}pwned;true".to_owned(),
    ] {
        for by_url in [false, true] {
            let env = TestEnv::new();
            let work = common::work_tree(&env, "src", &[(".bashrc", "# rc\n", false)], None);
            put_on_branch(&env, &work, &name);
            let source = if by_url {
                format!("file://{}", work.display())
            } else {
                work.to_string_lossy().into_owned()
            };
            let err = env.ok(&["import", "--yes", &source]);
            assert!(
                err.contains("so the import puts its commit on a new branch, main"),
                "{err}"
            );
            assert!(!err.contains(&token), "{err}");
            let repo = env.repo();
            assert_eq!(
                git_in(&env, &repo, &["symbolic-ref", "HEAD"]).trim(),
                "refs/heads/main"
            );
            let refs = git_in(&env, &repo, &["for-each-ref", "--format=%(refname)"]);
            assert_eq!(refs.trim(), "refs/heads/main", "{err}");
            assert_eq!(
                files_containing(&repo, name.as_bytes()),
                Vec::<String>::new()
            );
            assert_eq!(git_in(&env, &repo, &["remote"]), "", "{err}");
            env.ok(&["init", "--remote", &env.remote_url()]);
            exits(&env.run(&["sync"]), 0);
            let pushed = env.remote_refs();
            assert!(
                !pushed.contains(&token) && !pushed.contains("--"),
                "{pushed}"
            );
            assert!(!found_under(env.root(), "pwned"));
        }
    }
}

/// A rebase stopped on a branch whose name dotman refuses, with a lock file git left behind: the
/// recovery never prints the name, and its steps, run as printed, put the commits it had on `main`.
#[test]
fn the_rebase_recovery_never_prints_a_branch_dotman_refuses() {
    let env = TestEnv::new();
    env.write(".a", b"base\n", 0o644);
    env.ok(&["init"]);
    env.ok(&["add", ".a"]);
    env.ok(&["sync"]);
    let repo = env.repo();
    put_on_branch(&env, &repo, "--mirror");
    env.write_repo(".a", b"mine\n");
    git_in(&env, &repo, &["commit", "-q", "-am", "mine"]);
    let tip = git_in(&env, &repo, &["rev-parse", "HEAD"]);
    git_in(&env, &repo, &["update-ref", "refs/heads/up", "HEAD~1"]);
    git_in(&env, &repo, &["checkout", "-q", "up"]);
    env.write_repo(".a", b"theirs\n");
    git_in(&env, &repo, &["commit", "-q", "-am", "theirs"]);
    git_in(
        &env,
        &repo,
        &["symbolic-ref", "HEAD", "refs/heads/--mirror"],
    );
    git_in(&env, &repo, &["reset", "-q", "--hard"]);
    let rebase = env.git_raw(&["-C", &repo.to_string_lossy(), "rebase", "up"]);
    assert!(!rebase.status.success(), "the rebase should stop");
    fs::write(repo.join(".git/index.lock"), b"").unwrap();

    let err = exits(&env.run(&["status"]), 4);
    assert!(!err.contains("--mirror"), "{err}");
    let rm = err
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("1. "))
        .unwrap();
    let back = "dotman git rebase --quit && dotman git checkout -f -b main ORIG_HEAD";
    assert!(err.contains(&format!("3. if that fails: {back}")), "{err}");
    for line in [rm, back] {
        let out = common::paste(&env, line);
        assert!(out.status.success(), "{line}: {}", stderr(&out));
    }
    assert_eq!(
        git_in(&env, &repo, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    assert_eq!(git_in(&env, &repo, &["rev-parse", "HEAD"]), tip);
    exits(&env.run(&["status"]), 0);
}

/// The conflict recipe names a branch other than `main` as the repo has it, and its pull, run as
/// printed on a branch with a slash and a dot, reaches the conflict: a full refname passed to git
/// pulls the same branch.
#[test]
fn the_printed_pull_reaches_a_branch_other_than_main() {
    let a = TestEnv::new();
    a.ok(&["init"]);
    a.ok(&["init", "--remote", &a.remote_url()]);
    a.write(".a", b"base\n", 0o644);
    a.ok(&["add", ".a"]);
    a.ok(&["sync"]);
    a.ok(&["git", "branch", "-m", "topic/a.b"]);
    a.ok(&["sync"]);
    let remote = a.remote_path().to_string_lossy().into_owned();
    a.git(&[
        "--git-dir",
        &remote,
        "symbolic-ref",
        "HEAD",
        "refs/heads/topic/a.b",
    ]);
    let b = a.second_device();
    b.ok(&["init", &a.remote_url()]);
    a.write(".a", b"from a\n", 0o644);
    a.ok(&["sync"]);
    b.write(".a", b"from b\n", 0o644);

    let err = exits(&b.run(&["sync"]), 4);
    let pull = "dotman git pull --rebase origin topic/a.b";
    assert!(err.contains(&format!("1. {pull}")), "{err}");
    let out = common::paste(&b, pull);
    assert!(!out.status.success(), "the pull stops on the conflict");
    let merged = fs::read_to_string(b.repo().join(".a")).unwrap();
    assert!(merged.contains("<<<<<<<"), "{merged}");
}
