//! The three-way status model: each key's R, H and S tuples decide its row and prompt category.

use super::{Home, Node, Tuple, read_node, recorded_mode, safe_mode};
use crate::state::{IdentitySnapshot, Kind};
use crate::variant::{Selection, Sources};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A row of the plan's status table, or one of the cases that override rows 8 to 11.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    /// Row 1: R and H are missing. S is dropped silently.
    GoneEverywhere,
    /// Row 2: forgotten upstream and unchanged here. S is dropped and the file is left in place.
    Forgotten,
    /// Row 3: forgotten upstream but edited here. S is kept until `add` or `forget` decides.
    ForgottenEdited,
    /// Row 4: H equals R.
    Clean {
        /// S differs from R and is to be set to R.
        update_state: bool,
    },
    /// Row 5: R is to be written.
    NotApplied,
    /// Row 6: deleted here, unchanged upstream.
    DeletedLocally,
    /// Row 7: deleted here and changed upstream.
    DeletedChangedUpstream,
    /// Row 8: H exists but S does not, as on a fresh machine or after state loss.
    Existing,
    /// Row 9: H is to be captured into the source.
    ChangedLocally,
    /// Row 10: R is to be written over H, which still equals S.
    ChangedInRepo,
    /// Row 11: H, R and S all differ.
    BothChanged,
    /// R is a file or symlink and something dotman never removes is in the way.
    Blocked,
    /// R is a regular file and H is a symlink, which is never written through.
    SymlinkAtTarget,
}

/// Groups of paths that share one prompt or report line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Report-only; exit 3.
    ForgottenEdited,
    /// Keep local, take repo, or show diff.
    Conflicts,
    /// Keep local or take repo, after backing up H.
    ExistingFiles,
    /// Forget or restore.
    LocalDeletions,
    /// Report-only; exit 3.
    Blocked,
}

impl Row {
    /// The row number in the plan's table; `None` for the overriding cases.
    #[must_use]
    pub const fn number(self) -> Option<u8> {
        Some(match self {
            Self::GoneEverywhere => 1,
            Self::Forgotten => 2,
            Self::ForgottenEdited => 3,
            Self::Clean { .. } => 4,
            Self::NotApplied => 5,
            Self::DeletedLocally => 6,
            Self::DeletedChangedUpstream => 7,
            Self::Existing => 8,
            Self::ChangedLocally => 9,
            Self::ChangedInRepo => 10,
            Self::BothChanged => 11,
            Self::Blocked | Self::SymlinkAtTarget => return None,
        })
    }

    /// `None` for rows that need no prompt or report.
    #[must_use]
    pub const fn category(self) -> Option<Category> {
        match self {
            Self::ForgottenEdited => Some(Category::ForgottenEdited),
            Self::DeletedLocally => Some(Category::LocalDeletions),
            Self::DeletedChangedUpstream | Self::BothChanged | Self::SymlinkAtTarget => {
                Some(Category::Conflicts)
            }
            Self::Existing => Some(Category::ExistingFiles),
            Self::Blocked => Some(Category::Blocked),
            Self::GoneEverywhere
            | Self::Forgotten
            | Self::Clean { .. }
            | Self::NotApplied
            | Self::ChangedLocally
            | Self::ChangedInRepo => None,
        }
    }

    /// The row when S was applied from another source than R's. H was edited against the old
    /// source, so it is never captured into the new one, and S takes the new source even when the
    /// tuples agree.
    #[must_use]
    pub const fn switched(self) -> Self {
        match self {
            Self::ChangedLocally => Self::BothChanged,
            Self::Clean { .. } => Self::Clean { update_state: true },
            other => other,
        }
    }
}

/// Classifies one key. `None` when neither R nor S exists, so the key is not managed.
#[must_use]
pub fn classify(r: Option<&Tuple>, h: &Home, s: Option<&Tuple>) -> Option<Row> {
    let Some(r) = r else {
        return s.map(|s| match h {
            Home::Missing => Row::GoneEverywhere,
            Home::Present(h) if h == s => Row::Forgotten,
            Home::Present(_) | Home::Blocked(_) => Row::ForgottenEdited,
        });
    };
    let h = match h {
        Home::Present(h) if h == r => {
            return Some(Row::Clean {
                update_state: s != Some(r),
            });
        }
        Home::Blocked(_) => return Some(Row::Blocked),
        Home::Missing => {
            return Some(match s {
                None => Row::NotApplied,
                Some(s) if s == r => Row::DeletedLocally,
                Some(_) => Row::DeletedChangedUpstream,
            });
        }
        Home::Present(h) => h,
    };
    if h.kind == Kind::Symlink && r.kind == Kind::File {
        return Some(Row::SymlinkAtTarget);
    }
    Some(match s {
        None => Row::Existing,
        Some(s) if s == r => Row::ChangedLocally,
        Some(s) if s == h => Row::ChangedInRepo,
        Some(_) => Row::BothChanged,
    })
}

/// What R holds for one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    /// Repo-relative path that produced R; recorded as S's `source`.
    pub source: String,
    /// R's tuple, with the mode dotman writes.
    pub tuple: Tuple,
    /// Bytes or link target to write.
    pub content: Content,
}

/// The content of R.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    /// A regular file's bytes.
    Bytes(Vec<u8>),
    /// A symlink's target, verbatim.
    Link(PathBuf),
}

/// Produces R for a key.
pub trait Source {
    /// The winning entry, whose `source` is the repo path it came from.
    fn get(&self, key: &str) -> Result<Selection<SourceEntry>>;
}

/// Single repo paths, as-is: the R of a path is the file or symlink there.
#[derive(Debug, Clone, Copy)]
pub struct Worktree<'a> {
    root: &'a Path,
    modes: &'a BTreeMap<String, String>,
}

impl<'a> Worktree<'a> {
    /// `modes` is `.dotman.toml` `[modes]`.
    #[must_use]
    pub const fn new(root: &'a Path, modes: &'a BTreeMap<String, String>) -> Self {
        Self { root, modes }
    }

    /// `None` when nothing is at `key`. A path under a symlinked directory is not an entry, so
    /// it reads as missing.
    pub fn get(&self, key: &str) -> Result<Option<SourceEntry>> {
        let (tuple, content) = match read_node(self.root, key)? {
            Node::File {
                bytes,
                mode: on_disk,
                ..
            } => {
                // git keeps only the owner execute bit, so any other mode must come from `[modes]`.
                let default = if on_disk & 0o100 == 0 { 0o644 } else { 0o755 };
                let mode = safe_mode(recorded_mode(self.modes, key)?.unwrap_or(default));
                (Tuple::of_file(&bytes, mode), Content::Bytes(bytes))
            }
            Node::Symlink(target) => (Tuple::of_link(&target), Content::Link(target)),
            Node::Missing | Node::BadParent(_) | Node::Dir | Node::Special => return Ok(None),
        };
        Ok(Some(SourceEntry {
            source: key.to_owned(),
            tuple,
            content,
        }))
    }
}

/// The worktree with variants resolved for one machine identity.
#[derive(Debug, Clone, Copy)]
pub struct Variants<'a> {
    worktree: Worktree<'a>,
    sources: &'a Sources,
    identity: &'a IdentitySnapshot,
}

impl<'a> Variants<'a> {
    /// `sources` groups the worktree's paths; `identity` picks among them.
    #[must_use]
    pub const fn new(
        worktree: Worktree<'a>,
        sources: &'a Sources,
        identity: &'a IdentitySnapshot,
    ) -> Self {
        Self {
            worktree,
            sources,
            identity,
        }
    }

    /// The source `identity` would pick, such as the recorded snapshot rather than this run's.
    #[must_use]
    pub fn winner_under(&self, key: &str, identity: &IdentitySnapshot) -> Option<&'a str> {
        self.sources.select(key, identity).winner().copied()
    }
}

impl Source for Variants<'_> {
    fn get(&self, key: &str) -> Result<Selection<SourceEntry>> {
        Ok(match self.sources.select(key, self.identity) {
            Selection::None => Selection::None,
            Selection::Winner(source) => self
                .worktree
                .get(source)?
                .map_or(Selection::None, Selection::Winner),
            Selection::Ambiguous(sources) => Selection::Ambiguous(sources),
            Selection::Invalid(problems) => Selection::Invalid(problems),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Obstacle, observe};
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

    fn file(text: &str) -> Tuple {
        Tuple::of_file(text.as_bytes(), 0o644)
    }

    fn present(tuple: &Tuple) -> Home {
        Home::Present(tuple.clone())
    }

    #[test]
    fn row_1_gone_everywhere() {
        let row = classify(None, &Home::Missing, Some(&file("s")));
        assert_eq!(row, Some(Row::GoneEverywhere));
        assert_eq!(row.and_then(Row::number), Some(1));
        assert_eq!(row.and_then(Row::category), None);
    }

    #[test]
    fn row_2_forgotten_upstream() {
        let s = file("s");
        let row = classify(None, &present(&s), Some(&s));
        assert_eq!(row, Some(Row::Forgotten));
        assert_eq!(row.and_then(Row::number), Some(2));
        assert_eq!(row.and_then(Row::category), None);
    }

    #[test]
    fn row_3_forgotten_upstream_with_local_edits() {
        let row = classify(None, &present(&file("h")), Some(&file("s")));
        assert_eq!(row, Some(Row::ForgottenEdited));
        assert_eq!(row.and_then(Row::number), Some(3));
        assert_eq!(row.and_then(Row::category), Some(Category::ForgottenEdited));
        let blocked = Home::Blocked(Obstacle::Directory);
        assert_eq!(
            classify(None, &blocked, Some(&file("s"))),
            Some(Row::ForgottenEdited)
        );
    }

    #[test]
    fn row_4_clean() {
        let r = file("r");
        let row = classify(Some(&r), &present(&r), Some(&r));
        assert_eq!(
            row,
            Some(Row::Clean {
                update_state: false
            })
        );
        assert_eq!(row.and_then(Row::number), Some(4));
        assert_eq!(row.and_then(Row::category), None);
        for stale in [None, Some(file("old"))] {
            assert_eq!(
                classify(Some(&r), &present(&r), stale.as_ref()),
                Some(Row::Clean { update_state: true })
            );
        }
    }

    #[test]
    fn row_5_not_applied_yet() {
        let row = classify(Some(&file("r")), &Home::Missing, None);
        assert_eq!(row, Some(Row::NotApplied));
        assert_eq!(row.and_then(Row::number), Some(5));
        assert_eq!(row.and_then(Row::category), None);
    }

    #[test]
    fn row_6_deleted_locally() {
        let r = file("r");
        let row = classify(Some(&r), &Home::Missing, Some(&r));
        assert_eq!(row, Some(Row::DeletedLocally));
        assert_eq!(row.and_then(Row::number), Some(6));
        assert_eq!(row.and_then(Row::category), Some(Category::LocalDeletions));
    }

    #[test]
    fn row_7_deleted_locally_changed_upstream() {
        let row = classify(Some(&file("r")), &Home::Missing, Some(&file("s")));
        assert_eq!(row, Some(Row::DeletedChangedUpstream));
        assert_eq!(row.and_then(Row::number), Some(7));
        assert_eq!(row.and_then(Row::category), Some(Category::Conflicts));
    }

    #[test]
    fn row_8_existing_file() {
        let row = classify(Some(&file("r")), &present(&file("h")), None);
        assert_eq!(row, Some(Row::Existing));
        assert_eq!(row.and_then(Row::number), Some(8));
        assert_eq!(row.and_then(Row::category), Some(Category::ExistingFiles));
    }

    #[test]
    fn row_9_changed_locally() {
        let r = file("r");
        let row = classify(Some(&r), &present(&file("h")), Some(&r));
        assert_eq!(row, Some(Row::ChangedLocally));
        assert_eq!(row.and_then(Row::number), Some(9));
        assert_eq!(row.and_then(Row::category), None);
    }

    #[test]
    fn row_10_changed_in_repo() {
        let h = file("h");
        let row = classify(Some(&file("r")), &present(&h), Some(&h));
        assert_eq!(row, Some(Row::ChangedInRepo));
        assert_eq!(row.and_then(Row::number), Some(10));
        assert_eq!(row.and_then(Row::category), None);
    }

    #[test]
    fn row_11_both_changed() {
        let row = classify(Some(&file("r")), &present(&file("h")), Some(&file("s")));
        assert_eq!(row, Some(Row::BothChanged));
        assert_eq!(row.and_then(Row::number), Some(11));
        assert_eq!(row.and_then(Row::category), Some(Category::Conflicts));
    }

    #[test]
    fn unmanaged_keys_have_no_row() {
        assert_eq!(classify(None, &present(&file("h")), None), None);
        assert_eq!(classify(None, &Home::Missing, None), None);
    }

    #[test]
    fn mode_and_kind_count_as_content() {
        let r = file("same");
        let h = Tuple::of_file(b"same", 0o600);
        assert_eq!(
            classify(Some(&r), &present(&h), Some(&r)),
            Some(Row::ChangedLocally)
        );
        let link = Tuple::of_link(Path::new("same"));
        assert_eq!(link.hash, r.hash);
        assert_eq!(
            classify(Some(&link), &present(&r), Some(&r)),
            Some(Row::ChangedInRepo)
        );
    }

    #[test]
    fn a_directory_in_the_way_is_blocked() {
        let tmp = TempDir::new().unwrap();
        let (repo, home) = (tmp.path().join("repo"), tmp.path().join("home"));
        fs::create_dir_all(repo.join(".config")).unwrap();
        fs::create_dir_all(home.join(".config/app.conf")).unwrap();
        fs::write(repo.join(".config/app.conf"), b"x").unwrap();
        symlink("target", repo.join(".link")).unwrap();
        fs::create_dir(home.join(".link")).unwrap();

        let modes = BTreeMap::new();
        let source = Worktree::new(&repo, &modes);
        for key in [".config/app.conf", ".link"] {
            let r = source.get(key).unwrap().unwrap().tuple;
            let h = observe(&home, key).unwrap();
            assert_eq!(h, Home::Blocked(Obstacle::Directory));
            for s in [None, Some(&r)] {
                let row = classify(Some(&r), &h, s);
                assert_eq!(row, Some(Row::Blocked), "{key}");
                assert_eq!(row.and_then(Row::category), Some(Category::Blocked));
                assert_eq!(row.and_then(Row::number), None);
            }
        }
    }

    #[test]
    fn a_symlinked_parent_in_home_is_blocked() {
        let tmp = TempDir::new().unwrap();
        let (home, outside) = (tmp.path().join("home"), tmp.path().join("outside"));
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("app.conf"), b"x").unwrap();
        symlink(&outside, home.join(".config")).unwrap();
        let r = file("x");
        let h = observe(&home, ".config/app.conf").unwrap();
        assert_eq!(h, Home::Blocked(Obstacle::Parent(".config".to_owned())));
        assert_eq!(classify(Some(&r), &h, Some(&r)), Some(Row::Blocked));
    }

    #[test]
    fn a_symlink_where_r_is_a_file_is_a_conflict() {
        let tmp = TempDir::new().unwrap();
        let (repo, home) = (tmp.path().join("repo"), tmp.path().join("home"));
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(repo.join(".bashrc"), b"repo").unwrap();
        fs::write(tmp.path().join("elsewhere"), b"repo").unwrap();
        symlink(tmp.path().join("elsewhere"), home.join(".bashrc")).unwrap();

        let modes = BTreeMap::new();
        let r = Worktree::new(&repo, &modes)
            .get(".bashrc")
            .unwrap()
            .unwrap()
            .tuple;
        let h = observe(&home, ".bashrc").unwrap();
        let link = Tuple::of_link(&tmp.path().join("elsewhere"));
        assert_eq!(h, present(&link));
        for s in [None, Some(&r), Some(&link), Some(&file("other"))] {
            let row = classify(Some(&r), &h, s);
            assert_eq!(row, Some(Row::SymlinkAtTarget));
            assert_eq!(row.and_then(Row::category), Some(Category::Conflicts));
        }
    }

    #[test]
    fn a_file_where_r_is_a_symlink_follows_the_rows() {
        let link = Tuple::of_link(Path::new("target"));
        let h = file("h");
        assert_eq!(
            classify(Some(&link), &present(&h), None),
            Some(Row::Existing)
        );
        assert_eq!(
            classify(Some(&link), &present(&h), Some(&h)),
            Some(Row::ChangedInRepo)
        );
    }

    #[test]
    fn worktree_reads_without_following_and_applies_modes() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(repo.join("bin")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("x"), b"outside").unwrap();
        symlink(&outside, repo.join(".config")).unwrap();
        symlink("../outside/x", repo.join(".link")).unwrap();
        fs::write(repo.join("bin/run"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(repo.join("bin/run"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(repo.join(".plain"), b"p").unwrap();
        fs::write(repo.join(".suid"), b"s").unwrap();
        fs::write(repo.join(".private"), b"k").unwrap();

        let modes = BTreeMap::from([
            (".suid".to_owned(), "4755".to_owned()),
            (".private".to_owned(), "0600".to_owned()),
        ]);
        let source = Worktree::new(&repo, &modes);
        let mode = |key: &str| source.get(key).unwrap().unwrap().tuple.mode;
        assert_eq!(mode("bin/run"), 0o755);
        assert_eq!(mode(".plain"), 0o644);
        assert_eq!(mode(".suid"), 0o755);
        assert_eq!(mode(".private"), 0o600);

        let link = source.get(".link").unwrap().unwrap();
        assert_eq!(link.content, Content::Link(PathBuf::from("../outside/x")));
        assert_eq!(link.tuple, Tuple::of_link(Path::new("../outside/x")));
        assert_eq!(link.source, ".link");
        assert_eq!(
            source.get(".plain").unwrap().unwrap().content,
            Content::Bytes(b"p".to_vec())
        );

        assert_eq!(source.get(".config/x").unwrap(), None);
        assert_eq!(source.get("bin").unwrap(), None);
        assert_eq!(source.get("absent").unwrap(), None);
        assert!(source.get(".git/config").is_err());

        let bad = BTreeMap::from([(".plain".to_owned(), "rwx".to_owned())]);
        assert!(Worktree::new(&repo, &bad).get(".plain").is_err());
    }

    #[test]
    fn variants_read_the_winning_source_with_its_own_mode() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        fs::write(repo.join("a.conf"), b"plain").unwrap();
        fs::write(repo.join("a{host=x}.conf"), b"x").unwrap();
        let modes = BTreeMap::from([("a{host=x}.conf".to_owned(), "0600".to_owned())]);
        let sources = Sources::group(["a.conf", "a{host=x}.conf"]);
        let x = IdentitySnapshot {
            host: "x".to_owned(),
            ..IdentitySnapshot::default()
        };
        let variants = Variants::new(Worktree::new(repo, &modes), &sources, &x);
        let Selection::Winner(entry) = variants.get("a.conf").unwrap() else {
            unreachable!("a.conf has a winner on host x");
        };
        assert_eq!(entry.source, "a{host=x}.conf");
        assert_eq!(entry.tuple, Tuple::of_file(b"x", 0o600));
        assert_eq!(entry.content, Content::Bytes(b"x".to_vec()));
        let other = IdentitySnapshot::default();
        assert_eq!(variants.winner_under("a.conf", &other), Some("a.conf"));
        assert_eq!(variants.get("missing").unwrap(), Selection::None);
    }

    #[test]
    fn a_source_switch_never_captures_and_records_the_new_source() {
        assert_eq!(Row::ChangedLocally.switched(), Row::BothChanged);
        assert_eq!(
            Row::Clean {
                update_state: false
            }
            .switched(),
            Row::Clean { update_state: true }
        );
        for row in [
            Row::ChangedInRepo,
            Row::BothChanged,
            Row::Forgotten,
            Row::DeletedLocally,
        ] {
            assert_eq!(row.switched(), row);
        }
    }
}
