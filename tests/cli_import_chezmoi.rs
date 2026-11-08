//! `dot import` on chezmoi sources. The tests that need chezmoi skip with a printed reason when
//! it is not on `PATH`, and fail when `DOTMAN_REQUIRE_CHEZMOI=1`. Every test checks that the
//! source is untouched.

mod common;

use common::{
    Source, TestEnv, accounting, files_containing, have_chezmoi, mode_of, source_tree as source,
    stub_chezmoi, tree_checksum,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn markers(env: &TestEnv) -> PathBuf {
    let dir = env.root().join("markers");
    fs::create_dir_all(&dir).expect("create markers");
    dir
}

fn marker(dir: &Path, name: &str) -> String {
    dir.join(name).to_string_lossy().into_owned()
}

fn template_that_touches(path: &str) -> String {
    format!("{{{{ output \"touch\" \"{path}\" }}}}")
}

fn script_that_touches(path: &str) -> String {
    format!("#!/bin/sh\ntouch '{path}'\n")
}

/// The fixture of the task: 5 entries, 3 of which convert.
fn done_when(env: &TestEnv, markers: &Path) -> Source {
    let template = format!(
        "{}[user]\n\tname = Rendered\n",
        template_that_touches(&marker(markers, "template"))
    );
    source(
        env,
        "cz",
        &[
            ("dot_bashrc", "export A=1\n"),
            ("private_dot_ssh/private_config", "Host x\n"),
            ("dot_local/bin/executable_x", "#!/bin/sh\necho x\n"),
            ("dot_gitconfig.tmpl", &template),
            (
                "run_once_x.sh",
                &script_that_touches(&marker(markers, "script")),
            ),
        ],
    )
}

fn git_mode(env: &TestEnv, path: &str) -> String {
    let repo = env.repo();
    env.git(&["-C", &repo.to_string_lossy(), "ls-tree", "HEAD", path])
        .split(' ')
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn repo_has(env: &TestEnv, key: &str) -> bool {
    fs::symlink_metadata(env.repo().join(key)).is_ok()
}

#[test]
fn a_default_import_keeps_modes_and_runs_neither_templates_nor_scripts() {
    if !have_chezmoi() {
        return;
    }
    let env = TestEnv::new();
    let marks = markers(&env);
    let source = done_when(&env, &marks);
    let out = env.run(&["import", &source.to_string_lossy()]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (5, 3, 2), "{err}");

    assert_eq!(
        fs::read_to_string(env.repo().join(".bashrc")).unwrap_or_default(),
        "export A=1\n"
    );
    assert!(repo_has(&env, ".ssh/config") && repo_has(&env, ".local/bin/x"));
    assert!(!repo_has(&env, ".gitconfig"));
    let modes = env.repo_config().modes;
    assert_eq!(modes.get(".ssh/config").map(String::as_str), Some("0600"));
    assert_eq!(modes.get(".ssh/").map(String::as_str), Some("0700"));
    assert_eq!(modes.len(), 2, "{modes:?}");
    assert_eq!(git_mode(&env, ".local/bin/x"), "100755");
    assert_eq!(git_mode(&env, ".bashrc"), "100644");
    assert!(!marks.join("template").exists() && !marks.join("script").exists());
    assert!(err.contains("dot_gitconfig.tmpl"), "{err}");
    assert!(err.contains("run_once_x.sh"), "{err}");
    assert!(
        err.contains("chezmoi's git history is not imported"),
        "{err}"
    );
    assert_eq!(
        env.git(&[
            "-C",
            &env.repo().to_string_lossy(),
            "log",
            "-1",
            "--format=%s"
        ])
        .trim(),
        format!("import: chezmoi {}", source.display())
    );

    env.ok(&["apply"]);
    assert_eq!(mode_of(&env.home().join(".ssh/config")), 0o600);
    assert_eq!(mode_of(&env.home().join(".ssh")), 0o700);
    assert_eq!(mode_of(&env.home().join(".local/bin/x")), 0o755);
    assert_eq!(mode_of(&env.home().join(".bashrc")), 0o644);
}

#[test]
fn render_templates_warns_runs_the_templates_and_still_skips_scripts() {
    if !have_chezmoi() {
        return;
    }
    let env = TestEnv::new();
    let marks = markers(&env);
    let source = done_when(&env, &marks);
    let out = env.run(&["import", "--render-templates", &source.to_string_lossy()]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (5, 4, 1), "{err}");
    assert!(err.contains("full function set"), "{err}");
    assert!(
        err.contains("dot_gitconfig.tmpl"),
        "the template is named: {err}"
    );
    assert!(err.contains(".gitconfig: a template"), "{err}");
    assert!(err.contains("plain file now"), "{err}");
    assert!(marks.join("template").exists(), "the template ran");
    assert!(!marks.join("script").exists(), "the script did not run");
    assert_eq!(
        fs::read_to_string(env.repo().join(".gitconfig")).unwrap_or_default(),
        "[user]\n\tname = Rendered\n"
    );
}

#[test]
fn a_hostile_source_cannot_make_chezmoi_run_anything_by_default() {
    if !have_chezmoi() {
        return;
    }
    let env = TestEnv::new();
    let marks = markers(&env);
    let touch = |name: &str| template_that_touches(&marker(&marks, name));
    let hostile = [
        (".chezmoiignore", format!("{}README.md\n", touch("ignore"))),
        (".chezmoiremove", touch("remove")),
        (".chezmoiexternal.toml.tmpl", touch("external")),
        (
            "modify_dot_edited",
            format!("#!/bin/sh\ntouch '{}'\ncat\n", marker(&marks, "modify")),
        ),
        (
            ".chezmoiscripts/run_x.sh",
            script_that_touches(&marker(&marks, "scripts")),
        ),
        ("dot_bashrc", "export A=1\n".to_owned()),
        ("README.md", "readme\n".to_owned()),
    ];
    let files: Vec<(&str, &str)> = hostile.iter().map(|(p, t)| (*p, t.as_str())).collect();
    let source = source(&env, "hostile", &files);

    // Control: chezmoi given the real source does run all of it.
    let control = env.root().join("control");
    let config = env.root().join("control.toml");
    fs::write(&config, "").expect("fixture");
    let ran = std::process::Command::new("chezmoi")
        .args(["--config", &config.to_string_lossy()])
        .args([
            "--persistent-state",
            &control.join("state").to_string_lossy(),
        ])
        .args(["--cache", &control.join("cache").to_string_lossy()])
        .args(["--source", &source.to_string_lossy()])
        .args(["--destination", &env.home().to_string_lossy()])
        .args(["--no-tty", "dump", "-f", "json"])
        .env_clear()
        .env("HOME", env.home())
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(env.root())
        .output();
    let ran = ran.expect("run chezmoi");
    assert!(
        ran.status.success(),
        "the control dump ran: {}",
        String::from_utf8_lossy(&ran.stderr)
    );
    for name in ["ignore", "remove", "external", "modify"] {
        assert!(marks.join(name).exists(), "the control did not run {name}");
        let _ = fs::remove_file(marks.join(name));
    }

    let out = env.run(&["import", &source.to_string_lossy()]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    let ran: Vec<_> = fs::read_dir(&marks)
        .map(|dir| dir.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(ran.is_empty(), "markers appeared: {ran:?}");
    assert_eq!(accounting(&err), (6, 1, 5), "{err}");
    assert!(repo_has(&env, ".bashrc") && !repo_has(&env, "README.md"));
    assert!(err.contains("modify_dot_edited"), "{err}");
}

#[test]
fn the_users_chezmoi_hooks_do_not_run_unless_templates_are_rendered() {
    if !have_chezmoi() {
        return;
    }
    let hook = |env: &TestEnv, mark: &Path| {
        let dir = env.xdg_config().join("chezmoi");
        fs::create_dir_all(&dir).expect("fixture");
        let text = format!(
            "[hooks.read-source-state.pre]\ncommand = \"touch\"\nargs = [\"{}\"]\n",
            mark.display()
        );
        fs::write(dir.join("chezmoi.toml"), text).expect("fixture");
    };
    let files = [("dot_bashrc", "export A=1\n")];

    let plain = TestEnv::new();
    let mark = markers(&plain).join("hook");
    hook(&plain, &mark);
    let source_plain = source(&plain, "cz", &files);
    let out = plain.run(&["import", &source_plain.to_string_lossy()]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!mark.exists(), "the hook ran in a default import");

    let rendering = TestEnv::new();
    let mark = markers(&rendering).join("hook");
    hook(&rendering, &mark);
    let source_render = source(&rendering, "cz", &files);
    let out = rendering.run(&[
        "import",
        "--render-templates",
        &source_render.to_string_lossy(),
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        mark.exists(),
        "the hook is the user's config at work, so it ran"
    );
}

#[test]
fn a_plain_chezmoiignore_reports_the_files_it_matches() {
    if !have_chezmoi() {
        return;
    }
    let env = TestEnv::new();
    let source = source(
        &env,
        "cz",
        &[
            (".chezmoiignore", "README.md\n**/*.bak\n# comment\n"),
            ("dot_bashrc", "export A=1\n"),
            ("README.md", "readme\n"),
            ("notes.bak", "old\n"),
            ("dot_config/app/old.bak", "old\n"),
            ("dot_config/app/keep.conf", "keep\n"),
        ],
    );
    let out = env.run(&["import", &source.to_string_lossy()]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (5, 2, 3), "{err}");
    for kept in [".bashrc", ".config/app/keep.conf"] {
        assert!(repo_has(&env, kept), "{kept}");
    }
    for ignored in ["README.md", "notes.bak", ".config/app/old.bak"] {
        assert!(!repo_has(&env, ignored), "{ignored}");
    }
    assert!(err.contains("ignored by .chezmoiignore"), "{err}");
    assert!(err.contains("add it with `dot add`"), "{err}");
    assert!(err.contains("never run"), "the caveat is given: {err}");
}

#[test]
fn a_templated_chezmoiignore_is_read_as_text_and_its_patterns_are_reported() {
    if !have_chezmoi() {
        return;
    }
    let env = TestEnv::new();
    let marks = markers(&env);
    let ignore = format!(
        "{}{{{{ if eq .chezmoi.os \"darwin\" }}}}Library/**{{{{ end }}}}\n",
        template_that_touches(&marker(&marks, "ignore"))
    );
    let source = source(
        &env,
        "cz",
        &[
            (".chezmoiignore", &ignore),
            ("dot_bashrc", "export A=1\n"),
            ("Library/Fonts/a.ttf", "font\n"),
            ("Library/Preferences/b.plist", "plist\n"),
        ],
    );
    let out = env.run(&["import", &source.to_string_lossy()]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (3, 1, 2), "{err}");
    assert!(repo_has(&env, ".bashrc"));
    assert!(!repo_has(&env, "Library/Fonts/a.ttf") && !repo_has(&env, "Library"));
    assert!(!marks.join("ignore").exists(), "the ignore file was run");
    assert!(err.contains("Library/Fonts/a.ttf"), "{err}");
}

#[test]
fn symlinks_are_kept_and_everything_chezmoi_does_by_itself_is_reported() {
    if !have_chezmoi() {
        return;
    }
    let env = TestEnv::new();
    let outside = env.root().join("outside-secret");
    fs::write(&outside, "TOP-SECRET-CONTENT\n").expect("fixture");
    let source = source(
        &env,
        "cz",
        &[
            ("dot_bashrc", "export A=1\n"),
            ("symlink_dot_link", ".bashrc\n"),
            (
                "symlink_dot_abs",
                &format!("{}/.config/x\n", env.home().display()),
            ),
            ("encrypted_dot_secret.age", "not really age\n"),
            ("modify_dot_edited", "#!/bin/sh\ncat\n"),
            ("remove_dot_old", ""),
            ("create_dot_once", "created\n"),
            ("exact_dot_dir/file", "in an exact dir\n"),
            ("empty_dot_blank", ""),
            ("dot_data.tmpl", "{{ .chezmoi.os }}\n"),
            (".chezmoiexternal.toml", "[\".x\"]\n"),
            (".chezmoidata.toml", "a = 1\n"),
            (".gitignore", "x\n"),
        ],
    );
    std::os::unix::fs::symlink(&outside, source.join("dot_escape")).expect("fixture");
    std::os::unix::fs::symlink("dot_bashrc", source.join("dot_inside")).expect("fixture");
    source.seal();

    let out = env.run(&["import", &source.to_string_lossy()]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    // bashrc, link, abs, secret, edited, old, once, dir/file, blank, data, external, escape,
    // inside; the empty_ file converts and `remove_dot_old` is a removal.
    assert_eq!(accounting(&err), (13, 6, 7), "{err}");

    let link = fs::read_link(env.repo().join(".link")).unwrap_or_default();
    assert_eq!(link, Path::new(".bashrc"));
    let abs = fs::read_link(env.repo().join(".abs")).unwrap_or_default();
    assert_eq!(abs, Path::new(".config/x"), "portable like `add` makes it");
    assert!(repo_has(&env, ".once") && repo_has(&env, ".dir/file") && repo_has(&env, ".blank"));
    assert_eq!(
        files_containing(&env.repo(), b"TOP-SECRET-CONTENT"),
        Vec::<String>::new()
    );
    for reported in [
        "encrypted_dot_secret.age",
        "modify_dot_edited",
        "remove_dot_old",
        ".chezmoiexternal.toml",
        "dot_data.tmpl",
        "dot_escape",
        "dot_inside",
    ] {
        assert!(err.contains(reported), "{reported} is reported: {err}");
    }
    assert!(err.contains("exact_ directories"), "{err}");
    assert!(err.contains("create_ file imported as managed"), "{err}");
    assert!(err.contains(".chezmoidata.toml"), "{err}");
    assert!(err.contains("1 dot-file left out"), "{err}");
}

#[test]
fn a_chezmoiroot_moves_the_source_into_a_subdirectory() {
    if !have_chezmoi() {
        return;
    }
    let env = TestEnv::new();
    let source = source(
        &env,
        "cz",
        &[
            (".chezmoiroot", "home\n"),
            ("README.md", "about the repo\n"),
            ("home/dot_bashrc", "export A=1\n"),
            ("home/private_dot_netrc", "machine x\n"),
        ],
    );
    let out = env.run(&["import", &source.to_string_lossy()]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert_eq!(accounting(&err), (2, 2, 0), "{err}");
    assert!(repo_has(&env, ".bashrc") && !repo_has(&env, "README.md"));
    assert_eq!(
        env.repo_config().modes.get(".netrc").map(String::as_str),
        Some("0600")
    );
}

#[test]
fn a_git_checkout_of_a_source_is_still_a_chezmoi_source_and_its_origin_is_reported() {
    if !have_chezmoi() {
        return;
    }
    let env = TestEnv::new();
    let source = source(&env, "cz", &[("dot_bashrc", "export A=1\n")]);
    let dir = source.to_string_lossy().into_owned();
    env.git(&["-C", &dir, "init", "-q", "-b", "main"]);
    env.git(&["-C", &dir, "add", "-A"]);
    env.git(&["-C", &dir, "commit", "-q", "-m", "chezmoi history"]);
    env.git(&[
        "-C",
        &dir,
        "remote",
        "add",
        "origin",
        "https://example.invalid/me/dots.git",
    ]);
    source.seal();
    let out = env.run(&["import", &dir]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("https://example.invalid/me/dots.git"), "{err}");
    assert!(repo_has(&env, ".bashrc") && !repo_has(&env, "dot_bashrc"));
    let repo = env.repo().to_string_lossy().into_owned();
    assert_eq!(
        env.git(&["-C", &repo, "rev-list", "--count", "HEAD"])
            .trim(),
        "1"
    );
    assert_eq!(env.git(&["-C", &repo, "remote"]).trim(), "");
}

#[test]
fn a_dry_run_reports_and_changes_nothing_anywhere() {
    if !have_chezmoi() {
        return;
    }
    let env = TestEnv::new();
    let marks = markers(&env);
    let source = done_when(&env, &marks);
    let before = env.four_trees();
    let out = env.run(&[
        "import",
        "--dry-run",
        "--render-templates",
        &source.to_string_lossy(),
    ]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (5, 3, 2), "{err}");
    assert!(err.contains("would import 3 files"), "{err}");
    assert!(err.contains("runs nothing in a dry run"), "{err}");
    assert_eq!(env.four_trees(), before);
    assert!(!env.repo().exists() && !env.state_dir().exists());
    assert!(fs::read_dir(&marks).is_ok_and(|mut dir| dir.next().is_none()));
}

#[test]
fn render_templates_and_overlay_do_not_take_what_they_do_not_apply_to() {
    let env = TestEnv::new();
    let plain = source(&env, "plain", &[("note.txt", "x\n")]);
    let out = env.run(&["import", "--render-templates", &plain.to_string_lossy()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("--render-templates only applies to a chezmoi source"),
        "{}",
        stderr(&out)
    );
    let chezmoi = source(&env, "cz", &[("dot_bashrc", "x\n")]);
    let out = env.run(&["import", "--overlay", &chezmoi.to_string_lossy()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("does not take a chezmoi source"),
        "{}",
        stderr(&out)
    );
}

/// A directory with nothing on it but a link to the real git.
// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn path_with_only_git(env: &TestEnv) -> PathBuf {
    let git = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git on PATH");
    let dir = env.root().join("only-git");
    fs::create_dir_all(&dir).expect("create dir");
    std::os::unix::fs::symlink(git, dir.join("git")).expect("link git");
    dir
}

#[test]
fn without_chezmoi_the_import_says_how_to_install_it() {
    let env = TestEnv::new();
    let source = source(&env, "cz", &[("dot_bashrc", "x\n")]);
    let only_git = path_with_only_git(&env);
    let home = tree_checksum(&env.home());
    let out = env.run_with(
        &[("PATH", &only_git.to_string_lossy())],
        &["import", &source.to_string_lossy()],
    );
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("chezmoi is not installed"), "{err}");
    assert!(err.contains("pacman -S chezmoi"), "{err}");
    assert!(err.contains("brew install chezmoi"), "{err}");
    assert!(err.contains("https://www.chezmoi.io/install/"), "{err}");
    assert_eq!(tree_checksum(&env.home()), home);
    assert!(!env.repo().exists());
}

#[test]
fn without_entry_types_managed_and_cat_read_the_plain_files_only() {
    let mut env = TestEnv::new();
    let log = stub_chezmoi(
        &mut env,
        &[
            ".bashrc",
            ".ssh",
            ".ssh/config",
            ".local",
            ".local/bin",
            ".local/bin/x",
            ".gitconfig",
        ],
        &[
            (".bashrc", "stub bashrc\n"),
            (".ssh/config", "stub ssh\n"),
            (".local/bin/x", "stub x\n"),
        ],
    );
    let marks = markers(&env);
    let source = done_when(&env, &marks);
    let out = env.run(&["import", &source.to_string_lossy()]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (5, 3, 2), "{err}");

    let asked = fs::read_to_string(&log).unwrap_or_default();
    assert!(
        asked.contains("help dump") && asked.contains("managed"),
        "{asked}"
    );
    assert!(!asked.contains("dump -f"), "{asked}");
    assert!(!asked.contains("--no-pager"), "{asked}");
    let cats: Vec<&str> = asked
        .lines()
        .filter(|line| line.starts_with("cat "))
        .collect();
    assert_eq!(cats.len(), 3, "only the plain files are read: {asked}");
    assert!(
        cats.iter().all(|line| !line.contains(".gitconfig")),
        "{asked}"
    );

    assert_eq!(
        fs::read_to_string(env.repo().join(".bashrc")).unwrap_or_default(),
        "stub bashrc\n"
    );
    let modes = env.repo_config().modes;
    assert_eq!(modes.get(".ssh/config").map(String::as_str), Some("0600"));
    assert_eq!(modes.get(".ssh/").map(String::as_str), Some("0700"));
    assert_eq!(git_mode(&env, ".local/bin/x"), "100755");
    assert!(!marks.join("template").exists() && !marks.join("script").exists());
}

#[test]
fn a_source_in_the_usual_place_or_named_by_the_config_is_found() {
    if !have_chezmoi() {
        return;
    }
    let env = TestEnv::new();
    let found = env.xdg_data().join("chezmoi");
    fs::create_dir_all(found.join("dot_config")).expect("fixture");
    fs::write(found.join("dot_bashrc"), "export A=1\n").expect("fixture");
    let _usual = Source::new(found);
    let out = env.run(&["import"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(repo_has(&env, ".bashrc"));

    let configured = TestEnv::new();
    let elsewhere = source(&configured, "dots", &[("dot_zshrc", "export B=2\n")]);
    let dir = configured.xdg_config().join("chezmoi");
    fs::create_dir_all(&dir).expect("fixture");
    fs::write(
        dir.join("chezmoi.toml"),
        format!("sourceDir = \"{}\"\n", elsewhere.display()),
    )
    .expect("fixture");
    let out = configured.run(&["import"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(repo_has(&configured, ".zshrc"));
}

#[test]
fn two_chezmoi_sources_are_listed_and_nothing_is_picked() {
    let env = TestEnv::new();
    let usual = env.xdg_data().join("chezmoi");
    fs::create_dir_all(&usual).expect("fixture");
    fs::write(usual.join("dot_bashrc"), "x\n").expect("fixture");
    let elsewhere = source(&env, "dots", &[("dot_zshrc", "y\n")]);
    let dir = env.xdg_config().join("chezmoi");
    fs::create_dir_all(&dir).expect("fixture");
    fs::write(
        dir.join("chezmoi.json"),
        format!("{{\"sourceDir\": \"{}\"}}", elsewhere.display()),
    )
    .expect("fixture");
    let home = tree_checksum(&env.home());
    let out = env.run(&["import"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(err.contains("several sources found"), "{err}");
    assert!(err.contains("# chezmoi source"), "{err}");
    assert!(err.contains(&usual.to_string_lossy().into_owned()), "{err}");
    assert!(
        err.contains(&elsewhere.to_string_lossy().into_owned()),
        "{err}"
    );
    assert_eq!(tree_checksum(&env.home()), home);
    assert!(!env.repo().exists());
}
