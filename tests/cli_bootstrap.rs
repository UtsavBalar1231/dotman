//! The `install.sh` that `dotman init` and `dotman import` write, and the script run the way a container
//! platform runs it: `sh install.sh` in a depth-1 clone, with a PATH that has no dotman.

mod common;

use common::{
    BOOTSTRAP_BASHRC as BASHRC, INSTALLER_TOOLS, R, Setup, TestEnv, archive_name, curl_available,
    release, text, toolbox, write_exe,
};
use dotman::VERSION;
use dotman::state::State;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use xxhash_rust::xxh3::xxh3_128;

const SCRIPT: &str = include_str!("../src/bootstrap/install.sh.in");

/// A `dotman` that answers `--version` as `version` and, for anything else, logs its arguments and
/// environment to `log` and exits `code`.
fn fake_dotman(dir: &Path, version: &str, code: i32, log: &Path) -> R {
    write_exe(
        &dir.join("dotman"),
        &format!(
            "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'dotman {version}';;\n  *) echo \"$*\" > '{log}'\n     echo \"${{DOTMAN_NON_INTERACTIVE:-unset}} ${{DOTMAN_ALLOW_ROOT:-unset}}\" >> '{log}'\n     exit {code};;\nesac\n",
            log = log.display()
        ),
    )
}

#[test]
fn the_script_installs_the_release_and_applies_the_repo() -> R {
    if !curl_available() {
        return Ok(());
    }
    let setup = Setup::new()?;
    let out = setup.run(&[])?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let dotman = setup.home().join(".local/bin/dotman");
    assert_eq!(fs::metadata(&dotman)?.permissions().mode() & 0o777, 0o755);
    assert!(setup.applied(), "{}", text(&out));
    assert!(!setup.home().join("install.sh").exists());
    for entry in fs::read_dir(setup.home().join(".local/bin"))? {
        let name = entry?.file_name();
        assert!(name == "dotman" || name == "dot", "left behind: {name:?}");
    }
    Ok(())
}

#[test]
fn a_checksum_mismatch_or_a_missing_entry_installs_nothing() -> R {
    if !curl_available() {
        return Ok(());
    }
    let setup = Setup::new()?;
    let name = archive_name();
    let tampered = setup.machine.root().join("tampered");
    fs::create_dir_all(&tampered)?;
    symlink(release()?.join(&name), tampered.join(&name))?;
    for (checksums, why) in [
        (format!("{}  {name}\n", "0".repeat(64)), "NOT"),
        (String::new(), "no entry"),
    ] {
        fs::write(tampered.join("checksums.txt"), checksums)?;
        let out = setup.command(&tampered, &[]).output()?;
        assert_eq!(out.status.code(), Some(1), "{why}: {}", text(&out));
        assert!(!setup.installed(), "{why}");
        assert!(!setup.applied(), "{why}");
    }
    Ok(())
}

#[test]
fn a_dotman_of_the_pinned_version_on_path_is_reused_without_a_download() -> R {
    if !curl_available() {
        return Ok(());
    }
    let setup = Setup::new()?;
    let bin = setup.stubs("bin");
    fs::create_dir_all(&bin)?;
    symlink(release()?.join("pkg/dotman"), bin.join("dotman"))?;
    let out = setup.command(&setup.empty_base()?, &[&bin]).output()?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(!setup.installed());
    assert!(setup.applied(), "{}", text(&out));
    Ok(())
}

#[test]
fn a_dotman_that_is_foreign_or_older_is_not_reused() -> R {
    if !curl_available() {
        return Ok(());
    }
    let log = |setup: &Setup| setup.machine.root().join("fake.log");
    let setup = Setup::new()?;
    let foreign = setup.stubs("foreign");
    write_exe(
        &foreign.join("dotman"),
        "#!/bin/sh\necho 'dotman - some other tool 14.0.0'\nexit 0\n",
    )?;
    let out = setup.run(&[&foreign])?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(setup.installed(), "the download did not happen");
    assert!(setup.applied());

    let setup = Setup::new()?;
    let older = setup.stubs("older");
    fake_dotman(&older, "0.0.0", 7, &log(&setup))?;
    let out = setup.run(&[&older])?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(setup.installed());
    assert!(!log(&setup).exists(), "the older dotman ran init");
    Ok(())
}

fn dot_alias(setup: &Setup) -> PathBuf {
    setup.home().join(".local/bin/dot")
}

#[test]
fn a_dot_on_path_is_never_shadowed() -> R {
    if !curl_available() {
        return Ok(());
    }
    let setup = Setup::new()?;
    let graphviz = setup.stubs("graphviz");
    write_exe(
        &graphviz.join("dot"),
        "#!/bin/sh\necho 'dot - graphviz version 14.0.0 (20250101.0000)'\ncase \"$1\" in -V|--version) exit 0;; esac\nexit 1\n",
    )?;
    let out = setup.run(&[&graphviz])?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(setup.installed());
    assert!(setup.applied());
    assert!(
        fs::symlink_metadata(dot_alias(&setup)).is_err(),
        "a dot alias was created"
    );
    assert!(
        text(&out).contains("dotman is the command"),
        "{}",
        text(&out)
    );
    Ok(())
}

/// Runs the script with a `PATH` of only the tools it needs, so no `dotman` can be found.
fn run_without_dot(setup: &Setup) -> R<Output> {
    let tools = toolbox(&setup.machine.root().join("tools"), INSTALLER_TOOLS)?;
    Ok(setup
        .shell_on(&release()?, &[setup.uname.clone(), tools])
        .arg(setup.dotfiles.join("install.sh"))
        .output()?)
}

#[test]
fn with_no_dot_on_path_dot_becomes_a_symlink_to_dotman() -> R {
    if !curl_available() {
        return Ok(());
    }
    let setup = Setup::new()?;
    let out = run_without_dot(&setup)?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(setup.installed());
    assert!(setup.applied(), "{}", text(&out));
    assert_eq!(fs::read_link(dot_alias(&setup))?, Path::new("dotman"));
    let said = String::from_utf8(
        Command::new(dot_alias(&setup))
            .arg("--version")
            .output()?
            .stdout,
    )?;
    assert_eq!(said, format!("dotman {VERSION}\n"));

    let again = run_without_dot(&setup)?;
    assert_eq!(again.status.code(), Some(0), "{}", text(&again));
    assert_eq!(fs::read_link(dot_alias(&setup))?, Path::new("dotman"));
    Ok(())
}

#[test]
fn a_file_named_dot_that_path_cannot_see_is_left_alone() -> R {
    if !curl_available() {
        return Ok(());
    }
    let setup = Setup::new()?;
    fs::create_dir_all(setup.home().join(".local/bin"))?;
    fs::write(dot_alias(&setup), "mine\n")?;
    let out = run_without_dot(&setup)?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(setup.installed());
    assert_eq!(fs::read_to_string(dot_alias(&setup))?, "mine\n");
    Ok(())
}

#[test]
fn dotman_gets_the_directory_and_the_environment_and_exit_3_is_success() -> R {
    if !curl_available() {
        return Ok(());
    }
    let setup = Setup::new()?;
    let log = setup.machine.root().join("fake.log");
    let bin = setup.stubs("bin");
    fake_dotman(&bin, VERSION, 3, &log)?;
    let root = String::from_utf8(Command::new("id").arg("-u").output()?.stdout)?.trim() == "0";
    let allow_root = if root { "1" } else { "unset" };
    let dir = setup.dotfiles.display();

    let out = setup
        .shell(&setup.empty_base()?, &[&bin])
        .current_dir(&setup.dotfiles)
        .arg("install.sh")
        .output()?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        fs::read_to_string(&log)?,
        format!("init {dir} --take-repo\n1 {allow_root}\n")
    );

    let out = setup
        .command(&setup.empty_base()?, &[&bin])
        .env("DOTMAN_APPROVE_HOOKS", "1")
        .output()?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(
        fs::read_to_string(&log)?,
        format!("init {dir} --take-repo --approve-hooks\n1 {allow_root}\n")
    );

    fake_dotman(&bin, VERSION, 7, &log)?;
    let out = setup.command(&setup.empty_base()?, &[&bin]).output()?;
    assert_eq!(out.status.code(), Some(7), "{}", text(&out));
    Ok(())
}

/// A `dotman` whose first `refusals` runs, other than `--version` and `status`, say that
/// `--approve-hooks` approved nothing and exit 3; every such run appends its arguments to `log`.
fn refusing_dotman(dir: &Path, refusals: usize, log: &Path) -> R {
    write_exe(
        &dir.join("dotman"),
        &format!(
            "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'dotman {VERSION}';;\n  status) exit 1;;\n  *) echo \"$*\" >> '{log}'\n     if [ \"$(wc -l < '{log}')\" -le {refusals} ]; then\n       echo '--approve-hooks approved nothing: the [hooks] table in .dotman.toml changed since it was last shown on this machine' >&2\n       exit 3\n     fi;;\nesac\n",
            log = log.display()
        ),
    )
}

#[test]
fn approving_hooks_runs_the_approval_once_more_when_it_approved_nothing_and_never_loops() -> R {
    for (approve, refusals, code) in [(true, 1, 0), (true, 2, 1), (true, 9, 1), (false, 9, 0)] {
        let setup = Setup::new()?;
        let log = setup.machine.root().join("fake.log");
        let bin = setup.stubs("bin");
        refusing_dotman(&bin, refusals, &log)?;
        let mut cmd = setup.command(&setup.empty_base()?, &[&bin]);
        if approve {
            cmd.env("DOTMAN_APPROVE_HOOKS", "1");
        }
        let out = cmd.output()?;
        let said = text(&out);
        assert_eq!(out.status.code(), Some(code), "{refusals}: {said}");
        let init = format!("init {} --take-repo", setup.dotfiles.display());
        let runs = if approve {
            format!("{init} --approve-hooks\napply --approve-hooks\n")
        } else {
            format!("{init}\n")
        };
        assert_eq!(fs::read_to_string(&log)?, runs, "{refusals}: {said}");
        assert_eq!(
            said.contains("install.sh: the repo's hooks are not approved"),
            code != 0,
            "{refusals}: {said}"
        );
        assert!(said.contains("--approve-hooks approved nothing"), "{said}");
    }
    Ok(())
}

/// A container whose first run held hooks back reruns `install.sh` with `DOTMAN_APPROVE_HOOKS=1`
/// after the table changed upstream without touching a hooked path: the rerun approves the table
/// it shows, instead of approving nothing and reporting success.
#[test]
fn a_rerun_with_approve_hooks_approves_a_table_that_changed_since_it_was_shown() -> R {
    if !curl_available() {
        return Ok(());
    }
    let setup = Setup::new()?;
    let author = &setup.author;
    let config = author.repo().join(".dotman.toml");
    let base = fs::read_to_string(&config)?;
    assert!(base.contains("[hooks]\n"), "{base}");
    let marker = setup.machine.root().join("hooklog");
    let entry =
        |glob: &str, word: &str| format!("\"{glob}\" = \"echo {word} >> '{}'\"", marker.display());
    let dotfiles = setup.dotfiles.to_string_lossy().into_owned();
    let publish = |entry: &str| {
        author.write_repo(
            ".dotman.toml",
            base.replacen("[hooks]\n", &format!("[hooks]\n{entry}\n"), 1)
                .as_bytes(),
        );
        author.ok(&["sync"]);
        setup.machine.git(&["-C", &dotfiles, "pull", "-q"]);
    };

    publish(&entry(".bashrc", "first"));
    let out = setup.run(&[])?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("1 hook not run"), "{}", text(&out));
    assert!(!marker.exists());

    publish(&entry(".profile", "second"));
    let out = setup.machine.run(&["git", "pull", "-q"]);
    assert!(out.status.success(), "{}", text(&out));

    let out = setup
        .command(&setup.empty_base()?, &[])
        .env("DOTMAN_APPROVE_HOOKS", "1")
        .output()?;
    let said = text(&out);
    assert_eq!(out.status.code(), Some(0), "{said}");
    assert!(said.contains("--approve-hooks approved nothing"), "{said}");
    let state = State::read(&setup.machine.state_dir().join("state.toml"))?.ok_or("no state")?;
    let approved = format!(r#"{{".profile":"echo second >> '{}'"}}"#, marker.display());
    assert_eq!(
        state.hooks.approved.as_deref(),
        Some(approved.as_str()),
        "{said}"
    );
    Ok(())
}

#[test]
fn a_rerun_applies_without_downloading_and_keeps_a_local_edit() -> R {
    if !curl_available() {
        return Ok(());
    }
    let setup = Setup::new()?;
    let out = setup.run(&[])?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    fs::write(setup.home().join(".bashrc"), "local edit\n")?;
    let out = setup.command(&setup.empty_base()?, &[]).output()?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_eq!(fs::read(setup.home().join(".bashrc"))?, b"local edit\n");
    Ok(())
}

#[test]
fn the_script_sees_x86_64_linux_on_every_host() -> R {
    let setup = Setup::new()?;
    let out = setup
        .shell(&setup.empty_base()?, &[])
        .args(["-c", "uname -s; uname -m; uname -n"])
        .output()?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let host = String::from_utf8(Command::new("uname").arg("-n").output()?.stdout)?;
    assert_eq!(
        String::from_utf8(out.stdout)?,
        format!("Linux\nx86_64\n{host}")
    );
    Ok(())
}

#[test]
fn macos_prints_the_install_command_and_exits_0() -> R {
    if !curl_available() {
        return Ok(());
    }
    let setup = Setup::new()?;
    let bin = setup.stubs("darwin");
    write_exe(&bin.join("uname"), "#!/bin/sh\necho Darwin\n")?;
    let out = setup.command(&setup.empty_base()?, &[&bin]).output()?;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let said = text(&out);
    assert!(said.contains(&format!("releases/tag/v{VERSION}")), "{said}");
    assert!(
        said.contains(&format!(
            "dotman init \"{}\" --take-repo",
            setup.dotfiles.display()
        )),
        "{said}"
    );
    assert!(!setup.installed());
    Ok(())
}

#[test]
fn the_template_sets_allow_root_only_inside_the_id_check() {
    let mut lines = SCRIPT.lines().skip_while(|line| !line.contains("id -u"));
    let check = lines.next().expect("an id -u check");
    assert!(check.trim_start().starts_with("if "), "{check}");
    assert_eq!(lines.next().map(str::trim), Some("DOTMAN_ALLOW_ROOT=1"));
    assert_eq!(
        SCRIPT.matches("DOTMAN_ALLOW_ROOT=1").count(),
        1,
        "it is set nowhere else"
    );
}

fn repo_script(env: &TestEnv) -> PathBuf {
    env.repo().join("install.sh")
}

fn header_of(path: &Path) -> R<String> {
    Ok(fs::read_to_string(path)?
        .lines()
        .nth(1)
        .unwrap_or_default()
        .to_owned())
}

/// Rewrites the script as an older `dotman` wrote it: the pinned version changed everywhere, and
/// the hash recomputed, so it still counts as unedited.
fn pin_old(path: &Path) -> R {
    let old = fs::read_to_string(path)?.replace(VERSION, "0.0.0");
    let (shebang, rest) = old.split_once('\n').ok_or("no shebang")?;
    let (_, body) = rest.split_once('\n').ok_or("no header")?;
    let hash = format!("{:032x}", xxh3_128(format!("{shebang}\n{body}").as_bytes()));
    fs::write(
        path,
        format!("{shebang}\n# generated by dotman 0.0.0 sha:{hash}\n{body}"),
    )?;
    Ok(())
}

#[test]
fn init_writes_an_executable_script_with_the_hash_header() -> R {
    let env = TestEnv::new();
    env.ok(&["init", "--no-import"]);
    let path = repo_script(&env);
    let body = fs::read_to_string(&path)?;
    assert_eq!(body.lines().next(), Some("#!/bin/sh"));
    let header = header_of(&path)?;
    assert!(
        header.starts_with(&format!("# generated by dotman {VERSION} sha:")),
        "{header}"
    );
    assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o755);
    assert!(body.contains(&format!("pinned={VERSION}\n")));
    Ok(())
}

#[test]
fn status_and_list_never_show_install_sh_and_apply_never_writes_it() {
    let env = TestEnv::new();
    env.write(".bashrc", BASHRC, 0o644);
    env.ok(&["init", "--no-import"]);
    env.ok(&["add", ".bashrc"]);
    for stage in ["before sync", "after sync"] {
        for command in ["status", "list"] {
            let out = env.run(&[command]);
            assert!(!text(&out).contains("install.sh"), "{stage}: {command}");
            let keys = env.porcelain(command);
            assert!(keys.iter().all(|(_, key)| key != "install.sh"), "{stage}");
        }
        env.ok(&["sync"]);
    }
    assert!(
        env.git(&["-C", &env.repo().to_string_lossy(), "ls-tree", "HEAD"])
            .contains("install.sh")
    );
    env.ok(&["apply"]);
    assert!(!env.home().join("install.sh").exists());
}

#[test]
fn an_unedited_script_is_refreshed_on_request_and_an_edited_one_never() -> R {
    let env = TestEnv::new();
    env.ok(&["init", "--no-import"]);
    let path = repo_script(&env);
    let current = fs::read_to_string(&path)?;

    pin_old(&path)?;
    assert_ne!(fs::read_to_string(&path)?, current);
    env.ok(&["init"]);
    assert!(
        header_of(&path)?.contains("dotman 0.0.0"),
        "a plain init on an existing repo leaves the script alone"
    );
    env.ok(&["init", "--refresh-bootstrap"]);
    assert_eq!(fs::read_to_string(&path)?, current);

    let edited = format!("{current}# mine\n");
    fs::write(&path, &edited)?;
    let said = env.ok(&["init", "--refresh-bootstrap"]);
    assert_eq!(fs::read_to_string(&path)?, edited);
    assert!(said.contains("install.sh was edited"), "{said}");

    fs::remove_file(&path)?;
    env.ok(&["init", "--refresh-bootstrap"]);
    assert_eq!(fs::read_to_string(&path)?, current);
    Ok(())
}

#[test]
fn a_rerun_of_init_that_creates_the_repo_keeps_an_edited_script_and_refreshes_an_old_one() -> R {
    let env = TestEnv::new();
    env.ok(&["init", "--no-import"]);
    let path = repo_script(&env);
    let current = fs::read_to_string(&path)?;
    let git_dir = env.repo().join(".git");

    let edited = format!("{current}# mine\n");
    fs::write(&path, &edited)?;
    fs::remove_dir_all(&git_dir)?;
    env.ok(&["init", "--no-import"]);
    assert_eq!(fs::read_to_string(&path)?, edited);

    pin_old(&path)?;
    fs::remove_dir_all(&git_dir)?;
    env.ok(&["init", "--no-import"]);
    assert_eq!(fs::read_to_string(&path)?, current);
    Ok(())
}

#[test]
fn no_bootstrap_and_dry_run_write_nothing_and_the_flags_conflict() {
    let env = TestEnv::new();
    let said = env.ok(&["init", "--no-import", "--dry-run"]);
    assert!(said.contains("would write install.sh"), "{said}");
    assert!(!env.repo().exists());

    env.ok(&["init", "--no-import", "--no-bootstrap"]);
    assert!(!repo_script(&env).exists());
    env.ok(&["init", "--refresh-bootstrap", "--dry-run"]);
    assert!(!repo_script(&env).exists());

    let out = env.run(&["init", "--no-bootstrap", "--refresh-bootstrap"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
}

fn work_tree(env: &TestEnv, name: &str) -> R<PathBuf> {
    let dir = env.root().join("work").join(name);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(".vimrc"), "set number\n")?;
    let dir_arg = dir.to_string_lossy().into_owned();
    env.git(&["-C", &dir_arg, "init", "-q"]);
    env.git(&["-C", &dir_arg, "symbolic-ref", "HEAD", "refs/heads/main"]);
    env.git(&["-C", &dir_arg, "add", "-A"]);
    env.git(&["-C", &dir_arg, "commit", "-q", "-m", "first"]);
    Ok(dir)
}

#[test]
fn import_writes_the_script_after_its_commits_and_sync_commits_it() -> R {
    let env = TestEnv::new();
    let source = work_tree(&env, "dots")?;
    let source_arg = source.to_string_lossy().into_owned();
    let out = env.run(&["import", &source_arg, "--dry-run"]);
    assert!(
        text(&out).contains("would write install.sh"),
        "{}",
        text(&out)
    );
    assert!(!env.repo().exists());

    let out = env.run(&["import", &source_arg]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(repo_script(&env).is_file(), "{}", text(&out));
    let repo = env.repo().to_string_lossy().into_owned();
    assert_eq!(
        env.git(&["-C", &repo, "status", "--porcelain"]),
        "?? install.sh\n"
    );
    env.ok(&["sync"]);
    assert!(
        env.git(&["-C", &repo, "ls-tree", "HEAD"])
            .contains("install.sh")
    );
    Ok(())
}

#[test]
fn import_with_no_bootstrap_writes_none_and_a_script_from_init_does_not_block_it() -> R {
    let env = TestEnv::new();
    let source = work_tree(&env, "dots")?;
    let source_arg = source.to_string_lossy().into_owned();
    let out = env.run(&["import", &source_arg, "--no-bootstrap"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!repo_script(&env).exists());

    let env = TestEnv::new();
    let source = work_tree(&env, "dots")?;
    let source_arg = source.to_string_lossy().into_owned();
    env.ok(&["init", "--no-import"]);
    assert!(repo_script(&env).is_file());
    let out = env.run(&["import", &source_arg]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(repo_script(&env).is_file());
    Ok(())
}

#[test]
fn an_edited_script_in_the_repo_blocks_an_import_that_would_replace_the_repo() -> R {
    let env = TestEnv::new();
    let source = work_tree(&env, "dots")?;
    env.ok(&["init", "--no-import"]);
    fs::write(repo_script(&env), "#!/bin/sh\necho mine\n")?;
    let out = env.run(&["import", &source.to_string_lossy()]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("install.sh"), "{}", text(&out));
    assert_eq!(
        fs::read_to_string(repo_script(&env))?,
        "#!/bin/sh\necho mine\n"
    );
    Ok(())
}

#[test]
fn doctor_warns_about_an_old_pin_and_only_notes_an_edited_script() -> R {
    let env = TestEnv::new();
    env.ok(&["init", "--no-import"]);
    let path = repo_script(&env);
    let line = |env: &TestEnv| -> String {
        String::from_utf8_lossy(&env.run(&["doctor"]).stdout)
            .lines()
            .find(|line| line.contains("install.sh"))
            .unwrap_or_default()
            .to_owned()
    };
    assert_eq!(line(&env).trim_end(), "pass  install.sh");

    pin_old(&path)?;
    let warned = line(&env);
    assert!(warned.starts_with("warn"), "{warned}");
    assert!(warned.contains("0.0.0"), "{warned}");
    assert!(
        warned.contains("dotman init --refresh-bootstrap"),
        "{warned}"
    );

    fs::write(&path, format!("{}# mine\n", fs::read_to_string(&path)?))?;
    let noted = line(&env);
    assert!(noted.starts_with("info"), "{noted}");
    assert!(noted.contains("edited"), "{noted}");
    Ok(())
}

#[test]
fn the_generated_script_passes_shellcheck() -> R {
    if Command::new("shellcheck")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: shellcheck is not installed");
        return Ok(());
    }
    let env = TestEnv::new();
    env.ok(&["init", "--no-import"]);
    let out = Command::new("shellcheck")
        .args(["-s", "sh"])
        .arg(repo_script(&env))
        .output()?;
    assert!(out.status.success(), "{}", text(&out));
    Ok(())
}

#[test]
fn an_overlay_right_after_an_import_takes_the_generated_script_into_its_commit() -> R {
    let env = TestEnv::new();
    let first = work_tree(&env, "first")?;
    let second = work_tree(&env, "second")?;
    fs::remove_file(second.join(".vimrc"))?;
    fs::write(second.join(".zshrc"), "export Z=1\n")?;
    let second_arg = second.to_string_lossy().into_owned();
    env.git(&["-C", &second_arg, "add", "-A"]);
    env.git(&["-C", &second_arg, "commit", "-q", "-m", "second"]);
    let out = env.run(&["import", &first.to_string_lossy()]);
    assert!(out.status.success(), "{}", text(&out));
    let repo = env.repo().to_string_lossy().into_owned();
    let count = || env.git(&["-C", &repo, "rev-list", "--count", "HEAD"]);
    let before = count();

    let path = repo_script(&env);
    let generated = fs::read_to_string(&path)?;
    fs::write(&path, format!("{generated}# mine\n"))?;
    let out = env.run(&["import", "--overlay", &second_arg, "--include-missing"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    fs::write(&path, &generated)?;

    let out = env.run(&["import", "--overlay", &second_arg, "--include-missing"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        text(&out)
            .lines()
            .any(|line| line.starts_with("committed: ")
                && line.ends_with(" (1 file, plus install.sh)")),
        "{}",
        text(&out)
    );
    assert_eq!(
        count().trim().parse::<u32>()?,
        before.trim().parse::<u32>()? + 1
    );
    let files = env.git(&["-C", &repo, "show", "--name-only", "--format=", "HEAD"]);
    assert!(
        files.contains("install.sh") && files.contains(".zshrc"),
        "{files}"
    );
    Ok(())
}
