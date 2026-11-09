//! `install/install.sh` against a `file://` fake release.

mod common;

use common::{INSTALLER_TOOLS, toolbox};
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

type R = Result<(), Box<dyn Error>>;

/// The checksum line for `archive` in `dir`, from `sha256sum` or else `shasum`, which is all that
/// stock macOS has. `None` when neither exists.
fn checksum_line(dir: &Path, archive: &str) -> Option<String> {
    [("sha256sum", &[][..]), ("shasum", &["-a", "256"][..])]
        .into_iter()
        .find_map(|(tool, args)| {
            let out = Command::new(tool)
                .args(args)
                .arg(archive)
                .current_dir(dir)
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8(out.stdout).ok())?
        })
}

/// Writes a release tarball holding a fake `dotman` plus `checksums.txt` into `release`, with the
/// tarball's checksum line replaced by `forced_line` when given. `false` when no checksum tool
/// exists, after saying so.
fn fake_release(
    root: &Path,
    release: &Path,
    forced_line: Option<&str>,
) -> Result<bool, Box<dyn Error>> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", arch) => format!("{arch}-unknown-linux-musl"),
        ("macos", arch) => format!("{arch}-apple-darwin"),
        (os, _) => return Err(format!("unsupported test platform {os}").into()),
    };
    let archive = format!("dotman-v{}-{target}.tar.gz", dotman::VERSION);
    let stage = root.join("stage");
    fs::create_dir_all(&stage)?;
    let dotman = stage.join("dotman");
    fs::write(&dotman, "#!/bin/sh\necho 'dotman 0.0.0-fake'\n")?;
    fs::set_permissions(&dotman, fs::Permissions::from_mode(0o755))?;
    fs::create_dir_all(release)?;
    let status = Command::new("tar")
        .args(["-czf"])
        .arg(release.join(&archive))
        .arg("-C")
        .arg(&stage)
        .arg("dotman")
        .status()?;
    assert!(status.success());
    let line = if let Some(forced) = forced_line {
        format!("{forced}  {archive}\n")
    } else if let Some(line) = checksum_line(release, &archive) {
        line
    } else {
        eprintln!("skipped: neither sha256sum nor shasum is installed");
        return Ok(false);
    };
    fs::write(release.join("checksums.txt"), line)?;
    Ok(true)
}

/// Runs the installer against `release`, installing into `root/bin`, with `path` as `PATH` when
/// given.
fn run_installer(
    root: &Path,
    release: &Path,
    path: Option<OsString>,
) -> Result<Output, Box<dyn Error>> {
    run_installer_in(root, release, path, &root.join("bin"), "/bin/bash")
}

/// [`run_installer`] into `dir`, as a user whose login shell is `shell`.
fn run_installer_in(
    root: &Path,
    release: &Path,
    path: Option<OsString>,
    dir: &Path,
    shell: &str,
) -> Result<Output, Box<dyn Error>> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install/install.sh");
    let mut cmd = Command::new("bash");
    if let Some(path) = path {
        cmd.env("PATH", path);
    }
    Ok(cmd
        .arg(script)
        .env("HOME", root)
        .env("SHELL", shell)
        .env_remove("ZDOTDIR")
        .env_remove("XDG_CONFIG_HOME")
        .env("DOTMAN_INSTALL_DIR", dir)
        .env(
            "DOTMAN_INSTALL_BASE_URL",
            format!("file://{}", release.display()),
        )
        .output()?)
}

#[test]
fn a_matching_checksum_installs_the_binary() -> R {
    let tmp = tempfile::tempdir()?;
    let release = tmp.path().join("release");
    if !fake_release(tmp.path(), &release, None)? {
        return Ok(());
    }
    let out = run_installer(tmp.path(), &release, None)?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(tmp.path().join("bin/dotman").is_file());
    Ok(())
}

#[test]
fn a_checksum_mismatch_installs_nothing() -> R {
    let tmp = tempfile::tempdir()?;
    let release = tmp.path().join("release");
    if !fake_release(tmp.path(), &release, Some(&"0".repeat(64)))? {
        return Ok(());
    }
    let out = run_installer(tmp.path(), &release, None)?;
    assert!(!out.status.success());
    assert!(!tmp.path().join("bin/dotman").exists());
    Ok(())
}

#[test]
fn a_missing_checksum_line_installs_nothing() -> R {
    let tmp = tempfile::tempdir()?;
    let release = tmp.path().join("release");
    if !fake_release(tmp.path(), &release, None)? {
        return Ok(());
    }
    fs::write(release.join("checksums.txt"), "")?;
    let out = run_installer(tmp.path(), &release, None)?;
    assert!(!out.status.success());
    assert!(!tmp.path().join("bin/dotman").exists());
    Ok(())
}

/// A `PATH` of only the tools the installer needs, so no `dotman` can be found.
fn path_without_dot(root: &Path) -> Result<OsString, Box<dyn Error>> {
    let tools: PathBuf = toolbox(&root.join("tools"), INSTALLER_TOOLS)?;
    Ok(tools.into_os_string())
}

#[test]
fn with_no_dot_on_path_dot_becomes_a_symlink_to_dotman() -> R {
    let tmp = tempfile::tempdir()?;
    let release = tmp.path().join("release");
    if !fake_release(tmp.path(), &release, None)? {
        return Ok(());
    }
    let out = run_installer(tmp.path(), &release, Some(path_without_dot(tmp.path())?))?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(tmp.path().join("bin/dotman").is_file());
    assert_eq!(
        fs::read_link(tmp.path().join("bin/dot"))?,
        Path::new("dotman")
    );
    Ok(())
}

#[test]
fn a_dot_on_path_is_never_shadowed() -> R {
    let tmp = tempfile::tempdir()?;
    let release = tmp.path().join("release");
    if !fake_release(tmp.path(), &release, None)? {
        return Ok(());
    }
    let graphviz = tmp.path().join("graphviz");
    fs::create_dir_all(&graphviz)?;
    let stub = graphviz.join("dot");
    fs::write(&stub, "#!/bin/sh\necho 'dot - graphviz version 14.0.0'\n")?;
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o755))?;
    let path = std::env::join_paths([graphviz, path_without_dot(tmp.path())?.into()])?;
    let out = run_installer(tmp.path(), &release, Some(path))?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(tmp.path().join("bin/dotman").is_file());
    assert!(
        fs::symlink_metadata(tmp.path().join("bin/dot")).is_err(),
        "a dot alias was created"
    );
    Ok(())
}

const STARTUP_FILES: [&str; 3] = [".bashrc", ".zshrc", ".config/fish/config.fish"];

fn startup_files(root: &Path) -> Vec<&'static str> {
    STARTUP_FILES
        .into_iter()
        .filter(|file| root.join(file).exists())
        .collect()
}

#[test]
fn the_path_line_goes_to_the_startup_file_of_the_login_shell() -> R {
    for (shell, file, line) in [
        ("/bin/zsh", ".zshrc", "export PATH=\"@DIR@:$PATH\""),
        ("/usr/bin/bash", ".bashrc", "export PATH=\"@DIR@:$PATH\""),
        (
            "/usr/bin/fish",
            ".config/fish/config.fish",
            "set -gx PATH @DIR@ $PATH",
        ),
        ("/bin/tcsh", "", ""),
    ] {
        let tmp = tempfile::tempdir()?;
        let release = tmp.path().join("release");
        if !fake_release(tmp.path(), &release, None)? {
            return Ok(());
        }
        // An entry that only starts with the install dir does not put it on PATH.
        let dir = tmp.path().join("bin");
        let path = std::env::join_paths([
            tmp.path().join("bin2"),
            path_without_dot(tmp.path())?.into(),
        ])?;
        let out = run_installer_in(tmp.path(), &release, Some(path), &dir, shell)?;
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{shell}: {err}");
        if file.is_empty() {
            assert_eq!(startup_files(tmp.path()), Vec::<&str>::new(), "{shell}");
            assert!(
                err.contains("to PATH in your shell's startup file"),
                "{err}"
            );
            continue;
        }
        assert_eq!(startup_files(tmp.path()), [file], "{shell}");
        let text = fs::read_to_string(tmp.path().join(file))?;
        let line = line.replace("@DIR@", &dir.to_string_lossy());
        assert_eq!(text, format!("{line}\n"), "{shell}");
    }
    Ok(())
}

#[test]
fn an_install_dir_that_needs_quoting_is_never_written_into_a_startup_file() -> R {
    let tmp = tempfile::tempdir()?;
    let release = tmp.path().join("release");
    if !fake_release(tmp.path(), &release, None)? {
        return Ok(());
    }
    let marker = tmp.path().join("ran");
    let dir = tmp
        .path()
        .join(format!("bin\"$(touch {})\"", marker.display()));
    let path = path_without_dot(tmp.path())?;
    let out = run_installer_in(tmp.path(), &release, Some(path), &dir, "/bin/bash")?;
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(dir.join("dotman").is_file(), "{err}");
    assert_eq!(startup_files(tmp.path()), Vec::<&str>::new());
    assert!(err.contains("add it to PATH yourself"), "{err}");
    assert!(!marker.exists());
    Ok(())
}
