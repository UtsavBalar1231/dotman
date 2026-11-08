//! The names in the release workflow, the installers and the package recipe agree, and no source
//! file hard-codes the command a hint tells the user to type.

use std::error::Error;
use std::fs;
use std::path::Path;
use walkdir::WalkDir;

type R = Result<(), Box<dyn Error>>;

fn read(path: &str) -> Result<String, Box<dyn Error>> {
    Ok(fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(path),
    )?)
}

fn has(text: &str, needle: &str, what: &str) {
    assert!(text.contains(needle), "{what} lacks {needle:?}");
}

#[test]
fn no_source_file_hard_codes_a_dot_command() -> R {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = Vec::new();
    for entry in WalkDir::new(&src) {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "rs") || path.ends_with("name.rs") {
            continue;
        }
        for (n, line) in fs::read_to_string(path)?.lines().enumerate() {
            if ["`dot ", "`dot`", "\"dot ", "run dot ", " dot interactively"]
                .iter()
                .any(|bad| line.contains(bad))
            {
                found.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
            }
        }
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
    Ok(())
}

#[test]
fn the_tarball_and_the_binary_inside_it_have_the_same_names_everywhere() -> R {
    let release = read(".github/workflows/release.yml")?;
    let template = read("src/bootstrap/install.sh.in")?;
    let installer = read("install/install.sh")?;

    has(
        &release,
        "dotman-${GITHUB_REF_NAME}-${TARGET}.tar.gz",
        "release.yml",
    );
    has(&release, "-C stage dotman LICENSE README.md", "release.yml");
    has(&release, "target/${TARGET}/release/dotman", "release.yml");
    has(&release, "chmod 0755 stage/dotman", "release.yml");

    has(
        &template,
        "archive=dotman-v$pinned-$arch-unknown-linux-musl.tar.gz",
        "template",
    );
    has(&template, "-C \"$tmp/out\" dotman ", "template");
    has(&template, "\"$tmp/out/dotman\"", "template");

    has(
        &installer,
        "archive=\"dotman-v$version-$target.tar.gz\"",
        "install.sh",
    );
    has(&installer, "BINARY_NAME=\"dotman\"", "install.sh");
    has(
        &installer,
        "linux-*) target=\"${platform#linux-}-unknown-linux-musl\"",
        "install.sh",
    );
    Ok(())
}

#[test]
fn the_package_installs_dotman_and_never_usr_bin_dot() -> R {
    let pkgbuild = read("packaging/arch/PKGBUILD")?;
    has(&pkgbuild, "pkgname=dotman-cli", "PKGBUILD");
    has(&pkgbuild, "\"$pkgdir/usr/bin/dotman\"", "PKGBUILD");
    has(&pkgbuild, "completions/dotman\"", "PKGBUILD");
    has(&pkgbuild, "site-functions/_dotman\"", "PKGBUILD");
    has(&pkgbuild, "vendor_completions.d/dotman.fish\"", "PKGBUILD");
    has(&pkgbuild, "man1/dotman.1\"", "PKGBUILD");
    assert!(
        !pkgbuild.contains("/usr/bin/dot\""),
        "PKGBUILD installs /usr/bin/dot"
    );
    assert!(
        !pkgbuild.contains("release/dot "),
        "PKGBUILD builds from release/dot"
    );

    let manifest = read("Cargo.toml")?;
    has(&manifest, "[[bin]]\nname = \"dotman\"", "Cargo.toml");
    Ok(())
}
