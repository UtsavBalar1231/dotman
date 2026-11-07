use crate::DotmanContext;
use crate::commands::context::CommandContext;
use crate::output;
use crate::refs::resolver::RefResolver;
use crate::storage::Commit;
use crate::storage::snapshots::{Snapshot, SnapshotManager};
use crate::utils::pager::{Pager, PagerConfig, PagerWriter};
use crate::utils::paths::expand_tilde;
use anyhow::Result;
use chrono::{Local, TimeZone};
use colored::Colorize;
use glob::{MatchOptions, Pattern};
use std::collections::{BinaryHeap, HashSet};
use std::path::PathBuf;

/// Format and display a single commit
fn display_commit(writer: &mut dyn PagerWriter, commit: &Commit, oneline: bool) -> Result<()> {
    if oneline {
        let display_id = if commit.id.len() >= 8 {
            &commit.id[..8]
        } else {
            &commit.id
        };
        writeln!(writer, "{} {}", display_id.yellow(), commit.message)?;
    } else {
        writeln!(writer, "{} {}", "commit".yellow(), commit.id)?;

        if !commit.parents.is_empty() {
            let parent_display: Vec<String> = commit
                .parents
                .iter()
                .map(|p| p[..8.min(p.len())].to_string())
                .collect();
            writeln!(
                writer,
                "{}: {}",
                if commit.parents.len() > 1 {
                    "Parents"
                } else {
                    "Parent"
                }
                .bold(),
                parent_display.join(", ")
            )?;
        }

        writeln!(writer, "{}: {}", "Author".bold(), commit.author)?;

        let datetime = Local
            .timestamp_opt(commit.timestamp, 0)
            .single()
            .unwrap_or_else(Local::now);
        writeln!(
            writer,
            "{}: {}",
            "Date".bold(),
            datetime.format("%Y-%m-%d %H:%M:%S")
        )?;

        writeln!(writer, "\n    {}\n", commit.message)?;
    }
    Ok(())
}

/// Check if a string contains glob metacharacters
fn is_glob_pattern(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// Parse and normalize a single path argument
fn parse_path(ctx: &DotmanContext, path_str: &str) -> Result<PathBuf> {
    use std::path::Path;

    // Expand tilde
    let expanded = expand_tilde(Path::new(path_str))?;

    // Normalize to home-relative path (matches snapshot storage)
    let home_dir = ctx.get_home_dir()?;
    let normalized = expanded
        .strip_prefix(&home_dir)
        .map_or_else(|_| expanded.clone(), std::path::Path::to_path_buf);

    Ok(normalized)
}

/// Filter for matching file paths - supports both exact paths and glob patterns.
struct PathFilter {
    /// Exact paths for O(1) `HashMap` lookup
    exact_paths: Vec<PathBuf>,
    /// Compiled glob patterns for pattern matching
    patterns: Vec<Pattern>,
    /// Original pattern strings for display purposes
    pattern_strings: Vec<String>,
}

impl PathFilter {
    /// Create a new `PathFilter` from raw path strings.
    /// Exact paths are normalized to home-relative format.
    /// Glob patterns are compiled for efficient matching.
    fn new(ctx: &DotmanContext, path_strs: &[String]) -> Result<Self> {
        let mut exact_paths = Vec::new();
        let mut patterns = Vec::new();
        let mut pattern_strings = Vec::new();

        for path_str in path_strs {
            if is_glob_pattern(path_str) {
                match Pattern::new(path_str) {
                    Ok(pattern) => {
                        patterns.push(pattern);
                        pattern_strings.push(path_str.clone());
                    }
                    Err(_) => {
                        output::warning(&format!("Invalid glob pattern: {path_str}"));
                    }
                }
            } else {
                exact_paths.push(parse_path(ctx, path_str)?);
            }
        }

        Ok(Self {
            exact_paths,
            patterns,
            pattern_strings,
        })
    }

    /// Check if filter is empty (no filtering needed)
    const fn is_empty(&self) -> bool {
        self.exact_paths.is_empty() && self.patterns.is_empty()
    }

    /// Check if any file change in this commit matches the filter.
    /// Returns true if:
    /// - Filter is empty (no filtering), OR
    /// - Any exact path changed, OR
    /// - Any pattern matches a changed file (union behavior)
    fn matches_any_change(&self, snapshot: &Snapshot, prev: Option<&Snapshot>) -> bool {
        // No filtering - include all commits
        if self.is_empty() {
            return true;
        }

        // Check exact paths first (O(1) lookup per path)
        for path in &self.exact_paths {
            let current_hash = snapshot.files.get(path).map(|f| &f.hash);
            let prev_hash = prev.and_then(|p| p.files.get(path).map(|f| &f.hash));
            if current_hash != prev_hash {
                return true;
            }
        }

        // If no patterns, we're done
        if self.patterns.is_empty() {
            return false;
        }

        // Collect changed files for pattern matching
        let changed_files = Self::get_changed_files(snapshot, prev);

        // Git-style matching: * crosses directory separators
        let match_opts = MatchOptions {
            require_literal_separator: false,
            require_literal_leading_dot: false,
            case_sensitive: true,
        };

        // Check if any pattern matches any changed file
        for pattern in &self.patterns {
            for path in &changed_files {
                if pattern.matches_with(&path.to_string_lossy(), match_opts) {
                    return true;
                }
            }
        }

        false
    }

    /// Get set of files that changed between snapshots
    fn get_changed_files(snapshot: &Snapshot, prev: Option<&Snapshot>) -> Vec<PathBuf> {
        let mut changed = Vec::new();

        // Files in current snapshot (added or modified)
        for (path, file) in &snapshot.files {
            let prev_hash = prev.and_then(|p| p.files.get(path).map(|f| &f.hash));
            if prev_hash != Some(&file.hash) {
                changed.push(path.clone());
            }
        }

        // Files deleted (in prev but not current)
        if let Some(p) = prev {
            for path in p.files.keys() {
                if !snapshot.files.contains_key(path) {
                    changed.push(path.clone());
                }
            }
        }

        changed
    }

    /// Format filter for display in "no commits found" message
    fn display(&self) -> String {
        let mut parts = Vec::new();

        for path in &self.exact_paths {
            parts.push(format!("'{}'", path.display()));
        }

        for pattern_str in &self.pattern_strings {
            parts.push(format!("'{pattern_str}'"));
        }

        parts.join(", ")
    }
}

/// Parse ref arguments into resolved commit IDs.
///
/// Uses heuristic for backward compatibility when no explicit `--` separator is used:
/// - If first arg resolves as ref, ONLY the first arg is used as ref (rest are paths)
/// - If first arg doesn't resolve, all args are treated as paths (return empty)
///
/// When `--` separator is used (paths is non-empty), ALL refs are definitive and resolved.
fn parse_refs(refs: &[String], paths: &[String], resolver: &RefResolver) -> Result<Vec<String>> {
    use anyhow::Context;

    // Explicit -- separator used: all refs are definitive
    if !paths.is_empty() {
        return refs
            .iter()
            .map(|r| {
                resolver
                    .resolve(r)
                    .with_context(|| format!("Invalid reference: '{r}'"))
            })
            .collect();
    }

    if refs.is_empty() {
        return Ok(vec![]);
    }

    // Backward compat heuristic: only first arg treated as ref, rest become paths
    Ok(resolver
        .resolve(&refs[0])
        .map_or_else(|_| vec![], |commit_id| vec![commit_id]))
}

/// Parse path arguments into a `PathFilter`, handling both exact paths and glob patterns.
fn parse_paths(
    ctx: &DotmanContext,
    refs: &[String],
    paths: &[String],
    resolver: &RefResolver,
) -> Result<PathFilter> {
    let path_strs: Vec<String> = if !paths.is_empty() {
        // Explicit -- separator used
        paths.to_vec()
    } else if !refs.is_empty() && resolver.resolve(&refs[0]).is_err() {
        // Heuristic: if first "ref" doesn't resolve, all args are paths
        refs.to_vec()
    } else if refs.len() > 1 {
        // Backward compat: args after first ref are paths
        refs[1..].to_vec()
    } else {
        vec![]
    };

    PathFilter::new(ctx, &path_strs)
}

/// Display commit history
///
/// # Errors
///
/// Returns an error if:
/// - The repository is not initialized
/// - The specified target reference cannot be resolved
/// - Failed to load snapshots
#[allow(clippy::too_many_lines)] // Detailed log formatting requires multiple sections
pub fn execute(
    ctx: &DotmanContext,
    refs: &[String],
    paths: &[String],
    limit: usize,
    oneline: bool,
    all: bool,
) -> Result<()> {
    ctx.check_repo_initialized()?;

    let snapshot_manager =
        SnapshotManager::new(ctx.repo_path.clone(), ctx.config.core.compression_level);

    let snapshots = snapshot_manager.list_snapshots()?;

    if snapshots.is_empty() {
        output::info("No commits yet");
        return Ok(());
    }

    // Create pager once at the start
    let pager_config = PagerConfig::from_context(ctx, "log");
    let mut pager = Pager::builder().config(pager_config).build()?;
    let writer = pager.writer();

    // Handle --all flag: show all commits including orphaned ones
    if all {
        let mut commits_displayed = 0;

        // Load all snapshots and sort by timestamp (most recent first)
        let mut snapshot_data: Vec<_> = snapshots
            .iter()
            .filter_map(|id| {
                snapshot_manager
                    .load_snapshot(id)
                    .ok()
                    .map(|snap| (id.clone(), snap))
            })
            .collect();

        snapshot_data.sort_by_key(|(_, snap)| std::cmp::Reverse(snap.commit.timestamp));

        let display_limit = limit.min(snapshot_data.len());

        for (_, snapshot) in snapshot_data.iter().take(display_limit) {
            display_commit(writer, &snapshot.commit, oneline)?;
            commits_displayed += 1;
        }

        if commits_displayed >= limit && snapshot_data.len() > limit {
            writeln!(
                writer,
                "\n{} (showing {} of {} total commits, use -n to see more)",
                "...".dimmed(),
                commits_displayed,
                snapshot_data.len()
            )?;
        }

        if commits_displayed > 0 {
            pager.finish()?;
        }

        return Ok(());
    }

    // Use the reference resolver to handle HEAD, HEAD~n, branches, and short hashes
    let resolver = RefResolver::new(ctx.repo_path.clone());

    let start_commits = parse_refs(refs, paths, &resolver)?;
    let filter = parse_paths(ctx, refs, paths, &resolver)?;

    let mut commits_displayed = 0;

    let starting_commit_ids: Vec<String> = if start_commits.is_empty() {
        if let Ok(id) = resolver.resolve("HEAD") {
            vec![id]
        } else {
            output::info("No commits yet");
            return Ok(());
        }
    } else {
        start_commits
    };

    // BinaryHeap gives max-heap on (timestamp, commit_id) for chronological traversal
    let mut heap: BinaryHeap<(i64, String)> = BinaryHeap::new();
    let mut visited = HashSet::new();

    for commit_id in &starting_commit_ids {
        if !visited.contains(commit_id)
            && let Ok(snapshot) = snapshot_manager.load_snapshot(commit_id)
        {
            heap.push((snapshot.commit.timestamp, commit_id.clone()));
        }
    }

    while let Some((_, commit_id)) = heap.pop() {
        if commits_displayed >= limit {
            break;
        }

        // Merge commits from multiple starting refs can cause duplicates
        if visited.contains(&commit_id) {
            continue;
        }
        visited.insert(commit_id.clone());

        let Ok(snapshot) = snapshot_manager.load_snapshot(&commit_id) else {
            continue;
        };

        // Load parent snapshot for comparison (to detect changes in this commit)
        let parent_snapshot = snapshot
            .commit
            .parents
            .first()
            .and_then(|pid| snapshot_manager.load_snapshot(pid).ok());

        // Apply file filtering (compare current commit vs its parent)
        if filter.matches_any_change(&snapshot, parent_snapshot.as_ref()) {
            display_commit(writer, &snapshot.commit, oneline)?;
            commits_displayed += 1;
        }

        // Traverse all parents for union of multiple refs
        for parent_id in &snapshot.commit.parents {
            if !visited.contains(parent_id)
                && let Ok(parent_snap) = snapshot_manager.load_snapshot(parent_id)
            {
                heap.push((parent_snap.commit.timestamp, parent_id.clone()));
            }
        }
    }

    if commits_displayed == 0 {
        if filter.is_empty() {
            output::info("No commits yet");
        } else {
            output::info(&format!("No commits found matching {}", filter.display()));
        }
    } else if commits_displayed >= limit {
        // Only show truncation indicator if we hit the display limit
        writeln!(
            writer,
            "\n{} (showing {} commits, use -n to see more)",
            "...".dimmed(),
            commits_displayed
        )?;
    }

    if commits_displayed > 0 {
        pager.finish()?;
    }

    Ok(())
}
