//! Working tree status inspection.
//!
//! This module provides functionality for inspecting the current state of the
//! working tree, similar to `git status`. It handles:
//!
//! - Detection of staged changes (files ready to commit)
//! - Detection of unstaged modifications (changes in working directory)
//! - Detection of deleted files
//! - Untracked file discovery
//! - Short and long output formats
//! - Cache statistics for performance analysis
//!
//! # Output Formats
//!
//! - **Long format** (default): Grouped by status with detailed information
//! - **Short format** (`-s`): Compact single-line per file output
//! - **Verbose** (`-v`): Includes cache hit rate statistics
//!
//! # Examples
//!
//! ```no_run
//! use dotman::DotmanContext;
//! use dotman::commands::status;
//!
//! # fn main() -> anyhow::Result<()> {
//! let ctx = DotmanContext::new()?;
//!
//! // Show full status
//! status::execute(&ctx, false, false)?;
//!
//! // Show short status
//! status::execute(&ctx, true, false)?;
//!
//! // Show status with untracked files
//! status::execute(&ctx, false, true)?;
//! # Ok(())
//! # }
//! ```

use crate::refs::RefManager;
use crate::scanner::{DirTrie, find_untracked_files};
use crate::storage::FileStatus;
use crate::storage::index::Index;
use crate::{DotmanContext, INDEX_FILE};
use anyhow::{Context, Result};
use colored::Colorize;
use std::collections::HashSet;
use std::path::PathBuf;

/// Show working tree status
///
/// # Errors
///
/// Returns an error if:
/// - The repository is not initialized
/// - Cannot read the index
/// - File status checks fail
pub fn execute(ctx: &DotmanContext, short: bool, show_untracked: bool) -> Result<()> {
    execute_verbose(ctx, short, show_untracked, false)
}

/// Show working tree status with optional cache statistics
///
/// # Errors
///
/// Returns an error if:
/// - The repository is not initialized
/// - Cannot read the index
/// - File status checks fail
#[allow(clippy::too_many_lines)]
#[allow(clippy::cognitive_complexity)]
pub fn execute_verbose(
    ctx: &DotmanContext,
    short: bool,
    show_untracked: bool,
    verbose: bool,
) -> Result<()> {
    ctx.check_repo_initialized()?;

    let ref_manager = RefManager::new(ctx.repo_path.clone());
    if let Some(branch) = ref_manager.current_branch()? {
        println!("On branch {}", branch.bold());
    } else if let Some(commit) = ref_manager.get_head_commit()? {
        println!(
            "HEAD detached at {}",
            commit[..8.min(commit.len())].yellow()
        );
    }

    let index_path = ctx.repo_path.join(INDEX_FILE);
    let index = Index::load(&index_path)?;

    let placeholder_commit = "0".repeat(40);
    let has_commits = ref_manager
        .get_head_commit()?
        .is_some_and(|c| c != placeholder_commit);

    // Load HEAD snapshot to get committed files (source of truth)
    let committed_files = ref_manager.get_head_commit()?.and_then(|commit_id| {
        if commit_id == placeholder_commit {
            None
        } else {
            let snapshot_manager = crate::storage::snapshots::SnapshotManager::new(
                ctx.repo_path.clone(),
                ctx.config.core.compression_level,
            );
            snapshot_manager
                .load_snapshot(&commit_id)
                .map(|snapshot| snapshot.files)
                .ok()
        }
    });

    if committed_files.is_none() && index.staged_entries.is_empty() {
        if !has_commits {
            println!("\nNo commits yet");
        }
        println!("\nnothing to add (use \"dot add\" to track files)");
        return Ok(());
    }

    let mut statuses = Vec::new();
    let home = dirs::home_dir().context("Could not find home directory")?;

    // Load the last commit snapshot to check if files are new or modified
    let last_commit_files = ref_manager
        .get_head_commit()?
        .filter(|id| id != &placeholder_commit)
        .and_then(|commit_id| {
            let snapshot_manager = crate::storage::snapshots::SnapshotManager::new(
                ctx.repo_path.clone(),
                ctx.config.core.compression_level,
            );
            snapshot_manager.load_snapshot(&commit_id).ok()
        })
        .map(|snapshot| snapshot.files);

    // Helper to determine file status
    let file_status = |path: &PathBuf| -> FileStatus {
        let in_last_commit = last_commit_files
            .as_ref()
            .is_some_and(|files| files.contains_key(path));

        if in_last_commit {
            FileStatus::Modified(path.clone())
        } else {
            FileStatus::Added(path.clone())
        }
    };

    // Invariant: no file should be both staged and deleted
    // Index::mark_deleted() enforces this by removing from staged_entries
    debug_assert!(
        index
            .staged_entries
            .keys()
            .all(|k| !index.deleted_entries.contains(k)),
        "Invariant violated: file is both staged and deleted"
    );

    // Check staged entries - show ALL staged files (Git-like semantics)
    for path in index.staged_entries.keys() {
        // Skip files in deleted_entries to avoid showing them twice
        if index.deleted_entries.contains(path) {
            continue;
        }

        // Add all staged files to status, regardless of whether they differ from committed
        // This matches Git behavior: if a file is staged, it will be committed
        statuses.push(file_status(path));
    }

    // Check for deleted entries
    for path in &index.deleted_entries {
        statuses.push(FileStatus::Deleted(path.clone()));
    }

    // Track files that couldn't be checked due to errors
    let mut check_errors: Vec<(PathBuf, String)> = Vec::new();

    // Check if staged files were modified on disk
    for (path, staged_entry) in &index.staged_entries {
        // Skip files already in deleted_entries to avoid duplicates
        if index.deleted_entries.contains(path) {
            continue;
        }

        let abs_path = if path.is_relative() {
            home.join(path)
        } else {
            path.clone()
        };

        if abs_path.exists() {
            // Use cached hash for performance
            match crate::storage::file_ops::hash_file(&abs_path, staged_entry.cached_hash.as_ref())
            {
                Ok((current_hash, _)) => {
                    if current_hash != staged_entry.hash {
                        statuses.push(FileStatus::Modified(path.clone()));
                    }
                }
                Err(e) => {
                    // Hash failed - check if deleted or inaccessible
                    if abs_path.exists() {
                        // File exists but can't be hashed - log error
                        check_errors.push((path.clone(), format!("{e:#}")));
                    } else {
                        statuses.push(FileStatus::Deleted(path.clone()));
                    }
                }
            }
        } else {
            statuses.push(FileStatus::Deleted(path.clone()));
        }
    }

    // Check if committed files were modified on disk (not already staged)
    if let Some(ref files) = committed_files {
        for (path, snapshot_file) in files {
            // Skip if already staged (already checked above)
            if index.staged_entries.contains_key(path) {
                continue;
            }

            let abs_path = if path.is_relative() {
                home.join(path)
            } else {
                path.clone()
            };

            if abs_path.exists() {
                // Hash file to check for modifications (no cache available from snapshot)
                match crate::storage::file_ops::hash_file(&abs_path, None) {
                    Ok((current_hash, _)) => {
                        if current_hash != snapshot_file.hash {
                            statuses.push(FileStatus::Modified(path.clone()));
                        }
                    }
                    Err(e) => {
                        // Log error but continue checking other files
                        check_errors.push((path.clone(), format!("{e:#}")));
                    }
                }
            } else {
                // File was deleted from disk
                statuses.push(FileStatus::Deleted(path.clone()));
            }
        }
    }

    // Report any files that couldn't be checked
    if !check_errors.is_empty() {
        eprintln!("{}", "Warning: Could not check some files:".yellow());
        for (path, error) in &check_errors {
            eprintln!("  {}: {}", path.display(), error);
        }
    }

    if show_untracked {
        // Build trie and tracked files set for untracked file discovery
        let mut trie = DirTrie::new();
        let mut tracked_files = HashSet::new();

        // Add committed files
        if let Some(ref files) = committed_files {
            for path in files.keys() {
                let abs_path = if path.is_relative() {
                    home.join(path)
                } else {
                    path.clone()
                };
                trie.insert_tracked_file(&abs_path, &home);
                tracked_files.insert(abs_path);
            }
        }

        // Add staged files
        for path in index.staged_entries.keys() {
            let abs_path = if path.is_relative() {
                home.join(path)
            } else {
                path.clone()
            };
            trie.insert_tracked_file(&abs_path, &home);
            tracked_files.insert(abs_path);
        }

        let untracked = find_untracked_files(&home, &ctx.repo_path, &trie, &tracked_files)?;
        for file in untracked {
            // Check against ignore patterns
            let relative_path = file.strip_prefix(&home).unwrap_or(&file);
            if !crate::utils::should_ignore(relative_path, &ctx.config.tracking.ignore_patterns) {
                statuses.push(FileStatus::Untracked(file));
            }
        }
    }

    if statuses.is_empty() {
        println!("\nnothing to commit, working tree clean");

        // Show cache statistics in verbose mode
        if verbose {
            let (total, cached, hit_rate) = index.get_cache_stats();
            println!("\n{}", "Cache Statistics:".bold());
            println!("  Total entries: {total}");
            println!("  Cached entries: {cached}");
            println!("  Cache hit rate: {:.1}%", hit_rate * 100.0);
        }

        return Ok(());
    }

    statuses.sort_by_key(|s| (s.status_char(), s.path().to_path_buf()));

    if short {
        for status in statuses {
            println!("{} {}", status.status_char(), status.path().display());
        }
    } else {
        // Separate staged and unstaged modifications
        let staged_new: Vec<&FileStatus> = statuses
            .iter()
            .filter(|s| matches!(s, FileStatus::Added(_)))
            .collect();

        let staged_modified: Vec<&FileStatus> = statuses
            .iter()
            .filter(|s| {
                matches!(s, FileStatus::Modified(_)) && {
                    // Check if the file is in staged_entries
                    if let FileStatus::Modified(p) = s {
                        index.staged_entries.contains_key(p)
                    } else {
                        false
                    }
                }
            })
            .collect();

        // Explicit deletions via `dot rm` go under "Changes to be committed"
        let staged_deleted: Vec<&FileStatus> = statuses
            .iter()
            .filter(|s| {
                if let FileStatus::Deleted(p) = s {
                    index.deleted_entries.contains(p)
                } else {
                    false
                }
            })
            .collect();
        if !staged_new.is_empty() || !staged_modified.is_empty() || !staged_deleted.is_empty() {
            println!("\n{}:", "Changes to be committed:".bold());
            for status in &staged_new {
                println!("  {}: {}", "new file".green(), status.path().display());
            }
            for status in &staged_modified {
                println!("  {}: {}", "modified".yellow(), status.path().display());
            }
            for status in &staged_deleted {
                println!("  {}: {}", "deleted".red(), status.path().display());
            }
        }

        // Print unstaged modifications
        let unstaged_modified: Vec<&FileStatus> = statuses
            .iter()
            .filter(|s| {
                matches!(s, FileStatus::Modified(_)) && {
                    if let FileStatus::Modified(p) = s {
                        !index.staged_entries.contains_key(p)
                    } else {
                        false
                    }
                }
            })
            .collect();

        // Files deleted from disk (but still staged) go under "Changes not staged"
        let unstaged_deleted: Vec<&FileStatus> = statuses
            .iter()
            .filter(|s| {
                if let FileStatus::Deleted(p) = s {
                    !index.deleted_entries.contains(p)
                } else {
                    false
                }
            })
            .collect();
        if !unstaged_modified.is_empty() || !unstaged_deleted.is_empty() {
            println!("\n{}:", "Changes not staged for commit:".bold());
            for status in &unstaged_modified {
                println!("  {}: {}", "modified".yellow(), status.path().display());
            }
            for status in &unstaged_deleted {
                println!("  {}: {}", "deleted".red(), status.path().display());
            }
        }

        print_status_group(
            &statuses,
            &FileStatus::Untracked(PathBuf::new()),
            "Untracked files:",
            "untracked",
        );
    }

    // Show cache statistics in verbose mode
    if verbose {
        let (total, cached, hit_rate) = index.get_cache_stats();
        println!("\n{}", "Cache Statistics:".bold());
        println!("  Total entries: {total}");
        println!("  Cached entries: {cached}");
        println!("  Cache hit rate: {:.1}%", hit_rate * 100.0);
    }

    Ok(())
}

/// Returns absolute paths of all tracked files
///
/// # Errors
///
/// Returns an error if:
/// - Cannot load the HEAD snapshot
/// - Cannot determine home directory
pub fn get_current_files(ctx: &DotmanContext) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let home = dirs::home_dir().context("Could not find home directory")?;

    // Load files from HEAD snapshot (committed files)
    let ref_manager = crate::refs::RefManager::new(ctx.repo_path.clone());
    if let Some(commit_id) = ref_manager.get_head_commit()?
        && commit_id != "0".repeat(40)
    {
        let snapshot_manager = crate::storage::snapshots::SnapshotManager::new(
            ctx.repo_path.clone(),
            ctx.config.core.compression_level,
        );
        if let Ok(snapshot) = snapshot_manager.load_snapshot(&commit_id) {
            for path in snapshot.files.keys() {
                let abs_path = if path.is_relative() {
                    home.join(path)
                } else {
                    path.clone()
                };
                files.push(abs_path);
            }
        }
    }

    Ok(files)
}

/// Print a group of file statuses with a common status type.
///
/// This helper function filters statuses by discriminant type and prints
/// them in a formatted group with a header and colored labels.
///
/// # Arguments
///
/// * `statuses` - All file statuses to filter from
/// * `status_type` - Status type to match (discriminant comparison)
/// * `header` - Section header to print
/// * `label` - Status label for each file
fn print_status_group(
    statuses: &[FileStatus],
    status_type: &FileStatus,
    header: &str,
    label: &str,
) {
    let filtered: Vec<&FileStatus> = statuses
        .iter()
        .filter(|s| std::mem::discriminant(*s) == std::mem::discriminant(status_type))
        .collect();

    if !filtered.is_empty() {
        println!("\n{}:", header.bold());
        for status in filtered {
            let color_label = match status {
                FileStatus::Added(_) => label.green(),
                FileStatus::Modified(_) => label.yellow(),
                FileStatus::Deleted(_) => label.red(),
                FileStatus::Untracked(_) => label.white(),
            };
            println!("  {}: {}", color_label, status.path().display());
        }
    }
}
