//! Utility functions and helpers.
//!
//! This module provides a collection of utility functions used throughout dotman:
//!
//! - Path manipulation (tilde expansion, relative paths)
//! - File traversal with filtering
//! - Ignore pattern matching
//! - File size formatting
//! - Timestamp utilities
//! - User information retrieval
//!
//! # Submodules
//!
//! - [`commit`](crate::utils::commit): Commit-related utilities
//! - [`compress`](crate::utils::compress): Compression helpers
//! - [`formatters`](crate::utils::formatters): Output formatting
//! - [`pager`](crate::utils::pager): Pager integration
//! - [`paths`](crate::utils::paths): Path manipulation
//! - [`permissions`](crate::utils::permissions): Cross-platform file permissions
//! - [`serialization`](crate::utils::serialization): Binary serialization
//! - [`thread_pool`](crate::utils::thread_pool): Thread pool configuration
//!
//! # Examples
//!
//! ```
//! use dotman::utils::{expand_tilde, format_size};
//!
//! # fn main() -> anyhow::Result<()> {
//! // Expand tilde in paths
//! let path = expand_tilde("~/.bashrc")?;
//!
//! // Format file sizes
//! let size_str = format_size(1024 * 1024); // "1.00 MB"
//! # Ok(())
//! # }
//! ```

/// Commit ID generation and utilities
pub mod commit;
/// Compression utilities (Zstandard)
pub mod compress;
/// Output formatting and colorization
pub mod formatters;
/// Pager integration for long output
pub mod pager;
/// Path manipulation and resolution utilities
pub mod paths;
/// Unix permission handling
pub mod permissions;
/// Binary serialization utilities
pub mod serialization;
/// Thread pool configuration for parallel operations
pub mod thread_pool;

use anyhow::Result;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Expands a path starting with `~` to the user's home directory.
///
/// # Errors
///
/// Returns an error if the path is empty.
pub fn expand_tilde(path: &str) -> Result<PathBuf> {
    if path.is_empty() {
        anyhow::bail!("Path cannot be empty");
    }
    if path.starts_with("~/")
        && let Some(home) = dirs::home_dir()
    {
        return Ok(home.join(&path[2..]));
    }
    Ok(PathBuf::from(path))
}

/// Make `path` relative to `base` if possible, otherwise return `path` as is.
///
/// This is permissive for local operations (add/commit/checkout) but note that
/// files outside HOME directory cannot be synchronized via git remotes.
///
/// # Errors
/// Returns Ok with the relative path if possible, or the original path otherwise.
pub fn make_relative(path: &Path, base: &Path) -> Result<PathBuf> {
    path.strip_prefix(base)
        .map(Path::to_path_buf)
        .or_else(|_| Ok(path.to_path_buf()))
}

/// Walks a directory and returns all file paths that pass the provided filter function.
///
/// # Errors
/// Returns an error if any entry cannot be accessed.
pub fn walk_dir_filtered<F>(dir: &Path, filter: F, follow_symlinks: bool) -> Result<Vec<PathBuf>>
where
    F: Fn(&Path) -> bool,
{
    let mut paths = Vec::new();

    for entry in WalkDir::new(dir)
        .follow_links(follow_symlinks)
        .into_iter()
        .filter_entry(|e| filter(e.path()))
    {
        let entry = entry?;
        if entry.file_type().is_file() {
            paths.push(entry.path().to_path_buf());
        }
    }

    Ok(paths)
}

/// Determines if a given path should be ignored based on provided patterns.
#[must_use]
pub fn should_ignore(path: &Path, patterns: &[String]) -> bool {
    let path_str = path.to_string_lossy();

    for pattern in patterns {
        // Handle directory patterns (ending with /)
        if pattern.ends_with('/') {
            let dir_name = &pattern[..pattern.len() - 1];
            // or if the path contains this directory
            if path.components().any(|c| c.as_os_str() == dir_name) {
                return true;
            }
            // Also check if path starts with or contains the directory pattern
            if path_str.contains(&format!("/{dir_name}/"))
                || path_str.starts_with(&format!("{dir_name}/"))
                || path_str == dir_name
            {
                return true;
            }
        } else if pattern.starts_with('*') && pattern.ends_with('*') {
            // Contains pattern
            let search = &pattern[1..pattern.len() - 1];
            if path_str.contains(search) {
                return true;
            }
        } else if let Some(suffix) = pattern.strip_prefix('*') {
            // Ends with pattern
            if path_str.ends_with(suffix) {
                return true;
            }
        } else if pattern.ends_with('*') {
            // Starts with pattern
            let prefix = &pattern[..pattern.len() - 1];
            if path_str.starts_with(prefix) {
                return true;
            }
        } else {
            // Exact match or path component match
            if path_str == pattern.as_str()
                || path.components().any(|c| c.as_os_str() == pattern.as_str())
            {
                return true;
            }
        }
    }

    false
}

/// Formats a file size in bytes into a human-readable string with appropriate units.
#[must_use]
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
pub fn format_size(size: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = size as f64;
    let mut unit_index = 0;

    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    if unit_index == 0 {
        format!("{} {}", size.round() as u64, UNITS[unit_index])
    } else {
        format!("{:.2} {}", size, UNITS[unit_index])
    }
}

/// Returns the current timestamp as seconds since the Unix epoch.
#[must_use]
pub fn get_current_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_else(|_| {
            eprintln!("Warning: System time before Unix epoch, using 0");
            std::time::Duration::from_secs(0)
        })
        .as_secs()
        .try_into()
        .unwrap_or_else(|_| {
            eprintln!("Warning: Timestamp too large for i64, using i64::MAX");
            i64::MAX
        })
}

/// Get current timestamp with nanosecond precision for unique commit IDs
#[must_use]
pub fn get_precise_timestamp() -> (i64, u32) {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or((0, 0), |d| {
            (
                i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
                d.subsec_nanos(),
            )
        })
}

/// Retrieves the current system username, falling back to "unknown" if not found.
#[must_use]
pub fn get_current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Constructs the current user string based on the provided configuration.
#[must_use]
pub fn get_user_from_config(config: &crate::config::Config) -> String {
    match (&config.user.name, &config.user.email) {
        (Some(name), Some(email)) => {
            // Format as "Name <email>" like git does
            format!("{name} <{email}>")
        }
        (Some(name), None) => {
            // Only name is set
            name.clone()
        }
        (None, Some(email)) => {
            // Only email is set, use system username with email
            let username = get_current_user();
            format!("{username} <{email}>")
        }
        (None, None) => {
            // Neither is set, fall back to system username
            get_current_user()
        }
    }
}
