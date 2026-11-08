//! `install/install.sh` against a `file://` fake release.

use std::error::Error;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

type R = Result<(), Box<dyn Error>>;

/// Writes a release tarball holding a fake `dot` plus `checksums.txt` into `release`, with the
/// tarball's checksum line replaced by `checksum_line` when given.
fn fake_release(root: &Path, release: &Path, checksum_line: Option<&str>) -> R {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", arch) => format!("{arch}-unknown-linux-musl"),
        ("macos", arch) => format!("{arch}-apple-darwin"),
        (os, _) => return Err(format!("unsupported test platform {os}").into()),
    };
    let archive = format!("dotman-v{}-{target}.tar.gz", dotman::VERSION);
    let stage = root.join("stage");
    fs::create_dir_all(&stage)?;
    let dot = stage.join("dot");
    fs::write(&dot, "#!/bin/sh\necho 'dot 0.0.0-fake'\n")?;
    fs::set_permissions(&dot, fs::Permissions::from_mode(0o755))?;
    fs::create_dir_all(release)?;
    let status = Command::new("tar")
        .args(["-czf"])
        .arg(release.join(&archive))
        .arg("-C")
        .arg(&stage)
        .arg("dot")
        .status()?;
    assert!(status.success());
    let sums = Command::new("sha256sum")
        .arg(&archive)
        .current_dir(release)
        .output()?;
    let line = match checksum_line {
        Some(l) => format!("{l}  {archive}\n"),
        None => String::from_utf8(sums.stdout)?,
    };
    fs::write(release.join("checksums.txt"), line)?;
    Ok(())
}

/// Runs the installer against `release`, installing into `root/bin`.
fn run_installer(root: &Path, release: &Path) -> Result<Output, Box<dyn Error>> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install/install.sh");
    Ok(Command::new("bash")
        .arg(script)
        .env("HOME", root)
        .env("DOTMAN_INSTALL_DIR", root.join("bin"))
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
    fake_release(tmp.path(), &release, None)?;
    let out = run_installer(tmp.path(), &release)?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(tmp.path().join("bin/dot").is_file());
    Ok(())
}

#[test]
fn a_checksum_mismatch_installs_nothing() -> R {
    let tmp = tempfile::tempdir()?;
    let release = tmp.path().join("release");
    fake_release(tmp.path(), &release, Some(&"0".repeat(64)))?;
    let out = run_installer(tmp.path(), &release)?;
    assert!(!out.status.success());
    assert!(!tmp.path().join("bin/dot").exists());
    Ok(())
}

#[test]
fn a_missing_checksum_line_installs_nothing() -> R {
    let tmp = tempfile::tempdir()?;
    let release = tmp.path().join("release");
    fake_release(tmp.path(), &release, None)?;
    fs::write(release.join("checksums.txt"), "")?;
    let out = run_installer(tmp.path(), &release)?;
    assert!(!out.status.success());
    assert!(!tmp.path().join("bin/dot").exists());
    Ok(())
}
