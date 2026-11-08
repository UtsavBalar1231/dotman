//! `dotman import` and `dotman import --overlay` on git trees laid out relative to `$HOME`. Every
//! fixture is built with git commands, and every test checks that the source is untouched.

mod common;

use common::{
    File, Source, TestEnv, accounting, bare, git_in, tree_checksum, work_tree, yadm_fixture,
    yadm_repo,
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
fn an_import_of_a_repo_without_commits_is_refused() {
    let env = TestEnv::new();
    let empty = env.root().join("empty.git");
    env.git(&["init", "-q", "--bare", &empty.to_string_lossy()]);
    let err = env.fails(&["import", &empty.to_string_lossy()]);
    assert!(err.contains("no commits"), "{err}");
    assert!(!env.repo().exists(), "nothing is left behind");
}
