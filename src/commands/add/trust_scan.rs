//! Bounded, read-only inventory for local trust reviews.

use super::{Ignore, hold_by_content, hold_by_name_and_size, secret_patterns};
use crate::context::{Context, is_under};
use crate::engine::Tuple;
use crate::paths::{find_collisions, refuse_non_nfc, validate_key};
use crate::repo_config::RepoConfig;
use crate::state::State;
use crate::{sys, variant};
use anyhow::{Result, anyhow, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

const MAX_ENTRIES: usize = 4096;
const MAX_BYTES: u64 = 8 * 1024 * 1024;
const MAX_DEPTH: usize = 32;
const MAX_ROOTS: usize = 256;
const MAX_KEY: usize = 4096;
const REFUSAL: &str = "trust review contains an unsupported, unsafe, changed or oversized tree";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Eligibility {
    Known,
    Ignored,
    UnlistedIgnore,
    Eligible,
    Unapproved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Leaf {
    pub(crate) eligibility: Eligibility,
    pub(crate) hash: String,
    pub(crate) mode: u32,
    pub(crate) size: u64,
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Directory {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
    pub(crate) mode: u32,
    pub(crate) mtime: i64,
    pub(crate) mtime_nsec: i64,
    pub(crate) ctime: i64,
    pub(crate) ctime_nsec: i64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct TrustScan {
    pub(crate) leaves: BTreeMap<String, Leaf>,
    pub(crate) directories: BTreeMap<String, Directory>,
}

/// Inventories explicit home-relative roots under fixed resource limits.
/// `known` must be the complete, successfully read repository and state key set.
///
/// Errors deliberately contain no repository or filesystem names.
pub fn scan(
    ctx: &Context,
    config: &RepoConfig,
    known: &BTreeSet<String>,
    state: &State,
    roots: &[String],
) -> Result<TrustScan> {
    scan_inner(ctx, config, known, state, roots).map_err(|_| anyhow!(REFUSAL))
}

struct Scanner<'a> {
    ctx: &'a Context,
    known: &'a BTreeSet<String>,
    approved: BTreeSet<String>,
    listed: Ignore,
    unlisted: Ignore,
    secrets: Vec<glob::Pattern>,
    count: usize,
    bytes: u64,
    result: TrustScan,
}

fn scan_inner(
    ctx: &Context,
    config: &RepoConfig,
    known: &BTreeSet<String>,
    state: &State,
    roots: &[String],
) -> Result<TrustScan> {
    ensure!(
        roots.len() <= MAX_ROOTS
            && known.len() <= MAX_ENTRIES
            && known.iter().all(|key| key.len() <= MAX_KEY)
            && config.ignore.patterns.len() <= MAX_ENTRIES
            && config.guard.secret_names.len() <= MAX_ENTRIES
            && config.dirs.paths.len() <= MAX_ROOTS
            && state.ignore_seen.len() <= MAX_ENTRIES
            && state
                .capture_dirs
                .as_ref()
                .is_none_or(|dirs| dirs.len() <= MAX_ENTRIES)
            && config
                .ignore
                .patterns
                .iter()
                .chain(&config.guard.secret_names)
                .chain(&config.dirs.paths)
                .chain(state.ignore_seen.iter())
                .chain(state.capture_dirs.iter().flatten())
                .all(|value| value.len() <= MAX_KEY),
        "limit"
    );
    let home = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&ctx.paths.home)?;
    let mut scanner = Scanner {
        ctx,
        known,
        approved: state
            .dirs_approved()
            .into_iter()
            .filter(|dir| config.dirs.paths.contains(dir))
            .collect(),
        listed: Ignore::compile(&config.ignore.patterns)?,
        unlisted: Ignore::compile(&state.unlisted_ignores(&config.ignore.patterns))?,
        secrets: secret_patterns(config)?,
        count: 0,
        bytes: 0,
        result: TrustScan::default(),
    };
    let mut ordered = BTreeSet::new();
    for root in roots {
        scanner.validate_name(root, true)?;
        ensure!(root.split('/').count() <= MAX_DEPTH, "depth");
        ensure!(ordered.insert(root.as_str()), "duplicate");
    }
    for root in &ordered {
        if ordered
            .iter()
            .any(|other| other != root && is_under(root, other))
        {
            continue;
        }
        let dir = open_root(&home, root)?;
        let before = dir.metadata()?;
        scanner.walk(&dir, root, 0, false, false)?;
        ensure!(
            same(&before, &open_root(&home, root)?.metadata()?),
            "changed"
        );
    }
    let collisions = find_collisions(
        known
            .iter()
            .map(String::as_str)
            .chain(scanner.result.leaves.keys().map(String::as_str))
            .chain(scanner.result.directories.keys().map(String::as_str)),
    );
    ensure!(
        collisions.is_empty()
            && scanner
                .result
                .directories
                .keys()
                .all(|key| !known.contains(key)),
        "collision"
    );
    Ok(scanner.result)
}

fn open_root(home: &File, root: &str) -> Result<File> {
    let mut dir = home.try_clone()?;
    for component in root.split('/') {
        dir = File::from(sys::openat_path(dir.as_fd(), &CString::new(component)?)?);
        ensure!(dir.metadata()?.is_dir(), "directory");
    }
    Ok(dir)
}

impl Scanner<'_> {
    fn validate_name(&self, key: &str, directory: bool) -> Result<()> {
        ensure!(!key.is_empty() && key.len() <= MAX_KEY, "name");
        validate_key(key)?;
        refuse_non_nfc(Path::new(key))?;
        self.ctx.check_key(key, !directory)?;
        ensure!(
            key.split('/').all(|part| {
                !part.starts_with(".dotman-tmp-")
                    && !part.eq_ignore_ascii_case(".git")
                    && !Path::new(part)
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("tmpl"))
                    && variant::is_literal(part)
            }),
            "unsupported name"
        );
        ensure!(
            hold_by_name_and_size(&self.secrets, key, 0).is_none(),
            "held"
        );
        Ok(())
    }

    fn walk(
        &mut self,
        dir: &File,
        key: &str,
        depth: usize,
        parent_ignored: bool,
        parent_unlisted: bool,
    ) -> Result<()> {
        ensure!(depth <= MAX_DEPTH, "depth");
        self.count = self.count.checked_add(1).ok_or_else(|| anyhow!("count"))?;
        ensure!(self.count <= MAX_ENTRIES, "count");
        let before = dir.metadata()?;
        ensure!(before.is_dir(), "directory");
        let ignored = parent_ignored || self.listed.matches(key, true);
        let unlisted = parent_unlisted || self.unlisted.matches(key, true);
        ensure!(
            self.result
                .directories
                .insert(key.to_owned(), directory(&before))
                .is_none(),
            "duplicate"
        );
        for entry in fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| anyhow!("name"))?;
            let child = format!("{key}/{name}");
            ensure!(child.len() <= MAX_KEY, "name");
            let component = CString::new(name)?;
            let pinned = File::from(sys::openat_path(dir.as_fd(), &component)?);
            let metadata = pinned.metadata()?;
            self.validate_name(&child, metadata.is_dir())?;
            if metadata.is_dir() {
                self.walk(&pinned, &child, depth.saturating_add(1), ignored, unlisted)?;
            } else {
                self.file(dir, &pinned, &component, &child, ignored, unlisted)?;
            }
            let current = File::from(sys::openat_path(dir.as_fd(), &component)?).metadata()?;
            ensure!(same(&metadata, &current), "changed");
        }
        ensure!(same(&before, &dir.metadata()?), "changed");
        Ok(())
    }

    fn file(
        &mut self,
        dir: &File,
        pinned: &File,
        name: &CString,
        key: &str,
        parent_ignored: bool,
        parent_unlisted: bool,
    ) -> Result<()> {
        self.count = self.count.checked_add(1).ok_or_else(|| anyhow!("count"))?;
        ensure!(self.count <= MAX_ENTRIES, "count");
        let before = pinned.metadata()?;
        ensure!(before.is_file() && before.nlink() == 1, "kind");
        ensure!(
            hold_by_name_and_size(&self.secrets, key, before.len()).is_none(),
            "held"
        );
        self.bytes = self
            .bytes
            .checked_add(before.len())
            .ok_or_else(|| anyhow!("size"))?;
        ensure!(self.bytes <= MAX_BYTES, "size");
        let mut file = File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))?;
        let mut bytes = Vec::new();
        file.by_ref()
            .take(before.len().saturating_add(1))
            .read_to_end(&mut bytes)?;
        ensure!(u64::try_from(bytes.len())? == before.len(), "changed");
        ensure!(same(&before, &file.metadata()?), "changed");
        let current = File::from(sys::openat_path(dir.as_fd(), name)?).metadata()?;
        ensure!(same(&before, &current), "changed");
        ensure!(hold_by_content(&bytes, key, &[]).is_none(), "held");
        let eligibility = if parent_ignored || self.listed.matches(key, false) {
            Eligibility::Ignored
        } else if parent_unlisted || self.unlisted.matches(key, false) {
            Eligibility::UnlistedIgnore
        } else if self.known.contains(key) {
            Eligibility::Known
        } else if self.approved.iter().any(|root| is_under(key, root)) {
            Eligibility::Eligible
        } else {
            Eligibility::Unapproved
        };
        let tuple = Tuple::of_file(&bytes, before.mode());
        ensure!(
            self.result
                .leaves
                .insert(
                    key.to_owned(),
                    Leaf {
                        eligibility,
                        hash: tuple.hash,
                        mode: tuple.mode,
                        size: before.len(),
                        dev: before.dev(),
                        ino: before.ino(),
                    }
                )
                .is_none(),
            "duplicate"
        );
        Ok(())
    }
}

fn directory(meta: &Metadata) -> Directory {
    Directory {
        dev: meta.dev(),
        ino: meta.ino(),
        mode: meta.mode(),
        mtime: meta.mtime(),
        mtime_nsec: meta.mtime_nsec(),
        ctime: meta.ctime(),
        ctime_nsec: meta.ctime_nsec(),
    }
}

fn same(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.mode() == b.mode()
        && a.len() == b.len()
        && a.nlink() == b.nlink()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::paths::{Env, Paths};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

    struct Fixture {
        _root: TempDir,
        ctx: Context,
        config: RepoConfig,
        state: State,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let root = tempfile::tempdir()?;
            let home = root.path().join("home");
            let repo = root.path().join("repo");
            fs::create_dir_all(home.join("configs"))?;
            fs::create_dir_all(&repo)?;
            let env = Env::from_fn(|name| match name {
                "HOME" => Some(home.clone().into_os_string()),
                "DOTMAN_REPO_PATH" => Some(repo.clone().into_os_string()),
                _ => None,
            });
            let ctx = Context::new(
                Paths::resolve(&env, sys::geteuid())?,
                Config::default(),
                "test",
            );
            Ok(Self {
                _root: root,
                ctx,
                config: RepoConfig::default(),
                state: State::default(),
            })
        }

        fn run(&self) -> Result<TrustScan> {
            scan(
                &self.ctx,
                &self.config,
                &BTreeSet::new(),
                &self.state,
                &["configs".into()],
            )
        }

        fn home(&self) -> &Path {
            &self.ctx.paths.home
        }
    }

    #[test]
    fn bounded_inventory_records_empty_directories_and_exact_tuples() -> Result<()> {
        let mut f = Fixture::new()?;
        fs::create_dir_all(f.home().join("configs/empty/nested"))?;
        let file = f.home().join("configs/settings");
        fs::write(&file, b"ordinary")?;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
        f.config.dirs.paths.push("configs".to_owned());
        f.state.approve_dirs([&"configs".to_owned()]);
        let first = f.run()?;
        assert_eq!(
            first.leaves.get("configs/settings").map(|x| x.eligibility),
            Some(Eligibility::Eligible)
        );
        assert!(first.directories.contains_key("configs/empty/nested"));
        assert_eq!(f.run()?, first);
        fs::write(&file, b"changed")?;
        let changed = f.run()?;
        assert_ne!(
            first.leaves.get("configs/settings"),
            changed.leaves.get("configs/settings")
        );
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640))?;
        assert_ne!(
            changed.leaves.get("configs/settings"),
            f.run()?.leaves.get("configs/settings")
        );
        Ok(())
    }

    #[test]
    fn ordinary_candidates_match_existing_directory_scan() -> Result<()> {
        let mut f = Fixture::new()?;
        f.config.dirs.paths.push("configs".into());
        f.state.approve_dirs([&"configs".to_owned()]);
        fs::write(f.home().join("configs/first"), b"first")?;
        fs::create_dir(f.home().join("configs/nested"))?;
        fs::write(f.home().join("configs/nested/second"), b"second")?;
        let keys = BTreeSet::new();
        let old = super::super::scan_dirs(&f.ctx, &f.config, &keys, &f.state)?;
        let new = f.run()?;
        let eligible: BTreeSet<_> = new
            .leaves
            .iter()
            .filter(|(_, leaf)| leaf.eligibility == Eligibility::Eligible)
            .map(|(key, _)| key.as_str())
            .collect();
        assert_eq!(eligible, old.new.keys().map(String::as_str).collect());
        Ok(())
    }

    #[test]
    fn refuses_file_count_and_counts_ignored_entries() -> Result<()> {
        let mut f = Fixture::new()?;
        fs::create_dir_all(f.home().join("configs/ignored"))?;
        f.config.ignore.patterns.push("configs/ignored/".into());
        for i in 0..MAX_ENTRIES.saturating_sub(2) {
            fs::write(f.home().join(format!("configs/ignored/f{i}")), b"x")?;
        }
        assert!(f.run().is_ok());
        fs::write(f.home().join("configs/ignored/excess"), b"x")?;
        assert!(f.run().is_err());
        Ok(())
    }

    #[test]
    fn refuses_total_bytes_and_excess_depth() -> Result<()> {
        let f = Fixture::new()?;
        let content = vec![b'x'; 1_048_576];
        for i in 0..8 {
            fs::write(f.home().join(format!("configs/file{i}")), &content)?;
        }
        assert!(f.run().is_ok());
        fs::write(f.home().join("configs/file8"), &content)?;
        assert!(f.run().is_err());
        for i in 0..9 {
            fs::remove_file(f.home().join(format!("configs/file{i}")))?;
        }
        let mut path = f.home().join("configs");
        for _ in 0..MAX_DEPTH {
            path.push("d");
            fs::create_dir(&path)?;
        }
        assert!(f.run().is_ok());
        path.push("d");
        fs::create_dir(&path)?;
        assert!(f.run().is_err());
        Ok(())
    }

    #[test]
    fn ignored_known_and_unapproved_are_classified_and_unsafe_trees_refused() -> Result<()> {
        let mut f = Fixture::new()?;
        fs::write(f.home().join("configs/known"), b"a")?;
        fs::write(f.home().join("configs/ignored"), b"b")?;
        fs::write(f.home().join("configs/new"), b"c")?;
        fs::write(f.home().join("configs/old"), b"d")?;
        f.config.ignore.patterns.push("configs/ignored".into());
        f.state.ignore_seen.insert("configs/old".into());
        let known = BTreeSet::from(["configs/known".to_owned()]);
        let report = scan(&f.ctx, &f.config, &known, &f.state, &["configs".into()])?;
        assert_eq!(
            report.leaves.get("configs/known").map(|v| v.eligibility),
            Some(Eligibility::Known)
        );
        assert_eq!(
            report.leaves.get("configs/ignored").map(|v| v.eligibility),
            Some(Eligibility::Ignored)
        );
        assert_eq!(
            report.leaves.get("configs/new").map(|v| v.eligibility),
            Some(Eligibility::Unapproved)
        );
        assert_eq!(
            report.leaves.get("configs/old").map(|v| v.eligibility),
            Some(Eligibility::UnlistedIgnore)
        );
        let link = f.home().join("configs/link");
        symlink("missing", &link)?;
        assert!(f.run().is_err());
        fs::remove_file(link)?;
        fs::write(f.home().join("configs/example.tmpl"), b"content")?;
        assert!(f.run().is_err());
        fs::remove_file(f.home().join("configs/example.tmpl"))?;
        let sensitive = concat!("se", "cret-token");
        fs::write(f.home().join(format!("configs/{sensitive}")), b"x")?;
        let message = f.run().expect_err("sensitive tree must fail").to_string();
        assert!(!message.contains(sensitive));
        Ok(())
    }
}
