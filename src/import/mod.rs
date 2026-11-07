//! What every importer shares.
//!
//! The report, the fresh-repo and source checks, and the question before writing. An importer
//! reads its source without changing it, and every entry of the source ends up either converted
//! or listed as needing action.

mod legacy;

use crate::cli::ImportArgs;
use crate::commands::Outcome;
use crate::commands::sync::IDENTITY_FIX;
use crate::context::Context;
use crate::engine::write::{back_up_bytes, backup_run_dir};
use crate::output;
use crate::prompt;
use crate::repo_config::RepoConfig;
use anyhow::{Context as _, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// An entry the import could not carry over by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Items with the same reason are printed together.
    pub reason: &'static str,
    /// The entry and what to do about it.
    pub detail: String,
}

/// The outcome of an import. Every source entry is counted once, in `converted` or in
/// `needs_action`, so `total` is the number of entries in the source.
#[derive(Debug, Default)]
pub struct Report {
    /// Entries that are now in the repo as the source has them.
    pub converted: usize,
    /// Entries left for the user.
    pub needs_action: Vec<Item>,
    /// Facts about the run that are not entries.
    pub notes: Vec<String>,
}

impl Report {
    /// The number of entries in the source.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.converted.saturating_add(self.needs_action.len())
    }

    /// Exit 3 when anything needs action, as for every command that reports paths for the user.
    #[must_use]
    pub const fn outcome(&self) -> Outcome {
        if self.needs_action.is_empty() {
            Outcome::Done
        } else {
            Outcome::Attention
        }
    }

    /// Prints the summary line, the entries that need action grouped by reason, then the notes.
    pub fn print(&self) {
        output::info(&format!(
            "{} entries: {} converted, {} need action",
            self.total(),
            self.converted,
            self.needs_action.len()
        ));
        let mut groups: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for item in &self.needs_action {
            groups
                .entry(item.reason)
                .or_default()
                .push(item.detail.as_str());
        }
        for (reason, details) in groups {
            output::warning(&format!("{reason} ({}):", details.len()));
            for detail in details {
                output::warning(&format!("  {detail}"));
            }
        }
        for note in &self.notes {
            output::info(note);
        }
    }
}

/// `1 file`, `2 files`.
pub(crate) fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// The legacy repo `dot import` takes when no source is given.
#[must_use]
pub fn find_legacy(home: &Path) -> Option<PathBuf> {
    let dir = home.join(".dotman");
    legacy::is_legacy(&dir).then_some(dir)
}

/// Runs `dot import`. The caller holds the lock and has run the in-progress guard.
pub fn run(ctx: &Context, args: &ImportArgs) -> Result<Outcome> {
    // Not a clap `requires`: clap skips it whenever the positional source, which conflicts with
    // `--overlay`, is present.
    if args.include_missing && args.overlay.is_none() {
        bail!("--include-missing only applies to --overlay");
    }
    let source = match (&args.overlay, &args.source) {
        (Some(path), _) | (None, Some(path)) => path.clone(),
        (None, None) => find_legacy(&ctx.paths.home).ok_or_else(|| {
            anyhow!(
                "no source given and no legacy dotman repo at {}; run `dot import <source>`",
                ctx.paths.home.join(".dotman").display()
            )
        })?,
    };
    refuse_overlap(&ctx.paths.repo, &source)?;
    if !source.exists() {
        bail!("{} does not exist", source.display());
    }
    if !legacy::is_legacy(&source) {
        bail!(
            "{} is not a source `dot import` understands; a legacy dotman repo holds index.bin or commits/",
            source.display()
        );
    }
    if args.overlay.is_some() {
        legacy::overlay(ctx, &source, args)
    } else {
        legacy::import(ctx, &source, args)
    }
}

/// `DOTMAN_REPO_PATH=~/.dotman` must never make the import read what it writes.
fn refuse_overlap(repo: &Path, source: &Path) -> Result<()> {
    let (repo_real, source_real) = (resolved(repo), resolved(source));
    if source_real.starts_with(&repo_real) || repo_real.starts_with(&source_real) {
        bail!(
            "the source {} is, contains or lies inside the dotman repo {}; point `dot import` at a different directory",
            source.display(),
            repo.display()
        );
    }
    Ok(())
}

/// `canonicalize` of the longest existing prefix of `path` with the rest appended, so a repo
/// that does not exist yet still compares correctly.
fn resolved(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut rest = PathBuf::new();
    let mut base = absolute.as_path();
    loop {
        if let Ok(real) = fs::canonicalize(base) {
            return real.join(rest);
        }
        let (Some(parent), Some(name)) = (base.parent(), base.file_name()) else {
            return absolute.clone();
        };
        rest = Path::new(name).join(&rest);
        base = parent;
    }
}

/// The error names `--overlay`, the way to add to a repo that already has content.
pub(crate) fn require_fresh(ctx: &Context) -> Result<()> {
    let repo = &ctx.paths.repo;
    let committed = repo.join(".git").exists() && ctx.git.head()?.is_some();
    if committed || (repo.is_dir() && !ctx.managed()?.is_empty()) {
        bail!(
            "the repo at {} already has files or history, and `dot import` needs a fresh repo. To add what a source has and the repo lacks, use `dot import --overlay <source>`",
            repo.display()
        );
    }
    Ok(())
}

/// Read from the global config while the repo does not exist yet.
pub(crate) fn author_email(ctx: &Context) -> Result<String> {
    ctx.git.author_email()?.ok_or_else(|| anyhow!(IDENTITY_FIX))
}

/// `$HOME` as given and as resolved: the prefixes stripped from absolute keys.
pub(crate) fn homes(ctx: &Context) -> Vec<&Path> {
    std::iter::once(ctx.paths.home.as_path())
        .chain(ctx.paths.home_real.as_deref())
        .collect()
}

/// A non-interactive run never asks.
pub(crate) fn proceed(args: &ImportArgs, question: &str) -> Result<bool> {
    Ok(args.yes || !prompt::interactive() || prompt::confirm(question)?)
}

/// A `.dotman.toml` in the worktree that is not what `dot init` writes: an import copies it
/// before the imported one replaces it.
pub(crate) struct EditedConfig {
    bytes: Vec<u8>,
    backup_dir: PathBuf,
}

impl EditedConfig {
    /// `None` when the worktree has no `.dotman.toml`, or has exactly what `dot init` writes.
    pub(crate) fn find(ctx: &Context) -> Result<Option<Self>> {
        let bytes = match fs::read(ctx.repo_config_file()) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("cannot read .dotman.toml"),
        };
        if bytes == toml::to_string_pretty(&RepoConfig::default())?.as_bytes() {
            return Ok(None);
        }
        let backup_dir = backup_run_dir(&ctx.paths.state_dir.join("backups"), SystemTime::now())?;
        Ok(Some(Self { bytes, backup_dir }))
    }

    pub(crate) fn note(&self, dry_run: bool) -> String {
        format!(
            "the .dotman.toml in the repo was edited; it {} backed up to {} before the imported one replaces it",
            if dry_run { "would be" } else { "will be" },
            self.backup_dir.join(".dotman.toml").display()
        )
    }

    /// The copy is created at 0600.
    pub(crate) fn back_up(&self) -> Result<()> {
        back_up_bytes(&self.backup_dir, ".dotman.toml", &self.bytes).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_source_that_is_contains_or_lies_inside_the_repo_is_refused() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("data/dotman");
        let inside = repo.join("legacy");
        fs::create_dir_all(&inside).unwrap();
        for source in [repo.clone(), inside, tmp.path().to_path_buf()] {
            let err = refuse_overlap(&repo, &source).unwrap_err().to_string();
            assert!(err.contains("inside the dotman repo"), "{err}");
        }
        let elsewhere = tmp.path().join("legacy");
        fs::create_dir_all(&elsewhere).unwrap();
        refuse_overlap(&repo, &elsewhere).unwrap();
    }

    #[test]
    fn a_repo_that_does_not_exist_yet_still_overlaps_through_a_symlink() {
        let tmp = TempDir::new().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, tmp.path().join("link")).unwrap();
        let repo = real.join("not/yet/created");
        let source = tmp.path().join("link/not");
        assert!(refuse_overlap(&repo, &source).is_err());
    }

    #[test]
    fn counts_are_pluralised() {
        assert_eq!(count(0, "file"), "0 files");
        assert_eq!(count(1, "file"), "1 file");
        assert_eq!(count(2, "commit"), "2 commits");
    }

    #[test]
    fn the_report_counts_every_entry_once() {
        let mut report = Report {
            converted: 2,
            ..Report::default()
        };
        assert_eq!(report.outcome(), Outcome::Done);
        report.needs_action.push(Item {
            reason: "why",
            detail: "x".to_owned(),
        });
        assert_eq!(report.total(), 3);
        assert_eq!(report.outcome(), Outcome::Attention);
    }
}
