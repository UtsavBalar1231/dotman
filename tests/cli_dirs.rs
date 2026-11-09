mod common;

use common::{TestEnv, assert_converged, mode_of};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::process::Command;

const TWO_MIB: usize = 2_097_152;

fn exits(env: &TestEnv, code: i32, args: &[&str]) -> String {
    let out = env.run(args);
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(code), "dot {args:?}: {err}");
    err
}

/// A device with `.config/nvim` and `.config/gh` managed as directories, synced once.
fn device() -> TestEnv {
    let env = TestEnv::new();
    env.ok(&["init"]);
    env.ok(&["init", "--remote", &env.remote_url()]);
    env.write(".config/nvim/init.lua", b"-- init\n", 0o644);
    env.write(".config/gh/config.yml", b"git_protocol: ssh\n", 0o644);
    env.ok(&["add", ".config/nvim", ".config/gh"]);
    exits(&env, 0, &["sync"]);
    let remote = env.remote_path().to_string_lossy().into_owned();
    env.git(&[
        "--git-dir",
        &remote,
        "symbolic-ref",
        "HEAD",
        "refs/heads/main",
    ]);
    env
}

fn write_held(env: &TestEnv) {
    env.write(".config/nvim/big.dat", &vec![b'x'; TWO_MIB], 0o644);
    env.write(".config/nvim/blob.bin", &[1, 0, 2, 3], 0o644);
    env.write(".config/gh/hosts.yml", b"github.com:\n", 0o600);
}

fn remote_tree(env: &TestEnv) -> Vec<String> {
    let remote = env.remote_path().to_string_lossy().into_owned();
    env.git(&["--git-dir", &remote, "ls-tree", "-r", "--name-only", "main"])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn tokens(env: &TestEnv) -> Vec<(String, String)> {
    env.porcelain("status")
        .into_iter()
        .filter(|(token, _)| token != "clean")
        .collect()
}

#[test]
fn a_new_file_under_a_managed_dir_reaches_the_other_device() {
    let a = device();
    let b = a.second_device();
    b.ok(&["init", &b.remote_url(), "--take-repo"]);

    a.write(".config/nvim/lua/plugins/foo.lua", b"return {}\n", 0o600);
    assert_eq!(
        a.token_of("status", ".config/nvim/lua/plugins/foo.lua")
            .as_deref(),
        Some("new")
    );
    exits(&a, 0, &["sync"]);
    exits(&b, 0, &["sync"]);

    let foo = b.home().join(".config/nvim/lua/plugins/foo.lua");
    assert_eq!(fs::read(&foo).expect("foo.lua on b"), b"return {}\n");
    assert_eq!(mode_of(&foo), 0o600);
    assert_converged(&a, &b);
    assert_eq!(
        a.token_of("status", ".config/nvim/lua/plugins/foo.lua")
            .as_deref(),
        Some("clean")
    );
}

#[test]
fn big_binary_and_secret_named_files_are_held_and_sync_exits_3_but_pushes_the_rest() {
    let a = device();
    write_held(&a);
    a.write(".config/nvim/foo.lua", b"return {}\n", 0o644);

    let held = [
        ".config/gh/hosts.yml",
        ".config/nvim/big.dat",
        ".config/nvim/blob.bin",
    ];
    let status = tokens(&a);
    for key in held {
        assert!(
            status.contains(&("held".to_owned(), key.to_owned())),
            "{status:?}"
        );
    }
    assert!(status.contains(&("new".to_owned(), ".config/nvim/foo.lua".to_owned())));
    let human = String::from_utf8_lossy(&a.run(&["status"]).stdout).into_owned();
    assert!(human.contains("dotman add <file>"), "{human}");
    assert!(
        human.contains(
            "hosts.yml (name matches [guard] secret_names; `dotman add --encrypt <file>`"
        ),
        "{human}"
    );
    assert!(
        human.contains("big.dat (over 1 MiB; `dotman add <file>` adds it)"),
        "{human}"
    );
    assert!(human.contains("blob.bin (binary"), "{human}");

    let before = (a.four_trees(), a.remote_refs());
    let err = exits(&a, 3, &["sync", "--dry-run"]);
    assert!(err.contains("would capture .config/nvim/foo.lua"), "{err}");
    assert!(err.contains(".config/gh/hosts.yml"), "{err}");
    assert_eq!((a.four_trees(), a.remote_refs()), before);

    let err = exits(&a, 3, &["sync"]);
    assert!(err.contains("3 new files held back"), "{err}");
    let tree = remote_tree(&a);
    assert!(
        tree.contains(&".config/nvim/foo.lua".to_owned()),
        "{tree:?}"
    );
    for key in held {
        assert!(!tree.contains(&key.to_owned()), "{key} was pushed");
    }
    assert_eq!(exits(&a, 3, &["sync"]).matches("held back").count(), 1);
}

#[test]
fn dot_add_releases_a_held_file() {
    let a = device();
    write_held(&a);
    exits(&a, 3, &["sync"]);

    a.ok(&["add", ".config/gh/hosts.yml"]);
    assert_eq!(
        a.token_of("status", ".config/gh/hosts.yml").as_deref(),
        Some("clean")
    );
    exits(&a, 3, &["sync"]);
    assert!(remote_tree(&a).contains(&".config/gh/hosts.yml".to_owned()));
    assert_eq!(
        a.token_of("status", ".config/nvim/big.dat").as_deref(),
        Some("held")
    );
}

#[test]
fn nested_git_dirs_are_never_listed_and_an_ignore_pattern_hides_a_file() {
    let a = device();
    a.write(".config/nvim/plugin/.git/config", b"[core]\n", 0o644);
    a.write(".config/nvim/plugin/init.lua", b"-- p\n", 0o644);
    a.write(".config/nvim/scratch.txt", b"scratch\n", 0o644);
    a.write(".config/nvim/.dotman-tmp-x", b"tmp\n", 0o644);
    a.symlink(".config/nvim/dangling", "nowhere");

    let keys =
        |env: &TestEnv| -> Vec<String> { tokens(env).into_iter().map(|(_, key)| key).collect() };
    assert_eq!(
        keys(&a),
        [".config/nvim/plugin/init.lua", ".config/nvim/scratch.txt"]
    );
    let err = a.ok(&["status"]);
    assert!(err.contains("dangling: dangling symlink"), "{err}");

    let mut config = a.repo_config();
    config.ignore.patterns.push("**/scratch.txt".to_owned());
    config
        .save(&a.repo().join(".dotman.toml"), false)
        .expect("save .dotman.toml");
    assert_eq!(keys(&a), [".config/nvim/plugin/init.lua"]);

    exits(&a, 0, &["sync"]);
    let tree = remote_tree(&a);
    assert!(tree.contains(&".config/nvim/plugin/init.lua".to_owned()));
    assert!(
        !tree.iter().any(|k| k.contains(".git/")
            || k.contains("scratch")
            || k.contains("dangling")
            || k.contains("dotman-tmp")),
        "{tree:?}"
    );
}

/// Ignored junk beside a managed file in the repo copy is left out of the commit path by path.
/// Those paths reach git on stdin: as arguments, 6500 of these 1 KiB ones come to over 6 MiB,
/// the kernel's ceiling whatever the stack limit, and staging failed with E2BIG.
#[test]
fn thousands_of_left_out_paths_reach_git_without_overflowing_its_arguments() {
    let a = device();
    let dir = format!(
        ".config/nvim/{}/{}/{}",
        "d".repeat(250),
        "e".repeat(250),
        "f".repeat(250)
    );
    a.write(format!("{dir}/keep.lua"), b"-- keep\n", 0o644);
    let mut config = a.repo_config();
    config.ignore.patterns.push("**/*.bak".to_owned());
    config
        .save(&a.repo().join(".dotman.toml"), false)
        .expect("save .dotman.toml");
    exits(&a, 0, &["sync"]);
    let junk = a.repo().join(&dir);
    let tail = "x".repeat(240);
    for i in 0..6500 {
        fs::write(junk.join(format!("{i:05}-{tail}.bak")), b"").expect("write junk");
    }
    a.write(".config/nvim/init.lua", b"-- edited\n", 0o644);

    exits(&a, 0, &["sync"]);
    let tree = remote_tree(&a);
    assert!(tree.contains(&format!("{dir}/keep.lua")), "{tree:?}");
    assert!(!tree.iter().any(|k| k.contains(".bak")), "{tree:?}");
    let remote = a.remote_path().to_string_lossy().into_owned();
    let pushed = a.git(&["--git-dir", &remote, "show", "main:.config/nvim/init.lua"]);
    assert_eq!(pushed, "-- edited\n");
}

#[test]
fn a_forgotten_file_under_a_managed_dir_stays_out_of_the_repo() {
    let a = device();
    a.write(".config/nvim/a.lua", b"-- a\n", 0o644);
    a.ok(&["add", ".config/nvim/a.lua"]);
    a.ok(&["forget", ".config/nvim/a.lua"]);
    exits(&a, 0, &["sync"]);

    assert!(
        a.porcelain("status")
            .iter()
            .all(|(_, key)| key != ".config/nvim/a.lua")
    );
    assert_eq!(a.read(".config/nvim/a.lua"), b"-- a\n");
    assert!(!remote_tree(&a).contains(&".config/nvim/a.lua".to_owned()));
    assert!(
        a.repo_config()
            .ignore
            .patterns
            .contains(&".config/nvim/a.lua".to_owned())
    );
}

#[test]
fn a_forget_does_not_bounce_back_from_the_other_device() {
    let a = device();
    a.write(".config/nvim/a.lua", b"-- a\n", 0o644);
    exits(&a, 0, &["sync"]);
    let b = a.second_device();
    b.ok(&["init", &b.remote_url(), "--take-repo"]);
    assert_eq!(b.read(".config/nvim/a.lua"), b"-- a\n");

    a.ok(&["forget", ".config/nvim/a.lua"]);
    exits(&a, 0, &["sync"]);
    exits(&b, 0, &["sync"]);
    assert_eq!(b.read(".config/nvim/a.lua"), b"-- a\n");
    assert!(
        b.porcelain("status")
            .iter()
            .all(|(_, key)| key != ".config/nvim/a.lua")
    );
    exits(&a, 0, &["sync"]);
    assert!(!remote_tree(&a).contains(&".config/nvim/a.lua".to_owned()));
}

#[test]
fn entries_the_scan_cannot_capture_are_reported_and_specials_are_held() {
    let a = device();
    let nvim = a.home().join(".config/nvim");
    a.write(".config/nvim/cafe\u{301}.lua", b"nfd\n", 0o644);
    a.write(".config/nvim/x{os=linux}.lua", b"brace\n", 0o644);
    let made = Command::new("mkfifo")
        .arg(nvim.join("pipe"))
        .status()
        .expect("run mkfifo");
    assert!(made.success());
    let socket = UnixListener::bind(nvim.join("sock")).expect("bind a socket");
    a.write(".config/nvim/locked.lua", b"locked\n", 0o000);
    a.write(".config/nvim/private/x.lua", b"x\n", 0o644);
    fs::set_permissions(nvim.join("private"), fs::Permissions::from_mode(0o000)).expect("chmod");
    let _restore = Readable(nvim.join("private"));
    // root reads anything, so the unreadable cases mean nothing there.
    let unreadable = fs::read(nvim.join("locked.lua")).is_err();

    let status = tokens(&a);
    let mut held = vec![".config/nvim/pipe", ".config/nvim/sock"];
    if unreadable {
        held.extend([".config/nvim/locked.lua", ".config/nvim/private"]);
    }
    for key in &held {
        assert!(
            status.contains(&("held".to_owned(), (*key).to_owned())),
            "{key}: {status:?}"
        );
    }
    let out = a.run(&["status"]);
    let (human, err) = (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    );
    // Only a kind `add` can store gets `add` as its remedy.
    assert!(
        human.contains("pipe (a socket, FIFO or device, which dotman never stores)\n")
            && !human.contains("keeps one"),
        "{human}"
    );
    assert!(
        err.contains("its NFC form is `caf\u{e9}.lua`") && err.contains("x{os=linux}.lua"),
        "{err}"
    );
    if unreadable {
        assert!(
            human.contains("locked.lua (cannot be read; fix its permissions"),
            "{human}"
        );
    }

    let err = exits(&a, 3, &["sync"]);
    assert!(
        err.contains("held back") && err.contains("NFC form"),
        "{err}"
    );
    let tree = remote_tree(&a);
    assert!(
        !tree.iter().any(|key| key.contains("pipe")
            || key.contains("sock")
            || key.contains("locked")
            || key.contains("private")),
        "{tree:?}"
    );
    drop(socket);
}

/// Makes a directory readable again when dropped, so a failed assertion cannot leave the temp
/// root undeletable.
struct Readable(std::path::PathBuf);

impl Drop for Readable {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[test]
fn a_name_that_is_not_utf8_is_noted_until_an_ignore_pattern_matches_its_lossy_form() {
    let a = device();
    let name = OsStr::from_bytes(b"bad\xff.lua");
    fs::write(a.home().join(".config/nvim").join(name), b"x\n").expect("write");
    let err = a.ok(&["status"]);
    assert!(err.contains("not valid UTF-8"), "{err}");

    let mut config = a.repo_config();
    config.ignore.patterns.push("**/bad*.lua".to_owned());
    config
        .save(&a.repo().join(".dotman.toml"), false)
        .expect("save .dotman.toml");
    let err = a.ok(&["status"]);
    assert!(!err.contains("not valid UTF-8"), "{err}");
}

#[test]
fn a_name_with_a_control_character_is_noted_escaped_and_never_captured() {
    let a = device();
    a.write(".config/nvim/evil\nheld forged", b"x\n", 0o644);
    a.write(".config/nvim/esc\u{1b}[2J", b"x\n", 0o644);
    let err = a.ok(&["status"]);
    assert!(
        err.contains("evil\\nheld forged") && err.contains("esc\\u{1b}[2J"),
        "{err}"
    );
    assert!(
        !err.contains('\u{1b}') && !err.lines().any(|line| line.starts_with("held forged")),
        "{err:?}"
    );
    exits(&a, 0, &["sync"]);
    assert!(
        !remote_tree(&a)
            .iter()
            .any(|key| key.contains("evil") || key.contains("esc")),
        "never captured"
    );
}

#[test]
fn a_new_file_that_differs_only_in_case_from_a_managed_one_is_held() {
    let a = device();
    a.write(".config/nvim/Init.lua", b"-- other\n", 0o644);
    assert_eq!(
        a.token_of("status", ".config/nvim/Init.lua").as_deref(),
        Some("held")
    );
    let human = String::from_utf8_lossy(&a.run(&["status"]).stdout).into_owned();
    assert!(
        human.contains("Init.lua (differs only in case or Unicode normalization"),
        "{human}"
    );
    exits(&a, 3, &["sync"]);
    assert!(!remote_tree(&a).contains(&".config/nvim/Init.lua".to_owned()));
}

#[test]
fn forgetting_a_subdirectory_ignores_it_and_odd_names_are_escaped() {
    let a = device();
    a.write(".config/nvim/old/x.lua", b"x\n", 0o644);
    let odd = ".config/nvim/we{ir}d*[1]?.lua";
    a.write(odd, b"odd\n", 0o644);
    let twin = ".config/nvim/weird.lua";
    a.write(twin, b"twin\n", 0o644);
    // Matches `odd` if its glob characters were left unescaped.
    let lookalike = ".config/nvim/we{ir}dabc1Z.lua";
    a.write(lookalike, b"look\n", 0o644);
    a.ok(&["add", odd, twin, lookalike]);
    exits(&a, 0, &["sync"]);

    a.ok(&["forget", ".config/nvim/old", odd]);
    let patterns = a.repo_config().ignore.patterns;
    assert!(
        patterns.contains(&".config/nvim/old/**".to_owned()),
        "{patterns:?}"
    );
    exits(&a, 0, &["sync"]);

    let keys: Vec<String> = a
        .porcelain("status")
        .into_iter()
        .map(|(_, key)| key)
        .collect();
    assert!(
        !keys.iter().any(|key| key.contains("old/") || key == odd),
        "{keys:?}"
    );
    for kept in [twin, lookalike] {
        assert!(keys.contains(&kept.to_owned()), "{keys:?}");
    }
    assert_eq!(a.read(".config/nvim/old/x.lua"), b"x\n");
}
