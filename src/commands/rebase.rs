//! Rebase command implementation
//!
//! This module provides git-style rebase functionality, allowing users to replay
//! commits on top of a new base commit. Supports continuation after conflict resolution,
//! aborting to restore original state, and skipping problematic commits.

use crate::DotmanContext;
use crate::commands::context::CommandContext;
use crate::conflicts::{detect_conflicts, write_conflict_markers};
use crate::dag;
use crate::output;
use crate::rebase::RebaseState;
use crate::refs::RefManager;
use crate::storage::file_ops::hash_bytes;
use crate::storage::index::Index;
use crate::storage::snapshots::{Snapshot, SnapshotManager};
use crate::storage::{Commit, FileEntry};
use crate::utils::formatters::format_commit_id;
use crate::utils::{commit::generate_commit_id, get_precise_timestamp, get_user_from_config};
use anyhow::{Context, Result};
use colored::Colorize;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

/// Execute rebase command with subcommand routing
///
/// # Arguments
///
/// * `ctx` - The dotman context
/// * `upstream` - The commit/branch to rebase onto (None for --continue, --abort, --skip)
/// * `branch` - Optional branch to rebase (defaults to current branch)
/// * `continue_rebase` - Whether to continue after conflict resolution
/// * `abort` - Whether to abort and restore original state
/// * `skip` - Whether to skip the current commit
///
/// # Errors
///
/// Returns an error if the operation fails
pub fn execute(
    ctx: &DotmanContext,
    upstream: Option<&str>,
    branch: Option<&str>,
    continue_rebase: bool,
    abort: bool,
    skip: bool,
) -> Result<()> {
    ctx.ensure_initialized()?;

    // Route to appropriate subcommand
    if continue_rebase {
        execute_continue(ctx)
    } else if abort {
        execute_abort(ctx)
    } else if skip {
        execute_skip(ctx)
    } else {
        // Start a new rebase
        let upstream = upstream.context("Missing upstream argument for rebase")?;
        execute_start(ctx, upstream, branch)
    }
}

/// Start a new rebase operation
///
/// # Arguments
///
/// * `ctx` - The dotman context
/// * `upstream` - The commit/branch to rebase onto
/// * `branch` - Optional branch to rebase (defaults to current branch)
///
/// # Errors
///
/// Returns an error if:
/// - A rebase is already in progress
/// - The upstream or branch cannot be resolved
/// - The rebase fails
pub fn execute_start(ctx: &DotmanContext, upstream: &str, branch: Option<&str>) -> Result<()> {
    // Check if rebase is already in progress
    if RebaseState::is_in_progress(&ctx.repo_path) {
        anyhow::bail!(
            "A rebase is already in progress.\n\
            Use 'dot rebase --continue' to continue,\n\
            'dot rebase --abort' to abort, or\n\
            'dot rebase --skip' to skip the current commit."
        );
    }

    let resolver = ctx.create_ref_resolver();
    let ref_manager = RefManager::new(ctx.repo_path.clone());

    // Resolve upstream commit
    let onto_commit = resolver
        .resolve(upstream)
        .with_context(|| format!("Failed to resolve upstream: {upstream}"))?;

    // If branch is specified, use it as the target; otherwise use current HEAD/branch
    let (original_head, original_branch, rebase_from) = if let Some(branch_name) = branch {
        let commit = resolver
            .resolve(branch_name)
            .with_context(|| format!("Failed to resolve branch: {branch_name}"))?;
        (commit.clone(), Some(branch_name.to_string()), commit)
    } else {
        let head = ref_manager
            .get_head_commit()?
            .context("No commits in current branch")?;
        let current_branch = ref_manager.current_branch()?;
        (head.clone(), current_branch, head)
    };

    // Check if already up to date
    if rebase_from == onto_commit {
        output::info("Current branch is up to date.");
        return Ok(());
    }

    let snapshot_manager = ctx.create_snapshot_manager();

    // Check if this is a fast-forward (rebase_from is ancestor of onto_commit)
    if dag::is_ancestor(&snapshot_manager, &rebase_from, &onto_commit) {
        output::info("Fast-forwarding...");
        // Update HEAD to onto_commit
        if let Some(current_branch) = &original_branch {
            ref_manager.update_branch(current_branch, &onto_commit)?;
        } else {
            ref_manager.set_head_to_commit(
                &onto_commit,
                Some("rebase"),
                Some(&format!("rebase: fast-forward to {}", &onto_commit[..8])),
            )?;
        }
        // Update working directory
        crate::commands::checkout::execute(ctx, &onto_commit, false, false)?;
        output::success(&format!(
            "Fast-forwarded to {}",
            format_commit_id(&onto_commit).yellow()
        ));
        return Ok(());
    }

    // Find commits to replay
    let common_ancestor = dag::find_common_ancestor(&snapshot_manager, &rebase_from, &onto_commit)
        .context("Could not find common ancestor")?;

    let commits_to_replay = collect_commits_between(ctx, &common_ancestor, &rebase_from);

    if commits_to_replay.is_empty() {
        output::info("Nothing to rebase.");
        return Ok(());
    }

    output::info(&format!(
        "Rebasing {} commit(s) onto {}",
        commits_to_replay.len(),
        format_commit_id(&onto_commit).yellow()
    ));

    // Create rebase state
    let state = RebaseState::new(
        onto_commit.clone(),
        original_head,
        original_branch.clone(),
        commits_to_replay,
    );
    state.save(&ctx.repo_path)?;

    // Reset HEAD to onto commit before replaying
    if let Some(branch) = &original_branch {
        ref_manager.update_branch(branch, &onto_commit)?;
    }
    ref_manager.set_head_to_commit(
        &onto_commit,
        Some("rebase"),
        Some(&format!("rebase (start): checkout {}", &onto_commit[..8])),
    )?;

    // Checkout onto commit to update working directory
    crate::commands::checkout::execute(ctx, &onto_commit, true, false)?;

    // Start replaying commits
    replay_commits(ctx, state)
}

/// Internal entry point for starting rebase (called from pull.rs)
///
/// # Errors
///
/// Returns an error if the rebase fails
pub fn execute_start_internal(ctx: &DotmanContext, onto_commit: &str) -> Result<()> {
    execute_start(ctx, onto_commit, None)
}

/// Continue a rebase after resolving conflicts
///
/// # Errors
///
/// Returns an error if:
/// - No rebase is in progress
/// - Conflicts still exist
/// - The rebase fails
pub fn execute_continue(ctx: &DotmanContext) -> Result<()> {
    let mut state = RebaseState::load(&ctx.repo_path)?
        .context("No rebase in progress. Use 'dot rebase <upstream>' to start a rebase.")?;

    output::info("Continuing rebase...");

    // Check if conflicts are resolved
    let home_dir = ctx.get_home_dir()?;
    for conflict_file in &state.conflict_files {
        let abs_path = if conflict_file.is_relative() {
            home_dir.join(conflict_file)
        } else {
            conflict_file.clone()
        };

        if abs_path.exists() {
            let content = fs::read_to_string(&abs_path)?;
            if crate::conflicts::ConflictMarker::has_markers(&content) {
                anyhow::bail!(
                    "Conflict markers still present in: {}\n\
                    Please resolve all conflicts and stage the changes with 'dot add' before continuing.",
                    conflict_file.display()
                );
            }
        }
    }

    // Create the rebased commit for the current (just-resolved) commit
    let current_commit_id = state
        .current_commit()
        .context("No current commit to continue")?
        .to_string();

    let snapshot_manager = ctx.create_snapshot_manager();
    let commit_snapshot = snapshot_manager
        .load_snapshot(&current_commit_id)
        .with_context(|| format!("Failed to load commit: {current_commit_id}"))?;

    let index = ctx.load_index()?;

    // Create rebased commit
    create_rebased_commit_for_branch(
        ctx,
        &index,
        &commit_snapshot.commit,
        state.original_branch.as_deref(),
    )?;

    // Clear staging area after commit
    let index_path = ctx.repo_path.join("index.bin");
    let clean_index = Index::new();
    clean_index.save(&index_path)?;

    // Move to next commit
    state.advance();
    state.save(&ctx.repo_path)?;

    // Continue replaying
    replay_commits(ctx, state)
}

/// Abort a rebase and restore original state
///
/// # Errors
///
/// Returns an error if:
/// - No rebase is in progress
/// - Failed to restore original state
pub fn execute_abort(ctx: &DotmanContext) -> Result<()> {
    let state = RebaseState::load(&ctx.repo_path)?.context("No rebase in progress.")?;

    output::info("Aborting rebase...");

    let ref_manager = RefManager::new(ctx.repo_path.clone());

    // Restore original HEAD
    if let Some(branch) = &state.original_branch {
        ref_manager.update_branch(branch, &state.original_head)?;
        ref_manager.set_head_to_branch(
            branch,
            Some("rebase"),
            Some(&format!("rebase: abort to {}", &state.original_head[..8])),
        )?;
    } else {
        ref_manager.set_head_to_commit(
            &state.original_head,
            Some("rebase"),
            Some(&format!("rebase: abort to {}", &state.original_head[..8])),
        )?;
    }

    // Restore working directory (force to override conflict markers)
    crate::commands::checkout::execute(ctx, &state.original_head, true, false)?;

    // Clear rebase state
    RebaseState::clear(&ctx.repo_path)?;

    output::success("Rebase aborted. HEAD is now at original position.");

    Ok(())
}

/// Skip the current commit during rebase
///
/// # Errors
///
/// Returns an error if:
/// - No rebase is in progress
/// - The rebase fails
pub fn execute_skip(ctx: &DotmanContext) -> Result<()> {
    let mut state = RebaseState::load(&ctx.repo_path)?.context("No rebase in progress.")?;

    let current_commit = state
        .current_commit()
        .context("No current commit to skip")?;
    output::info(&format!(
        "Skipping commit {}",
        format_commit_id(current_commit).yellow()
    ));

    // Restore working directory to HEAD (clean up conflict markers)
    let ref_manager = RefManager::new(ctx.repo_path.clone());
    if let Some(head) = ref_manager.get_head_commit()? {
        crate::commands::checkout::execute(ctx, &head, true, false)?;
    }

    // Move to next commit
    state.advance();
    state.save(&ctx.repo_path)?;

    // Continue replaying
    replay_commits(ctx, state)
}

/// Replay commits from the rebase state
///
/// # Errors
///
/// Returns an error if:
/// - A commit cannot be loaded
/// - A conflict occurs during cherry-pick
/// - Snapshot creation fails
fn replay_commits(ctx: &DotmanContext, mut state: RebaseState) -> Result<()> {
    let snapshot_manager = ctx.create_snapshot_manager();
    let total = state.total_commits();
    let mut progress = output::start_progress("Replaying commits", total);

    while !state.is_complete() {
        let commit_id = state
            .current_commit()
            .context("Failed to get current commit")?
            .to_string();

        progress.update(state.current_index + 1);

        // Cherry-pick the commit
        match cherry_pick_commit(ctx, &snapshot_manager, &state, &commit_id) {
            Ok(()) => {
                // Success, advance to next commit
                state.advance();
                state.save(&ctx.repo_path)?;
            }
            Err(e) => {
                progress.finish();
                // Check if this is a conflict
                if e.to_string().contains("conflicts") {
                    output::warning(
                        "Conflicts detected. Resolve conflicts and run 'dot rebase --continue'",
                    );
                    output::info("Or use 'dot rebase --skip' to skip this commit");
                    output::info("Or use 'dot rebase --abort' to abort the rebase");
                }
                return Err(e);
            }
        }
    }

    progress.finish();

    // Rebase complete
    RebaseState::clear(&ctx.repo_path)?;
    output::success("Successfully rebased and updated HEAD.");

    Ok(())
}

/// Cherry-pick a single commit onto the current HEAD
///
/// # Errors
///
/// Returns an error if:
/// - The commit cannot be loaded
/// - Conflicts occur
/// - The new commit cannot be created
fn cherry_pick_commit(
    ctx: &DotmanContext,
    snapshot_manager: &SnapshotManager,
    state: &RebaseState,
    commit_id: &str,
) -> Result<()> {
    // Load the commit to replay
    let commit_snapshot = snapshot_manager
        .load_snapshot(commit_id)
        .with_context(|| format!("Failed to load commit: {commit_id}"))?;

    // Get current HEAD
    let ref_manager = RefManager::new(ctx.repo_path.clone());
    let current_head = ref_manager.get_head_commit()?.context("No HEAD commit")?;

    // Load current HEAD snapshot
    let head_snapshot = snapshot_manager
        .load_snapshot(&current_head)
        .with_context(|| format!("Failed to load HEAD: {current_head}"))?;

    // Get first parent of commit being replayed (for three-way merge)
    let parent_snapshot = if let Some(parent_id) = commit_snapshot.commit.parents.first() {
        Some(
            snapshot_manager
                .load_snapshot(parent_id)
                .with_context(|| format!("Failed to load parent: {parent_id}"))?,
        )
    } else {
        None
    };

    // Apply the commit's changes
    apply_commit_changes(
        ctx,
        snapshot_manager,
        state,
        &head_snapshot,
        &commit_snapshot,
        parent_snapshot.as_ref(),
    )?;

    Ok(())
}

/// Apply changes from a commit to the current working directory
///
/// # Errors
///
/// Returns an error if:
/// - Conflicts are detected
/// - Files cannot be written
/// - The new commit cannot be created
#[allow(clippy::too_many_lines)]
fn apply_commit_changes(
    ctx: &DotmanContext,
    snapshot_manager: &SnapshotManager,
    state: &RebaseState,
    head_snapshot: &Snapshot,
    commit_snapshot: &Snapshot,
    parent_snapshot: Option<&Snapshot>,
) -> Result<()> {
    let home_dir = ctx.get_home_dir()?;
    let objects_path = ctx.repo_path.join("objects");
    let index_path = ctx.repo_path.join("index.bin");
    let mut index = ctx.load_index()?;

    // Detect conflicts
    let all_conflicts = detect_conflicts(head_snapshot, commit_snapshot, parent_snapshot)?;

    // Filter out false conflicts due to dotman's snapshot model.
    // In dotman, snapshots only contain files that were staged for that commit.
    // A file missing from commit_snapshot doesn't mean "deleted by commit" -
    // it means "not changed by this commit". So we only report real conflicts
    // where the commit actually touched the file.
    let conflicts: Vec<_> = all_conflicts
        .into_iter()
        .filter(|c| {
            // If remote_hash is empty (not in commit) and base_hash exists (was in parent),
            // the commit didn't change this file - not a real conflict for rebase
            !(c.remote_hash.is_empty() && c.base_hash.is_some())
        })
        .collect();

    if !conflicts.is_empty() {
        // Write conflict markers to files
        output::warning(&format!("Conflicts in {} file(s)", conflicts.len()));

        let mut state_mut = state.clone();
        state_mut.conflict_files = conflicts.iter().map(|c| c.path.clone()).collect();
        state_mut.save(&ctx.repo_path)?;

        for conflict in &conflicts {
            let target_path = if conflict.path.is_relative() {
                home_dir.join(&conflict.path)
            } else {
                conflict.path.clone()
            };

            write_conflict_markers(
                conflict,
                snapshot_manager,
                &objects_path,
                &target_path,
                &format!("rebase-{}", &commit_snapshot.commit.id[..8]),
            )
            .with_context(|| {
                format!(
                    "Failed to write conflict markers: {}",
                    conflict.path.display()
                )
            })?;

            output::error(&format!("  CONFLICT: {}", conflict.path.display()));
        }

        anyhow::bail!("Automatic merge failed. Fix conflicts and run 'dot rebase --continue'");
    }

    // No conflicts - apply changes
    let all_paths: HashSet<PathBuf> = head_snapshot
        .files
        .keys()
        .chain(commit_snapshot.files.keys())
        .cloned()
        .collect();

    for path in all_paths {
        let in_head = head_snapshot.files.get(&path);
        let in_commit = commit_snapshot.files.get(&path);
        let in_parent = parent_snapshot.and_then(|p| p.files.get(&path));

        // Determine action based on three-way merge logic
        match (in_head, in_commit, in_parent) {
            // File exists in commit but not in parent (added)
            (_, Some(commit_file), None) => {
                // Copy file to working directory
                let target_path = if path.is_relative() {
                    home_dir.join(&path)
                } else {
                    path.clone()
                };

                snapshot_manager.restore_file_content(&commit_file.content_hash, &target_path)?;

                // Stage the file
                let metadata = fs::metadata(&target_path)?;
                let entry = FileEntry {
                    path: path.clone(),
                    hash: commit_file.hash.clone(),
                    size: metadata.len(),
                    modified: metadata
                        .modified()?
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_secs()
                        .cast_signed(),
                    mode: commit_file.mode,
                    cached_hash: None,
                };
                index.stage_entry(entry);
            }
            // File exists in parent but not in commit (deleted)
            (_, None, Some(_)) => {
                // Delete file from working directory
                let target_path = if path.is_relative() {
                    home_dir.join(&path)
                } else {
                    path.clone()
                };

                if target_path.exists() {
                    fs::remove_file(&target_path).with_context(|| {
                        format!("Failed to remove file: {}", target_path.display())
                    })?;
                }

                // Mark as deleted in index
                index.mark_deleted(&path);
            }
            // File exists in both and changed from parent (modified)
            (_, Some(commit_file), Some(parent_file)) if commit_file.hash != parent_file.hash => {
                let target_path = if path.is_relative() {
                    home_dir.join(&path)
                } else {
                    path.clone()
                };

                snapshot_manager.restore_file_content(&commit_file.content_hash, &target_path)?;

                // Stage the file
                let metadata = fs::metadata(&target_path)?;
                let entry = FileEntry {
                    path: path.clone(),
                    hash: commit_file.hash.clone(),
                    size: metadata.len(),
                    modified: metadata
                        .modified()?
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_secs()
                        .cast_signed(),
                    mode: commit_file.mode,
                    cached_hash: None,
                };
                index.stage_entry(entry);
            }
            _ => {
                // No action needed for other cases
            }
        }
    }

    // Create new commit with the replayed changes
    create_rebased_commit_for_branch(
        ctx,
        &index,
        &commit_snapshot.commit,
        state.original_branch.as_deref(),
    )?;

    // Save index
    index.save(&index_path)?;

    Ok(())
}

/// Create a new commit for the rebased changes, updating a specific branch
///
/// # Arguments
///
/// * `ctx` - The dotman context
/// * `index` - The index containing staged changes
/// * `original_commit` - The original commit being replayed (for message)
/// * `branch_name` - Optional branch name to update (None for detached HEAD)
///
/// # Errors
///
/// Returns an error if snapshot creation fails
fn create_rebased_commit_for_branch(
    ctx: &DotmanContext,
    index: &Index,
    original_commit: &Commit,
    branch_name: Option<&str>,
) -> Result<()> {
    let (timestamp, nanos) = get_precise_timestamp();
    let author = get_user_from_config(&ctx.config);

    let ref_manager = RefManager::new(ctx.repo_path.clone());
    let parent = ref_manager.get_head_commit()?;

    // Calculate tree hash
    let mut tree_content = String::new();
    for (path, entry) in &index.staged_entries {
        use std::fmt::Write;
        let _ = writeln!(&mut tree_content, "{} {}", entry.hash, path.display());
    }
    for path in &index.deleted_entries {
        use std::fmt::Write;
        let _ = writeln!(&mut tree_content, "DELETED {}", path.display());
    }
    let tree_hash = hash_bytes(tree_content.as_bytes());

    // Generate new commit ID
    let parents: Vec<String> = parent.into_iter().collect();
    let parent_refs: Vec<&str> = parents.iter().map(String::as_str).collect();
    let commit_id = generate_commit_id(
        &tree_hash,
        &parent_refs,
        &original_commit.message,
        &author,
        timestamp,
        nanos,
    );

    let commit = Commit {
        id: commit_id.clone(),
        parents,
        message: original_commit.message.clone(),
        author,
        timestamp,
        tree_hash,
    };

    // Create snapshot
    let snapshot_manager = ctx.create_snapshot_manager();
    let files: Vec<FileEntry> = index
        .staged_entries
        .iter()
        .filter(|(path, _)| !index.deleted_entries.contains(*path))
        .map(|(_, entry)| entry.clone())
        .collect();

    snapshot_manager.create_snapshot(commit.clone(), &files, None::<fn(usize)>)?;

    // Update HEAD and branch
    if let Some(branch) = branch_name {
        ref_manager.update_branch(branch, &commit_id)?;
        ref_manager.set_head_to_branch(
            branch,
            Some("rebase"),
            Some(&format!("rebase: {}", commit.message)),
        )?;
    } else {
        ref_manager.set_head_to_commit(
            &commit_id,
            Some("rebase"),
            Some(&format!("rebase: {}", commit.message)),
        )?;
    }

    Ok(())
}

/// Collect all commits between `from` (exclusive) and `to` (inclusive)
///
/// Returns commits in chronological order (oldest first)
fn collect_commits_between(ctx: &DotmanContext, from: &str, to: &str) -> Vec<String> {
    let snapshot_manager = ctx.create_snapshot_manager();
    let mut commits = Vec::new();

    let mut current = Some(to.to_string());
    while let Some(commit_id) = current {
        if commit_id == from {
            break;
        }

        commits.push(commit_id.clone());

        if let Ok(snapshot) = snapshot_manager.load_snapshot(&commit_id) {
            current = snapshot.commit.parents.first().cloned();
        } else {
            break;
        }
    }

    // Reverse to get chronological order (oldest first)
    commits.reverse();

    commits
}
