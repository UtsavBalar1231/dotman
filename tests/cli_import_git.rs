//! `dotman import` and `dotman import --overlay` on git trees laid out relative to `$HOME`. Every
//! fixture is built with git commands, and every test checks that the source is untouched.

mod common;

use common::{
    File, ODD_ORIGINS, Source, TestEnv, accounting, bare, check_origin_hint, git_in, tree_checksum,
    work_tree, yadm_fixture, yadm_repo,
};
use dotman::config::Config;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

const CONVERT: &str = "import: convert yadm alternates";

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn commits(env: &TestEnv, repo: &Path) -> usize {
    git_in(env, repo, &["rev-list", "--count", "HEAD"])
        .trim()
        .parse()
        .expect("a count")
}

fn tree_names(env: &TestEnv, repo: &Path) -> Vec<String> {
    git_in(env, repo, &["ls-tree", "-r", "--name-only", "HEAD"])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn blob(env: &TestEnv, repo: &Path, path: &str) -> String {
    git_in(env, repo, &["show", &format!("HEAD:{path}")])
}

// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn yadm_config(env: &TestEnv, key: &str, value: &str) {
    let file = env.xdg_config().join("yadm/config");
    fs::create_dir_all(file.parent().expect("parent")).expect("create yadm dir");
    env.git(&[
        "config",
        "--file",
        &file.to_string_lossy(),
        "--add",
        key,
        value,
    ]);
}

#[test]
fn yadm_alternates_become_one_commit_and_the_accounting_holds() {
    let env = TestEnv::new();
    let _source = yadm_fixture(&env);
    yadm_config(&env, "local.class", "work");
    let out = env.run(&["import"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (13, 8, 5), "{err}");

    let repo = env.repo();
    assert_eq!(commits(&env, &repo), 3, "the original two plus one");
    assert_eq!(
        git_in(&env, &repo, &["log", "-1", "--format=%s"]).trim(),
        CONVERT
    );
    let mut names = tree_names(&env, &repo);
    names.sort();
    assert_eq!(
        names,
        [
            ".bashrc",
            ".dotman.toml",
            ".toolrc{tmpl}",
            ".zshrc{os=linux}",
            ".zshrc{os=macos}",
            "README.md",
            "bin/run",
            "neg{os!=macos}",
            "sub",
            "x{host=box+profile=work}",
        ]
    );
    assert_eq!(blob(&env, &repo, ".zshrc{os=macos}"), "mac\n");
    assert_eq!(blob(&env, &repo, ".zshrc{os=linux}"), "linux\n");
    let template = blob(&env, &repo, ".toolrc{tmpl}");
    assert!(template.contains("{% if os == \"linux\" %}"), "{template}");
    assert!(!template.contains("yadm."), "{template}");
    assert!(template.contains("{{ env.EDITOR }}"), "{template}");
    assert_eq!(
        git_in(&env, &repo, &["ls-tree", "HEAD", "bin/run"])
            .split(' ')
            .next(),
        Some("100755")
    );

    assert!(err.contains("EDITOR"), "the env name is reported: {err}");
    assert!(err.contains("templates for esh, envtpl or j2"), "{err}");
    assert!(err.contains("esh.conf##template.esh"), "{err}");
    assert!(err.contains("{% include %}"), "{err}");
    assert!(err.contains("alternates on a directory"), "{err}");
    assert!(err.contains("host.conf##h.my.host"), "{err}");
    assert!(err.contains("submodules"), "{err}");
    assert!(err.contains("vendor or install"), "{err}");
    assert!(
        err.contains("README.md"),
        "the unmanaged root file is named: {err}"
    );
    assert!(err.contains("never applied"), "{err}");
    assert!(
        err.contains("dotman git show 'HEAD~1:host.conf##h.my.host'"),
        "{err}"
    );
    assert!(err.contains("other yadm machines"), "{err}");
    assert!(
        err.contains("run `dotman status`, then `dotman apply`"),
        "{err}"
    );
}

#[test]
fn what_cannot_be_renamed_is_removed_from_the_tree_and_still_in_history() {
    let env = TestEnv::new();
    let _source = yadm_fixture(&env);
    let out = env.run(&["import"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let repo = env.repo();
    for gone in [
        "dir##o.Linux/file",
        "host.conf##h.my.host",
        "esh.conf##template.esh",
        "inc.conf##template",
    ] {
        assert!(!tree_names(&env, &repo).iter().any(|n| n == gone), "{gone}");
        assert!(!repo.join(gone).exists(), "{gone}");
    }
    let shown = env.run(&["git", "show", "HEAD~1:dir##o.Linux/file"]);
    assert_eq!(String::from_utf8_lossy(&shown.stdout), "dir\n");
}

#[test]
fn local_class_and_identity_reach_the_machine_config() {
    let env = TestEnv::new();
    let _source = yadm_fixture(&env);
    yadm_config(&env, "local.class", "work");
    yadm_config(&env, "local.class", "home");
    yadm_config(&env, "local.hostname", "Box");
    yadm_config(&env, "local.os", "Darwin");
    let out = env.run(&["import"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    let text = fs::read_to_string(env.xdg_config().join("dotman/config.toml")).expect("config");
    let (config, _) = Config::parse(&text).expect("parse");
    assert_eq!(config.profiles, ["work", "home"]);
    assert_eq!(config.identity.host.as_deref(), Some("Box"));
    assert_eq!(config.identity.os.as_deref(), Some("macos"));
}

#[test]
fn a_dry_run_changes_nothing_anywhere() {
    let env = TestEnv::new();
    let _source = yadm_fixture(&env);
    yadm_config(&env, "local.class", "work");
    let before = env.four_trees();
    let config_before = tree_checksum(&env.xdg_config());
    let out = env.run(&["import", "--dry-run"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (13, 8, 5), "{err}");
    assert!(err.contains("would import 2 commits"), "{err}");
    assert!(err.contains("would go into"), "{err}");
    assert_eq!(env.four_trees(), before);
    assert_eq!(tree_checksum(&env.xdg_config()), config_before);
    assert!(!env.repo().exists() && !env.state_dir().exists());
}

#[test]
fn yadm_leftovers_in_home_are_listed_and_left_alone() {
    let env = TestEnv::new();
    let _source = yadm_fixture(&env);
    env.write(".zshrc##o.Linux", b"linux\n", 0o644);
    env.symlink(".zshrc", ".zshrc##o.Linux");
    let home_before = tree_checksum(&env.home());
    let out = env.run(&["import"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(err.contains(".zshrc -> .zshrc##o.Linux"), "{err}");
    assert!(err.contains("dotman import --swap"), "{err}");
    assert!(err.contains("can be removed once yadm is gone"), "{err}");
    assert_eq!(tree_checksum(&env.home()), home_before);
}

#[test]
fn yadm_encrypt_patterns_and_bootstrap_are_reported() {
    let env = TestEnv::new();
    let _source = yadm_fixture(&env);
    let dir = env.xdg_config().join("yadm");
    fs::create_dir_all(&dir).expect("create yadm dir");
    fs::write(dir.join("encrypt"), "# keys\n.ssh/id_*\n.gnupg/**\n").expect("encrypt");
    fs::write(dir.join("bootstrap"), "#!/bin/sh\n").expect("bootstrap");
    let err = stderr(&env.run(&["import"]));
    assert!(err.contains(".ssh/id_*, .gnupg/**"), "{err}");
    assert!(err.contains("dotman add --encrypt"), "{err}");
    assert!(err.contains("no run-once scripts"), "{err}");
}

#[test]
fn an_imported_yadm_repo_applies() {
    let env = TestEnv::new();
    let _source = yadm_fixture(&env);
    env.run(&["import"]);
    env.write_machine_config("[identity]\nos = \"linux\"\n\n[template]\nenv = [\"EDITOR\"]\n");
    env.ok_with(&[("EDITOR", "ed")], &["apply"]);
    assert_eq!(env.read(".bashrc"), b"bash\n");
    assert_eq!(env.read(".toolrc"), b"[core]\neditor = vim\n# ed\n");
    assert_eq!(env.read(".zshrc"), b"linux\n");
    assert!(!env.home().join(".zshrc{os=linux}").exists());
}

#[test]
fn a_template_that_reads_the_environment_renders_after_import() {
    let env = TestEnv::new();
    let work = work_tree(
        &env,
        "tmpl",
        &[(".editor##template", "{{ env.EDITOR }}\n", false)],
        None,
    );
    let _source = bare(&env, &work, &yadm_repo(&env), Some(&env.home()));
    let out = env.run(&["import"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("[template] env"), "{err}");
    env.write_machine_config("[template]\nenv = [\"EDITOR\"]\n");
    let applied = env.run_with(&[("EDITOR", "ed")], &["apply"]);
    assert!(applied.status.success(), "{}", stderr(&applied));
    assert_eq!(env.read(".editor"), b"ed\n");
}

#[test]
fn a_target_where_yadm_and_dotman_pick_different_files_is_reported() {
    let env = TestEnv::new();
    let work = work_tree(
        &env,
        "score",
        &[
            ("x##c.home,c.work", "classes\n", false),
            ("x##h.box", "host\n", false),
        ],
        None,
    );
    let _source = bare(&env, &work, &yadm_repo(&env), Some(&env.home()));
    yadm_config(&env, "local.class", "work");
    yadm_config(&env, "local.class", "home");
    let out = env.run_with(&[("DOTMAN_HOST", "box")], &["import"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (2, 1, 1), "{err}");
    assert!(
        err.contains("x: yadm applies x##c.home,c.work, dotman applies x{host=box}"),
        "{err}"
    );
    assert_eq!(
        tree_names(&env, &env.repo())
            .into_iter()
            .filter(|n| n.starts_with('x'))
            .collect::<Vec<_>>(),
        ["x{host=box}", "x{profile=home+profile=work}"]
    );
}

#[test]
fn a_plain_file_beats_a_default_alternate_and_the_alternate_is_reported() {
    let env = TestEnv::new();
    let work = work_tree(
        &env,
        "plain",
        &[("f", "plain\n", false), ("f##default", "alt\n", false)],
        None,
    );
    let _source = bare(&env, &work, &yadm_repo(&env), Some(&env.home()));
    let out = env.run(&["import"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (2, 1, 1), "{err}");
    assert!(err.contains("f##default"), "{err}");
    assert_eq!(blob(&env, &env.repo(), "f"), "plain\n");
}

#[test]
fn a_tree_without_alternates_gets_a_config_commit() {
    let env = TestEnv::new();
    let work = work_tree(&env, "p", &[(".vimrc", "set nu\n", false)], None);
    let _source = bare(&env, &work, &yadm_repo(&env), Some(&env.home()));
    let out = env.run(&["import"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert_eq!(accounting(&err), (1, 1, 0), "{err}");
    let repo = env.repo();
    assert_eq!(commits(&env, &repo), 3);
    assert_eq!(
        git_in(&env, &repo, &["log", "-1", "--format=%s"]).trim(),
        "import: add .dotman.toml"
    );
    assert!(repo.join(".git/dotman-id").exists());
}

fn dot_dir_bare(env: &TestEnv, files: &[File], worktree: Option<&Path>) -> (PathBuf, Source) {
    let work = work_tree(env, "cfg", files, None);
    let dir = bare(env, &work, &env.home().join(".cfg"), worktree);
    (work, dir)
}

#[test]
fn a_bare_repo_with_core_worktree_set_to_home_is_found() {
    let env = TestEnv::new();
    let (_, _dir) = dot_dir_bare(&env, &[(".vimrc", "v\n", false)], Some(&env.home()));
    let err = env.ok(&["import"]);
    assert_eq!(accounting(&err), (1, 1, 0), "{err}");
    assert_eq!(commits(&env, &env.repo()), 3);
}

#[test]
fn a_relative_core_worktree_resolves_against_the_git_dir() {
    let env = TestEnv::new();
    let (_, dir) = dot_dir_bare(&env, &[(".vimrc", "v\n", false)], None);
    git_in(&env, &dir, &["config", "core.worktree", ".."]);
    dir.seal();
    env.ok(&["import"]);
    assert_eq!(commits(&env, &env.repo()), 3);
}

#[test]
fn a_bare_repo_without_core_worktree_needs_half_its_files_in_home() {
    let env = TestEnv::new();
    let (_, _dir) = dot_dir_bare(
        &env,
        &[
            (".vimrc", "v\n", false),
            (".zshrc", "z\n", false),
            (".tmux.conf", "t\n", false),
            (".inputrc", "i\n", false),
        ],
        None,
    );
    let out = env.run(&["import"]);
    assert_eq!(out.status.code(), Some(1), "one in four is not enough");
    assert!(stderr(&out).contains("none found"), "{}", stderr(&out));

    env.write(".vimrc", b"v\n", 0o644);
    env.write(".zshrc", b"z\n", 0o644);
    let err = env.ok(&["import"]);
    assert_eq!(accounting(&err), (4, 4, 0), "{err}");
}

#[test]
fn a_bare_repo_whose_files_are_not_in_home_is_not_detected() {
    let env = TestEnv::new();
    let (_, dir) = dot_dir_bare(&env, &[(".vimrc", "v\n", false)], None);
    let out = env.run(&["import"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(!env.repo().exists());
    let other = env.root().join("elsewhere");
    fs::create_dir_all(&other).expect("create dir");
    git_in(
        &env,
        &dir,
        &["config", "core.worktree", &other.to_string_lossy()],
    );
    dir.seal();
    let out = env.run(&["import"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a worktree elsewhere is not $HOME"
    );
}

#[test]
fn several_candidates_are_listed_with_their_commands_and_nothing_is_picked() {
    let env = TestEnv::new();
    let _source = yadm_fixture(&env);
    let (_, dir) = dot_dir_bare(&env, &[(".vimrc", "v\n", false)], Some(&env.home()));
    let before = env.four_trees();
    let out = env.run(&["import"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(
        err.contains(&format!("dotman import {}", yadm_repo(&env).display())),
        "{err}"
    );
    assert!(
        err.contains(&format!("dotman import {}", dir.display())),
        "{err}"
    );
    let after = env.four_trees();
    assert_eq!(
        (&after[0], &after[1], &after[3]),
        (&before[0], &before[1], &before[3])
    );
    assert!(!env.repo().exists());
}

fn vcsh_fixture(env: &TestEnv) -> (Source, Source) {
    let dir = env.xdg_config().join("vcsh/repo.d");
    let zsh = work_tree(
        env,
        "zsh",
        &[(".zshrc", "z\n", false), (".zshenv", "e\n", false)],
        None,
    );
    let vim = work_tree(
        env,
        "vim",
        &[(".vimrc", "v\n", false), (".vim/colors/x.vim", "c\n", true)],
        None,
    );
    (
        bare(env, &zsh, &dir.join("zsh.git"), Some(&env.home())),
        bare(env, &vim, &dir.join("vim.git"), Some(&env.home())),
    )
}

#[test]
fn a_single_vcsh_repo_is_imported() {
    let env = TestEnv::new();
    let dir = env.xdg_config().join("vcsh/repo.d");
    let zsh = work_tree(&env, "zsh", &[(".zshrc", "z\n", false)], None);
    let _source = bare(&env, &zsh, &dir.join("zsh.git"), Some(&env.home()));
    let err = env.ok(&["import"]);
    assert_eq!(accounting(&err), (1, 1, 0), "{err}");
}

#[test]
fn two_vcsh_repos_are_listed_without_a_source() {
    let env = TestEnv::new();
    let (zsh, vim) = vcsh_fixture(&env);
    let out = env.run(&["import"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(
        err.contains(&format!("dotman import {}", zsh.display())),
        "{err}"
    );
    assert!(
        err.contains(&format!("dotman import {}", vim.display())),
        "{err}"
    );
}

#[test]
fn another_vcsh_repo_is_reported_and_added_by_overlay_as_one_commit() {
    let env = TestEnv::new();
    let (zsh, vim) = vcsh_fixture(&env);
    let out = env.run(&["import", &zsh.to_string_lossy(), "--no-bootstrap"]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert_eq!(accounting(&err), (2, 2, 0), "{err}");
    let command = format!("dotman import --overlay {}", vim.display());
    assert!(err.contains(&command), "{err}");

    let repo = env.repo();
    let before = commits(&env, &repo);
    let out = env.run(&["import", "--overlay", &vim.to_string_lossy()]);
    let err = stderr(&out);
    assert_eq!(
        out.status.code(),
        Some(3),
        "missing files are left out: {err}"
    );
    assert!(err.contains("--include-missing"), "{err}");
    assert_eq!(commits(&env, &repo), before, "no commit without the flag");

    let err = env.ok(&[
        "import",
        "--overlay",
        &vim.to_string_lossy(),
        "--include-missing",
    ]);
    assert_eq!(accounting(&err), (2, 2, 0), "{err}");
    assert_eq!(commits(&env, &repo), before + 1);
    let subject = git_in(&env, &repo, &["log", "-1", "--format=%s"]);
    assert!(subject.starts_with("import: overlay from "), "{subject}");
    assert!(subject.contains("vim.git"), "{subject}");
    assert_eq!(fs::read(repo.join(".vimrc")).expect("vimrc"), b"v\n");
    let mode = fs::metadata(repo.join(".vim/colors/x.vim"))
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111, "the source's executable bit");
}

#[test]
fn overlay_reports_differing_files_converts_alternates_and_never_changes_content() {
    let env = TestEnv::new();
    let work = work_tree(&env, "base", &[(".vimrc", "mine\n", false)], None);
    let own = bare(&env, &work, &env.root().join("own.git"), None);
    env.ok(&["import", &own.to_string_lossy(), "--no-bootstrap"]);
    let other = work_tree(
        &env,
        "other",
        &[
            (".vimrc", "theirs\n", false),
            (".zshrc##o.Linux", "z\n", false),
            ("ok", "same\n", false),
        ],
        None,
    );
    let source = bare(&env, &other, &env.root().join("other.git"), None);
    let repo = env.repo();
    let before = commits(&env, &repo);
    let out = env.run(&[
        "import",
        "--overlay",
        &source.to_string_lossy(),
        "--include-missing",
    ]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert_eq!(accounting(&err), (3, 2, 1), "{err}");
    assert!(err.contains("content differs"), "{err}");
    assert_eq!(fs::read(repo.join(".vimrc")).expect("vimrc"), b"mine\n");
    assert_eq!(
        fs::read(repo.join(".zshrc{os=linux}")).expect("zshrc"),
        b"z\n"
    );
    assert_eq!(commits(&env, &repo), before + 1);
}

#[test]
fn overlay_dry_run_and_a_dirty_repo() {
    let env = TestEnv::new();
    let (zsh, vim) = vcsh_fixture(&env);
    env.ok(&["import", &zsh.to_string_lossy(), "--no-bootstrap"]);
    let before = env.four_trees();
    let out = env.run(&[
        "import",
        "--overlay",
        &vim.to_string_lossy(),
        "--include-missing",
        "--dry-run",
    ]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(err.contains("would commit: import: overlay from"), "{err}");
    assert_eq!(env.four_trees(), before);

    env.write_repo("untracked", b"x");
    let out = env.run(&["import", "--overlay", &vim.to_string_lossy()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("uncommitted changes"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_local_path_import_has_no_origin_and_reports_the_sources_own() {
    let env = TestEnv::new();
    let work = work_tree(&env, "o", &[(".vimrc", "v\n", false)], None);
    let source = bare(&env, &work, &env.root().join("src.git"), None);
    git_in(
        &env,
        &source,
        &[
            "config",
            "remote.origin.url",
            "https://example.invalid/dots.git",
        ],
    );
    source.seal();
    let err = env.ok(&["import", &source.to_string_lossy()]);
    assert!(err.contains("https://example.invalid/dots.git"), "{err}");
    assert!(err.contains("dotman init --remote"), "{err}");
    let remotes = git_in(&env, &env.repo(), &["remote"]);
    assert_eq!(remotes.trim(), "");
}

#[test]
fn the_hint_for_the_sources_own_origin_runs_as_printed_whatever_the_url_holds() {
    for origin in ODD_ORIGINS {
        let env = TestEnv::new();
        let work = work_tree(&env, "o", &[(".vimrc", "v\n", false)], None);
        let source = bare(&env, &work, &env.root().join("src.git"), None);
        git_in(&env, &source, &["config", "remote.origin.url", origin]);
        source.seal();
        let err = env.ok(&["import", &source.to_string_lossy(), "--no-bootstrap"]);
        check_origin_hint(&env, &err, origin);
    }
}

#[test]
fn a_url_import_keeps_the_url_as_origin() {
    let env = TestEnv::new();
    let work = work_tree(&env, "u", &[(".zshrc##o.Linux", "z\n", false)], None);
    let source = bare(&env, &work, &env.root().join("src.git"), None);
    let url = format!("file://{}", source.display());
    let err = env.ok(&["import", &url]);
    assert!(err.contains("pushed by the next `dotman sync`"), "{err}");
    assert!(err.contains("other yadm machines"), "{err}");
    let origin = git_in(&env, &env.repo(), &["config", "remote.origin.url"]);
    assert_eq!(origin.trim(), url);
    assert_eq!(commits(&env, &env.repo()), 3);
}

#[test]
fn a_credential_in_a_url_never_reaches_the_report() {
    let env = TestEnv::new();
    // Port 9 on localhost refuses at once; the harness's https rewrite would put the credential
    // into git's own error text.
    let out = env.run_with(
        &[
            ("GIT_CONFIG_PARAMETERS", ""),
            ("GIT_ALLOW_PROTOCOL", "file:https"),
        ],
        &[
            "import",
            "--dry-run",
            "https://user:s3cret@127.0.0.1:9/dots.git",
        ],
    );
    assert_ne!(out.status.code(), Some(0));
    assert!(!stderr(&out).contains("s3cret"), "{}", stderr(&out));
}

#[test]
fn an_empty_repo_made_by_init_is_replaced_and_keeps_its_origin() {
    let env = TestEnv::new();
    let work = work_tree(&env, "i", &[(".vimrc", "v\n", false)], None);
    let source = bare(&env, &work, &env.root().join("src.git"), None);
    env.ok(&["init", "--remote", "https://example.invalid/mine.git"]);
    env.ok(&["import", &source.to_string_lossy()]);
    let repo = env.repo();
    assert_eq!(commits(&env, &repo), 3);
    let origin = git_in(&env, &repo, &["config", "remote.origin.url"]);
    assert_eq!(origin.trim(), "https://example.invalid/mine.git");
    assert!(repo.join(".git/dotman-id").exists());
}

#[test]
fn a_repo_directory_with_other_files_is_refused_before_anything_is_cloned() {
    let env = TestEnv::new();
    let work = work_tree(&env, "r", &[(".vimrc", "v\n", false)], None);
    let source = bare(&env, &work, &env.root().join("src.git"), None);
    fs::create_dir_all(env.repo()).expect("create repo dir");
    fs::write(env.repo().join("README"), "mine").expect("write");
    let err = env.fails(&["import", &source.to_string_lossy()]);
    assert!(err.contains("README"), "{err}");
    assert_eq!(fs::read(env.repo().join("README")).expect("kept"), b"mine");
}

/// A token shape, built here so no token-shaped literal sits in the source.
fn github_token() -> String {
    format!("ghp_{}", "Zq7".repeat(12))
}

/// A bare source of two commits: `.tokrc` holds a token in both, `.oldrc` only in the first, and
/// `.bashrc` none. With `tainted` false, neither holds one.
// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn history_source(env: &TestEnv, tainted: bool) -> Source {
    let work = env.root().join("work/history");
    fs::create_dir_all(&work).expect("create work tree");
    git_in(env, &work, &["init", "-q"]);
    git_in(env, &work, &["symbolic-ref", "HEAD", "refs/heads/main"]);
    let secret = if tainted {
        github_token()
    } else {
        "none".to_owned()
    };
    fs::write(work.join(".bashrc"), "alias ll=ls\n").expect("write");
    fs::write(work.join(".tokrc"), format!("export GH={secret}\n")).expect("write");
    fs::write(work.join(".oldrc"), format!("export OLD={secret}\n")).expect("write");
    git_in(env, &work, &["add", "-A"]);
    git_in(env, &work, &["commit", "-q", "-m", "one"]);
    fs::remove_file(work.join(".oldrc")).expect("remove");
    fs::write(work.join(".bashrc"), "alias ll=ls\nalias la=ls\n").expect("write");
    git_in(env, &work, &["add", "-A"]);
    git_in(env, &work, &["commit", "-q", "-m", "two"]);
    bare(env, &work, &env.root().join("history.git"), None)
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

/// The reviewer's case: a URL source whose history holds a token, in an old commit and in its
/// current tree. The import is one commit of the guarded tree, with no origin to push the old
/// history back to, so after a sync to a new remote no object anywhere holds the token. The
/// report names the files, the rule and the commits; the source is never changed.
#[test]
fn a_history_that_holds_a_credential_is_squashed_into_its_current_tree() {
    let env = TestEnv::new();
    let source = history_source(&env, true);
    let token = github_token();
    let url = format!("file://{}", source.display());
    let first = git_in(&env, &source, &["rev-parse", "main~1"]);
    let first = first.trim().get(..12).unwrap_or_default();

    let out = env.run(&["import", "--dry-run", &url]);
    assert!(
        stderr(&out).contains("would import the current tree of 2 commits as one commit"),
        "{}",
        stderr(&out)
    );
    let out = env.run(&["import", "--yes", &url]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    for line in [
        "the source's history holds credentials or age secret keys:",
        &format!("  .oldrc: github-token, added in {first}"),
        "  .tokrc: github-token, added in ",
        "so the import is one commit of the current tree",
        "the new repo has no remote, and the source cannot be one: the source still holds the history this import left out, credentials included",
        "Set a new, empty repo as the remote with `dotman init --remote <url>`",
        "imported the current tree of 2 commits as one commit into ",
    ] {
        assert!(err.contains(line), "{line}: {err}");
    }
    assert!(!err.contains(&token), "{err}");
    assert!(
        !err.contains("HEAD~1:"),
        "no history to show it from: {err}"
    );
    let repo = env.repo();
    assert_eq!(commits(&env, &repo), 1);
    assert_eq!(tree_names(&env, &repo), [".bashrc", ".dotman.toml"]);
    assert_eq!(blob(&env, &repo, ".bashrc"), "alias ll=ls\nalias la=ls\n");
    assert!(!every_object(&env, &repo.join(".git")).contains(&token));
    assert_eq!(git_in(&env, &repo, &["remote"]).trim(), "");
    assert!(repo.join(".git/dotman-id").exists());
    assert!(repo.join(".git/info/attributes").exists());

    env.ok(&["init", "--remote", &env.remote_url()]);
    env.ok(&["sync"]);
    let pushed = every_object(&env, &env.remote_path());
    assert!(pushed.contains("alias la=ls"), "the tree was pushed");
    assert!(!pushed.contains(&token));
}

/// `--keep-history` imports every commit of a history that holds a credential, and says so.
#[test]
fn keep_history_imports_a_credential_history_with_a_warning() {
    let env = TestEnv::new();
    let source = history_source(&env, true);
    let out = env.run(&[
        "import",
        "--yes",
        "--keep-history",
        &source.to_string_lossy(),
    ]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(err.contains("  .oldrc: github-token, added in "), "{err}");
    assert!(
        err.contains("--keep-history imports all 2 commits"),
        "{err}"
    );
    assert!(
        err.contains("imported 2 commits and added one conversion commit into "),
        "the same count as the warning: {err}"
    );
    assert_eq!(commits(&env, &env.repo()), 3);
}

/// A history the guard finds nothing in keeps every commit.
#[test]
fn a_clean_history_keeps_every_commit() {
    let env = TestEnv::new();
    let source = history_source(&env, false);
    let err = env.ok(&["import", "--yes", &source.to_string_lossy()]);
    assert!(!err.contains("history holds"), "{err}");
    assert!(
        err.contains("imported 2 commits and added one conversion commit into "),
        "{err}"
    );
    assert_eq!(commits(&env, &env.repo()), 3);
}

/// A bare source whose `main` is one clean commit. A token sits only where the import never
/// looks: on the branch `feature`, and in a commit only the annotated tag `v1` reaches.
// reason: clippy.toml exempts expect only inside #[test] fns
#[allow(clippy::expect_used)]
fn side_refs_source(env: &TestEnv) -> Source {
    let work = env.root().join("work/side");
    fs::create_dir_all(&work).expect("create work tree");
    let git = |args: &[&str]| git_in(env, &work, args);
    git(&["init", "-q"]);
    git(&["symbolic-ref", "HEAD", "refs/heads/main"]);
    fs::write(work.join(".bashrc"), "alias ll=ls\n").expect("write");
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "one"]);
    let secret = format!("export GH={}\n", github_token());
    git(&["checkout", "-q", "-b", "feature"]);
    fs::write(work.join(".featrc"), &secret).expect("write");
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "feature"]);
    git(&["checkout", "-q", "--detach", "main"]);
    fs::write(work.join(".tagrc"), &secret).expect("write");
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "tagged"]);
    git(&["tag", "-a", "v1", "-m", "v1"]);
    git(&["checkout", "-q", "main"]);
    bare(env, &work, &env.root().join("side.git"), None)
}

/// An import takes `HEAD`'s history and nothing else: a token only on another branch or behind a
/// tag neither squashes the import nor reaches the repo, with or without `--keep-history`, and
/// no tag or other branch comes along. The counts agree from the prompt to the last line.
#[test]
fn a_token_only_on_another_branch_or_tag_stays_out_and_nothing_squashes() {
    let token = github_token();
    for keep in [false, true] {
        let env = TestEnv::new();
        let source = side_refs_source(&env);
        let url = format!("file://{}", source.display());
        let mut args = vec!["import", "--yes"];
        if keep {
            args.push("--keep-history");
        }
        args.push(&url);
        let err = env.ok(&args);
        assert!(!err.contains("history holds"), "{err}");
        assert!(
            err.contains("imported 1 commit and added one conversion commit into "),
            "{err}"
        );
        let repo = env.repo();
        assert_eq!(commits(&env, &repo), 2);
        assert_eq!(tree_names(&env, &repo), [".bashrc", ".dotman.toml"]);
        assert_eq!(git_in(&env, &repo, &["tag", "-l"]), "");
        let refs = git_in(&env, &repo, &["for-each-ref", "--format=%(refname)"]);
        assert!(
            refs.lines()
                .all(|name| name.ends_with("/main") || name.ends_with("/HEAD")),
            "{refs}"
        );
        assert!(!every_object(&env, &repo.join(".git")).contains(&token));
    }
}

/// A token in a commit message counts as one in the history: the import is squashed, the report
/// names the commit, and the message reaches no object of the repo.
#[test]
fn a_token_in_a_commit_message_squashes_the_import() {
    let env = TestEnv::new();
    let token = github_token();
    let work = work_tree(&env, "msg", &[(".bashrc", "alias ll=ls\n", false)], None);
    git_in(
        &env,
        &work,
        &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            &format!("token {token}"),
        ],
    );
    let tip = git_in(&env, &work, &["rev-parse", "HEAD"]);
    let tip = tip.trim().get(..12).unwrap_or_default();
    let source = bare(&env, &work, &env.root().join("msg.git"), None);
    let out = env.run(&["import", "--yes", &source.to_string_lossy()]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(
        err.contains(&format!("  commit message: github-token, in {tip}")),
        "{err}"
    );
    assert!(
        err.contains("so the import is one commit of the current tree"),
        "{err}"
    );
    assert!(!err.contains(&token), "{err}");
    let repo = env.repo();
    assert_eq!(commits(&env, &repo), 1);
    assert!(!every_object(&env, &repo.join(".git")).contains(&token));
}

/// A token in a commit's author header is reported by that field, not as the commit message, and
/// squashes the import all the same.
#[test]
fn a_token_in_a_commit_author_is_named_by_its_field() {
    let env = TestEnv::new();
    let token = github_token();
    let work = work_tree(&env, "who", &[(".bashrc", "alias ll=ls\n", false)], None);
    let author = format!("bot {token} <bot@example.invalid>");
    git_in(
        &env,
        &work,
        &[
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "plain",
            "--author",
            &author,
        ],
    );
    let tip = git_in(&env, &work, &["rev-parse", "HEAD"]);
    let tip = tip.trim().get(..12).unwrap_or_default();
    let source = bare(&env, &work, &env.root().join("who.git"), None);
    let out = env.run(&["import", "--yes", &source.to_string_lossy()]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(
        err.contains(&format!("  commit author: github-token, in {tip}")),
        "{err}"
    );
    assert!(!err.contains("commit message"), "{err}");
    assert!(!err.contains(&token), "{err}");
    let repo = env.repo();
    assert_eq!(commits(&env, &repo), 1);
    assert!(!every_object(&env, &repo.join(".git")).contains(&token));
}

/// A source whose HEAD is detached gives a repo on a new branch, `main`, at the imported commit,
/// which the report names, so that sync has a branch to push.
#[test]
fn a_source_with_a_detached_head_is_imported_onto_a_new_main() {
    let env = TestEnv::new();
    let work = work_tree(&env, "loose", &[(".bashrc", "alias ll=ls\n", false)], None);
    git_in(&env, &work, &["checkout", "-q", "--detach", "HEAD~1"]);
    let detached = git_in(&env, &work, &["rev-parse", "HEAD"]);
    let source = Source::new(work);
    let out = env.run(&["import", "--yes", &source.to_string_lossy()]);
    let err = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{err}");
    assert!(
        err.contains(
            "the source's HEAD is detached, so the import puts its commit on a new branch, main"
        ),
        "{err}"
    );
    let repo = env.repo();
    assert_eq!(
        git_in(&env, &repo, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    let first = git_in(&env, &repo, &["rev-list", "--max-parents=0", "HEAD"]);
    assert_eq!(first.trim(), detached.trim(), "{err}");

    env.ok(&["init", "--remote", &env.remote_url()]);
    let err = env.ok(&["sync"]);
    assert!(err.contains("to origin/main"), "{err}");
    let remote = env.remote_path().to_string_lossy().into_owned();
    assert_eq!(
        env.git(&["--git-dir", &remote, "rev-parse", "main"]),
        git_in(&env, &repo, &["rev-parse", "HEAD"])
    );
}

/// The repo an import leaves passes `git fsck` and `dotman doctor`. Dropping `origin` after a
/// `--single-branch` clone of a local path once left `origin/HEAD` pointing at a deleted branch.
#[test]
fn an_import_leaves_no_dangling_remote_ref() {
    for kind in ["work tree", "bare", "url"] {
        let env = TestEnv::new();
        let work = work_tree(&env, "f", &[(".vimrc", "v\n", false)], None);
        let bare_source = bare(&env, &work, &env.root().join("src.git"), None);
        let source = match kind {
            "work tree" => work.to_string_lossy().into_owned(),
            "bare" => bare_source.to_string_lossy().into_owned(),
            _ => format!("file://{}", bare_source.display()),
        };
        env.ok(&["import", &source]);
        let repo = env.repo();
        let fsck = env.git_raw(&["-C", &repo.to_string_lossy(), "fsck"]);
        assert!(fsck.status.success(), "{kind}: {}", stderr(&fsck));
        let left = git_in(&env, &repo, &["for-each-ref", "refs/remotes/"]);
        if kind == "url" {
            assert!(left.contains("refs/remotes/origin/main"), "{kind}: {left}");
        } else {
            assert_eq!(left, "", "{kind}");
            let head = env.git_raw(&[
                "-C",
                &repo.to_string_lossy(),
                "symbolic-ref",
                "-q",
                "refs/remotes/origin/HEAD",
            ]);
            assert!(!head.status.success(), "{kind}: origin/HEAD is left");
        }
        let doctor = env.run(&["doctor"]);
        let said = String::from_utf8_lossy(&doctor.stdout);
        assert!(
            said.lines().any(|line| line == "pass  repo objects"),
            "{kind}: {said}"
        );
        // A local path leaves the repo with no remote, which doctor fails on its own.
        let fails: Vec<&str> = said
            .lines()
            .filter(|line| line.starts_with("fail"))
            .collect();
        let allowed = |line: &&str| kind != "url" && line.starts_with("fail  remote: ");
        assert!(fails.iter().all(allowed), "{kind}: {said}");
    }
}

/// Imported by URL, a source whose HEAD is detached while its `main` has moved on gives a repo
/// with no remote: sync would otherwise pull that `main`, which the import did not take, apply
/// its files and push to the source.
#[test]
fn a_url_import_of_a_detached_head_sets_no_origin() {
    let env = TestEnv::new();
    let work = work_tree(&env, "loose", &[(".bashrc", "alias ll=ls\n", false)], None);
    fs::write(work.join(".mainfile"), "main only\n").expect("write");
    git_in(&env, &work, &["add", "-A"]);
    git_in(&env, &work, &["commit", "-q", "-m", "main tip"]);
    git_in(&env, &work, &["checkout", "-q", "--detach", "HEAD~1"]);
    fs::write(work.join(".detfile"), "detached only\n").expect("write");
    git_in(&env, &work, &["add", "-A"]);
    git_in(&env, &work, &["commit", "-q", "-m", "detached tip"]);
    let main = git_in(&env, &work, &["rev-parse", "main"]);
    let source = Source::new(work);
    let url = format!("file://{}", source.display());

    let err = env.ok(&["import", "--yes", &url]);
    assert!(
        err.contains(
            "the new repo has no remote, and the source cannot be one: its HEAD is detached"
        ),
        "{err}"
    );
    let repo = env.repo();
    assert_eq!(git_in(&env, &repo, &["remote"]), "", "{err}");
    let fsck = env.git_raw(&["-C", &repo.to_string_lossy(), "fsck"]);
    assert!(fsck.status.success(), "{}", stderr(&fsck));

    env.ok(&["sync"]);
    assert_eq!(env.read(".detfile"), b"detached only\n");
    assert!(!env.home().join(".mainfile").exists());
    assert_eq!(git_in(&env, &source, &["rev-parse", "main"]), main);

    let remote = env.remote_url();
    let step = common::hint(&err, "init --remote");
    let step: Vec<&str> = step
        .iter()
        .map(|arg| if arg == "<url>" { remote.as_str() } else { arg })
        .collect();
    env.ok(&step);
    let err = env.ok(&["sync"]);
    assert!(err.contains("to origin/main"), "{err}");
    let pushed = env.remote_path().to_string_lossy().into_owned();
    assert_eq!(
        env.git(&["--git-dir", &pushed, "rev-parse", "main"]),
        git_in(&env, &repo, &["rev-parse", "HEAD"])
    );
    assert_eq!(git_in(&env, &source, &["rev-parse", "main"]), main);
}

#[test]
fn a_populated_repo_is_refused_naming_overlay() {
    let env = TestEnv::new();
    let work = work_tree(&env, "q", &[(".vimrc", "v\n", false)], None);
    let source = bare(&env, &work, &env.root().join("src.git"), None);
    env.ok(&["init"]);
    env.write(".bashrc", b"mine\n", 0o644);
    env.ok(&["add", ".bashrc"]);
    let err = env.fails(&["import", &source.to_string_lossy()]);
    assert!(err.contains("--overlay"), "{err}");
}

#[test]
fn a_directory_that_is_no_source_is_refused() {
    let env = TestEnv::new();
    let plain = env.root().join("plain");
    fs::create_dir_all(&plain).expect("create dir");
    let err = env.fails(&["import", &plain.to_string_lossy()]);
    assert!(err.contains("not a source"), "{err}");
    let err = env.fails(&["import", &env.root().join("missing").to_string_lossy()]);
    assert!(err.contains("does not exist"), "{err}");
}

#[test]
fn a_yadm_repo_in_the_places_older_yadm_used_is_found() {
    let homes: [fn(&TestEnv) -> PathBuf; 2] = [
        |env| env.home().join(".yadm/repo.git"),
        |env| env.xdg_config().join("yadm/repo.git"),
    ];
    for place in homes {
        let env = TestEnv::new();
        let work = work_tree(&env, "old", &[(".vimrc##o.Linux", "v\n", false)], None);
        let repo = place(&env);
        let _source = bare(&env, &work, &repo, Some(&env.home()));
        let out = env.run(&["import"]);
        let err = stderr(&out);
        assert_eq!(out.status.code(), Some(0), "{}: {err}", repo.display());
        assert_eq!(
            tree_names(&env, &env.repo()),
            [".dotman.toml", ".vimrc{os=linux}"],
            "{}",
            repo.display()
        );
    }
}

#[test]
fn a_stricter_mode_in_home_with_the_same_bytes_goes_into_modes() {
    let env = TestEnv::new();
    let files: [File; 3] = [
        (".ssh/config", "Host x\n", false),
        (".netrc", "machine x\n", false),
        (".vimrc", "set nu\n", false),
    ];
    let (_, _dir) = dot_dir_bare(&env, &files, Some(&env.home()));
    env.write(".ssh/config", b"Host x\n", 0o600);
    env.write(".netrc", b"machine edited\n", 0o600);
    env.write(".vimrc", b"set nu\n", 0o640);

    let err = env.ok(&["import", "--no-bootstrap"]);
    assert!(
        err.contains("1 file in $HOME has the same bytes as the source and a stricter mode than git keeps; [modes] records it: .ssh/config 0600"),
        "{err}"
    );
    let modes = env.repo_config().modes;
    assert_eq!(modes.get(".ssh/config").map(String::as_str), Some("0600"));
    assert!(
        !modes.contains_key(".netrc") && !modes.contains_key(".vimrc"),
        "{modes:?}"
    );
    assert_eq!(git_in(&env, &env.repo(), &["status", "--porcelain"]), "");

    env.ok(&["apply", "--take-repo"]);
    let mode = |key: &str| {
        fs::metadata(env.home().join(key))
            .expect("stat")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(".ssh/config"), 0o600);
}

#[test]
fn a_source_s_own_config_is_kept_byte_for_byte_and_the_modes_are_reported() {
    const OWN: &str = "# my settings\n[modes]\n\".netrc\" = \"0600\"\n";
    let env = TestEnv::new();
    let files: [File; 3] = [
        (".dotman.toml", OWN, false),
        (".ssh/config", "Host x\n", false),
        (".netrc", "machine x\n", false),
    ];
    let (_, _dir) = dot_dir_bare(&env, &files, Some(&env.home()));
    env.write(".ssh/config", b"Host x\n", 0o600);
    env.write(".netrc", b"machine x\n", 0o600);

    let err = env.ok(&["import", "--no-bootstrap"]);
    assert!(
        err.contains("the source's own .dotman.toml is kept as it is, so add these lines to its [modes] to keep them:")
            && err.contains("  \".ssh/config\" = \"0600\""),
        "{err}"
    );
    assert!(
        !err.contains("\".netrc\" = "),
        "already in its [modes]: {err}"
    );
    assert!(!err.contains("[modes] records it"), "{err}");
    assert_eq!(
        fs::read_to_string(env.repo().join(".dotman.toml")).expect("read"),
        OWN
    );

    let broken = TestEnv::new();
    let files: [File; 2] = [
        (".dotman.toml", "[modes\n", false),
        (".ssh/config", "Host x\n", false),
    ];
    let (_, _dir) = dot_dir_bare(&broken, &files, Some(&broken.home()));
    broken.write(".ssh/config", b"Host x\n", 0o600);
    let err = broken.ok(&["import", "--dry-run"]);
    assert!(
        err.contains("the source's own .dotman.toml (it does not parse yet) is kept as it is")
            && !err.contains("[modes] records it"),
        "{err}"
    );
}

#[test]
fn a_converted_template_that_prints_the_os_warns_that_its_case_changes() {
    let env = TestEnv::new();
    let files: [File; 1] = [(".profile##template", "export OS={{ yadm.os }}\n", false)];
    let work = work_tree(&env, "case", &files, None);
    let _source = bare(&env, &work, &yadm_repo(&env), Some(&env.home()));
    let err = env.ok(&["import", "--dry-run"]);
    assert!(
        err.contains("converted templates print yadm.os, which dotman spells in lowercase")
            && err.contains("rendered text changes case: .profile{tmpl}"),
        "{err}"
    );
}

#[test]
fn an_import_of_a_repo_without_commits_is_refused() {
    let env = TestEnv::new();
    let empty = env.root().join("empty.git");
    env.git(&["init", "-q", "--bare", &empty.to_string_lossy()]);
    let err = env.fails(&["import", &empty.to_string_lossy()]);
    assert!(err.contains("no commits"), "{err}");
    assert!(!env.repo().exists(), "nothing is left behind");
}

/// A staging directory a killed import left beside the repo is swept once it is stale, by the
/// next import and by `apply`: one named with a dead PID, or with none as older versions named it.
/// One whose PID is alive, or that was touched in the last ten minutes, stays. An import names its
/// own staging directory with its PID.
#[test]
fn the_staging_a_killed_import_left_is_swept() {
    use std::os::unix::fs::DirBuilderExt;
    let mut env = TestEnv::new();
    let parent = env.xdg_data();
    let old = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(1200))
        .expect("a time before now");
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("run true");
    let dead = child.id();
    child.wait().expect("wait for true");
    let stage = |name: &str, at: Option<std::time::SystemTime>| {
        let path = parent.join(name);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("make a staging dir");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("chmod");
        fs::create_dir(path.join("repo")).expect("a half clone");
        if let Some(at) = at {
            fs::File::open(&path)
                .and_then(|dir| dir.set_modified(at))
                .expect("age it");
        }
        path
    };
    let unnamed = stage(".dotman-import-a1B2c3", Some(old));
    let killed = stage(&format!(".dotman-import-{dead}-d1B2c3"), Some(old));
    let young = stage(".dotman-import-y1B2c3", None);
    let alive = stage(".dotman-import-1-l1B2c3", Some(old));

    let real = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH");
    let stubs = env.root().join("git-log");
    let log = env.root().join("git-args");
    fs::create_dir_all(&stubs).expect("stub dir");
    fs::write(
        stubs.join("git"),
        format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\nexec '{}' \"$@\"\n",
            log.display(),
            real.display()
        ),
    )
    .expect("write the wrapper");
    fs::set_permissions(stubs.join("git"), fs::Permissions::from_mode(0o755)).expect("chmod");
    env.prepend_path(&stubs);

    let work = work_tree(&env, "src", &[(".a", "a\n", false)], None);
    env.ok(&["import", &work.to_string_lossy(), "--no-bootstrap"]);
    assert!(!unnamed.exists() && !killed.exists());
    assert!(young.exists() && alive.exists());
    let args = fs::read_to_string(&log).expect("read the git log");
    let staging = format!("{}/.dotman-import-", parent.display());
    let rest = args
        .lines()
        .filter(|line| line.contains(" clone "))
        .find_map(|line| line.split_once(&staging))
        .map(|(_, rest)| rest)
        .expect("a clone into the staging dir");
    let pid: String = rest.chars().take_while(char::is_ascii_digit).collect();
    assert!(
        !pid.is_empty() && rest.strip_prefix(&pid).is_some_and(|r| r.starts_with('-')),
        "{args}"
    );

    let again = stage(".dotman-import-z1B2c3", Some(old));
    env.ok(&["apply"]);
    assert!(!again.exists());
    assert!(young.exists() && alive.exists());

    // A dry run stages in the temp directory and removes nothing there; `apply` does.
    let tmp = env.root().join("tmp");
    fs::create_dir_all(&tmp).expect("a temp dir");
    let in_tmp = tmp.join(".dotman-import-t1B2c3");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&in_tmp)
        .expect("make a staging dir");
    fs::set_permissions(&in_tmp, fs::Permissions::from_mode(0o700)).expect("chmod");
    fs::File::open(&in_tmp)
        .and_then(|dir| dir.set_modified(old))
        .expect("age it");
    let tmp_var = tmp.to_string_lossy().into_owned();
    let vars = [("TMPDIR", tmp_var.as_str())];
    let other = work_tree(&env, "other", &[(".b", "b\n", false)], None);
    let out = env.run_with(
        &vars,
        &["import", "--dry-run", "--overlay", &other.to_string_lossy()],
    );
    assert!(
        stderr(&out).contains(".b: not in the repo"),
        "the clone ran"
    );
    assert!(in_tmp.exists(), "{}", stderr(&out));
    env.ok_with(&vars, &["apply"]);
    assert!(!in_tmp.exists());
}
