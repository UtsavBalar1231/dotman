//! Build tasks for dotman.

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, ValueEnum};
use clap_complete::Shell;
use std::fs;
use std::path::{Path, PathBuf};

const BIN: &str = "dotman";

#[derive(Parser)]
#[command(name = "xtask", about = "Build tasks for dotman")]
enum Task {
    /// Write the man pages and shell completions from the clap tree
    Generate {
        /// Directory that holds `man/` and `completions/`
        #[arg(short, long, default_value = ".")]
        root: PathBuf,
    },
}

fn main() -> Result<()> {
    match Task::parse() {
        Task::Generate { root } => generate(&root),
    }
}

/// Replaces the generated files under `root`; `completions/README.md` is kept.
fn generate(root: &Path) -> Result<()> {
    let mut cmd = dotman::cli::Cli::command().disable_help_subcommand(true);
    cmd.build();

    let man_dir = root.join("man");
    let completions_dir = root.join("completions");
    for dir in [&man_dir, &completions_dir] {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    clear(&man_dir, |name| name.ends_with(".1"))?;
    clear(&completions_dir, |name| name != "README.md")?;

    write_page(&man_dir, cmd.clone())?;
    for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
        write_page(&man_dir, sub.clone())?;
    }
    for shell in Shell::value_variants() {
        clap_complete::generate_to(*shell, &mut cmd.clone(), BIN, &completions_dir)?;
    }
    Ok(())
}

/// Trailing whitespace is stripped because the pre-commit hook would do it and the next run would differ.
fn write_page(dir: &Path, cmd: clap::Command) -> Result<()> {
    let man = clap_mangen::Man::new(cmd);
    let mut roff = Vec::new();
    man.render(&mut roff)?;
    let text = String::from_utf8(roff)?;
    let text: String = text
        .lines()
        .map(|l| format!("{}\n", l.trim_end()))
        .collect();
    let path = dir.join(man.get_filename());
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
}

fn clear(dir: &Path, remove: impl Fn(&str) -> bool) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if remove(&entry.file_name().to_string_lossy()) {
            fs::remove_file(entry.path())
                .with_context(|| format!("removing {}", entry.path().display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn names(dir: &Path) -> BTreeSet<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn one_page_per_visible_subcommand_and_stale_files_are_removed() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("man")).unwrap();
        fs::write(root.path().join("man/dot-old.1"), "stale").unwrap();
        fs::create_dir_all(root.path().join("completions")).unwrap();
        fs::write(root.path().join("completions/README.md"), "keep").unwrap();
        fs::write(root.path().join("completions/dot-enhanced.bash"), "stale").unwrap();

        generate(root.path()).unwrap();

        let mut expected: BTreeSet<String> = dotman::cli::Cli::command()
            .get_subcommands()
            .filter(|s| !s.is_hide_set() && s.get_name() != "help")
            .map(|s| format!("{BIN}-{}.1", s.get_name()))
            .collect();
        expected.insert(format!("{BIN}.1"));
        assert_eq!(names(&root.path().join("man")), expected);
        for name in ["dotman-edit.1", "dotman-key.1"] {
            assert!(expected.contains(name), "{name}");
        }
        assert!(!expected.contains("dotman-system-write.1"));

        assert_eq!(
            names(&root.path().join("completions")),
            [
                "README.md",
                "dotman.bash",
                "_dotman",
                "dotman.fish",
                "dotman.elv",
                "_dotman.ps1"
            ]
            .map(String::from)
            .into()
        );
        let bash = fs::read_to_string(root.path().join("completions/dotman.bash")).unwrap();
        assert!(bash.contains("dotman__subcmd__edit") && bash.contains("dotman__subcmd__key"));
    }
}
