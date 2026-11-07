//! Configuration parsing, validation, and management.
//!
//! This module provides the configuration system for dotman, including:
//!
//! - TOML-based configuration files
//! - Multiple remote repository support
//! - Branch tracking configuration
//! - Performance tuning options
//! - File tracking and ignore patterns
//! - User identity configuration
//! - Security and path validation settings
//!
//! # Configuration File Location
//!
//! Default: `~/.config/dotman/config`
//! Override with: `DOTMAN_CONFIG_PATH` environment variable
//!
//! # Configuration Structure
//!
//! ```toml
//! [core]
//! compression = "zstd"
//! compression_level = 3
//!
//! [user]
//! name = "Your Name"
//! email = "you@example.com"
//!
//! [performance]
//! parallel_threads = 8
//! mmap_threshold = 1048576
//!
//! [tracking]
//! ignore_patterns = [".git", "*.swp"]
//! follow_symlinks = false
//! preserve_permissions = true
//!
//! [security]
//! allowed_directories = ["~"]
//! enforce_path_validation = true
//! strip_dangerous_permissions = true
//! ```
//!
//! # Examples
//!
//! ```no_run
//! use dotman::config::Config;
//! use std::path::PathBuf;
//!
//! # fn main() -> anyhow::Result<()> {
//! // Load config from default location
//! let config = Config::load(&PathBuf::from("~/.config/dotman/config"))?;
//!
//! // Modify and save
//! let mut config = Config::default();
//! config.set("user.name", "Alice".to_string())?;
//! config.save(&PathBuf::from("config.toml"))?;
//! # Ok(())
//! # }
//! ```

/// Configuration file parsing utilities.
///
/// This module provides functionality for parsing TOML configuration files
/// and converting them into strongly-typed configuration structures.
pub mod parser;

/// Configuration validation utilities.
///
/// This module provides validation logic for configuration values,
/// ensuring they meet the required constraints and are semantically correct.
pub mod validator;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Main configuration structure for dotman.
///
/// This structure contains all configuration sections including core settings,
/// remote repositories, performance tuning, file tracking, and user identity.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// Core dotman settings (compression, repository path, etc.).
    #[serde(default)]
    pub core: CoreConfig,

    /// Multiple named remotes (like git's origin, upstream, etc.).
    #[serde(default)]
    pub remotes: HashMap<String, RemoteConfig>,

    /// Branch tracking configuration.
    #[serde(default)]
    pub branches: BranchConfig,

    /// Performance optimization settings.
    #[serde(default)]
    pub performance: PerformanceConfig,

    /// File tracking and ignore patterns.
    #[serde(default)]
    pub tracking: TrackingConfig,

    /// User configuration (name and email for commits).
    #[serde(default)]
    pub user: UserConfig,

    /// Pager configuration for command output.
    #[serde(default)]
    pub pager: Option<PagerConfig>,

    /// Diff command configuration.
    #[serde(default)]
    pub diff: DiffConfig,

    /// Security and path validation settings.
    #[serde(default)]
    pub security: SecurityConfig,
}

/// Core dotman configuration settings.
///
/// Contains fundamental settings like repository path, compression
/// algorithm and level, and pager configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreConfig {
    /// Path to the dotman repository directory. Default: `~/.dotman`
    #[serde(default = "default_repo_path")]
    pub repo_path: PathBuf,

    /// Compression algorithm for snapshots. Default: Zstd
    #[serde(default = "default_compression")]
    pub compression: CompressionType,

    /// Compression level (1-22 for Zstd). Default: 3
    #[serde(default = "default_compression_level")]
    pub compression_level: i32,

    /// Optional pager command for displaying output.
    #[serde(default)]
    pub pager: Option<String>,
}

/// Compression algorithm type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompressionType {
    /// Zstandard compression (high speed, good ratio)
    Zstd,
    /// No compression
    None,
}

/// Remote repository configuration.
///
/// Defines a remote repository connection, similar to git remotes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Type of remote (Git or None).
    pub remote_type: RemoteType,

    /// URL of the remote repository (if applicable).
    pub url: Option<String>,
}

/// Remote repository type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteType {
    /// Git-based remote repository
    Git,
    /// No remote (local only)
    None,
}

/// Performance optimization configuration.
///
/// Controls parallel processing, memory-mapped I/O, and hard link usage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceConfig {
    /// Number of parallel threads for operations. Default: min(CPU count, 8)
    #[serde(default = "default_parallel_threads")]
    pub parallel_threads: usize,

    /// Memory-map threshold in bytes (files larger use mmap). Default: 1 MB
    #[serde(default = "default_mmap_threshold")]
    pub mmap_threshold: usize,

    /// Whether to use hard links when possible. Default: true
    #[serde(default = "default_use_hard_links")]
    pub use_hard_links: bool,
    // Future: Add cache_size field for LRU object caching (see issue #1)
    // pub cache_size: usize,
}

/// File tracking and ignore configuration.
///
/// Controls which files are tracked and how they are processed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackingConfig {
    /// Patterns to ignore when adding files (glob-style).
    pub ignore_patterns: Vec<String>,

    /// Whether to follow symbolic links during directory traversal.
    pub follow_symlinks: bool,

    /// Whether to preserve file permissions in snapshots.
    pub preserve_permissions: bool,

    /// Warn when adding files larger than this size (in bytes). Default: 100 MB
    #[serde(default = "default_large_file_threshold")]
    pub large_file_threshold: u64,

    /// Enable warnings and confirmation prompt when adding files larger than threshold.
    /// Default: true (warnings enabled for safety)
    #[serde(default = "default_warn_large_files")]
    pub warn_large_files: bool,
}

/// Branch tracking configuration.
///
/// Maps branch names to their upstream remote tracking information.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BranchConfig {
    /// Branch tracking information: `branch_name` -> [`BranchTracking`]
    #[serde(default)]
    pub tracking: HashMap<String, BranchTracking>,
}

/// Upstream tracking information for a branch.
///
/// Associates a local branch with a remote and remote branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchTracking {
    /// Name of the remote (e.g., "origin")
    pub remote: String,

    /// Name of the branch on the remote
    pub branch: String,
}

/// User identity configuration.
///
/// Used for commit authorship information.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserConfig {
    /// User's full name for commits.
    pub name: Option<String>,

    /// User's email address for commits.
    pub email: Option<String>,
}

/// Pager configuration for controlling output pagination.
///
/// Allows per-command control of pager behavior and custom pager commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PagerConfig {
    /// Enable/disable pager for diff command.
    #[serde(default)]
    pub diff: Option<bool>,

    /// Enable/disable pager for log command.
    #[serde(default)]
    pub log: Option<bool>,

    /// Enable/disable pager for show command.
    #[serde(default)]
    pub show: Option<bool>,

    /// Enable/disable pager for branch command.
    #[serde(default)]
    pub branch: Option<bool>,

    /// Enable/disable pager for status command.
    #[serde(default)]
    pub status: Option<bool>,

    /// Custom pager command for diff (overrides core.pager).
    #[serde(default)]
    pub diff_pager: Option<String>,

    /// Custom pager command for log (overrides core.pager).
    #[serde(default)]
    pub log_pager: Option<String>,

    /// Auto-detect if paging is needed based on output size.
    #[serde(default)]
    pub auto: Option<bool>,

    /// Minimum number of lines before paging is used.
    #[serde(default)]
    pub min_lines: Option<usize>,
}

/// Diff command configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffConfig {
    /// Show unified diff by default (true) or file status (false).
    #[serde(default = "default_unified")]
    pub unified: bool,

    /// Number of context lines in unified diffs.
    #[serde(default = "default_context")]
    pub context: usize,

    /// Diff algorithm to use.
    #[serde(default)]
    pub algorithm: DiffAlgorithm,

    /// Colorize diff output.
    #[serde(default = "default_true")]
    pub color: bool,
}

/// Diff algorithm selection.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DiffAlgorithm {
    /// Myers diff algorithm (default, fast).
    #[default]
    Myers,
    /// Patience diff algorithm (better for large changes).
    Patience,
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            unified: true, // Default to unified diff (Git-like)
            context: 3,    // 3 lines of context
            algorithm: DiffAlgorithm::Myers,
            color: true, // Colorize by default
        }
    }
}

/// Security and path validation configuration.
///
/// Controls security features including path validation and permission sanitization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Allowed base directories for file tracking.
    ///
    /// Default: `[$HOME]` only. Paths outside these directories will be rejected
    /// when `enforce_path_validation` is enabled.
    #[serde(default = "default_allowed_directories")]
    pub allowed_directories: Vec<PathBuf>,

    /// Enforce strict path validation (reject paths outside allowed directories).
    ///
    /// Default: `true`. When enabled, dotman will reject attempts to track files
    /// outside the allowed directories, preventing path traversal attacks.
    #[serde(default = "default_enforce_path_validation")]
    pub enforce_path_validation: bool,

    /// Strip dangerous permission bits (setuid/setgid/sticky).
    ///
    /// Default: `true`. When enabled, dangerous permission bits (0o4000, 0o2000, 0o1000)
    /// are automatically stripped when storing files, preventing privilege escalation
    /// attacks when files are restored.
    #[serde(default = "default_strip_dangerous_permissions")]
    pub strip_dangerous_permissions: bool,

    /// Maximum allowed file mode (for manual override).
    ///
    /// Default: `0o777` (rwxrwxrwx). Files with permission bits exceeding this mask
    /// will have the excess bits stripped.
    #[serde(default = "default_max_file_mode")]
    pub max_file_mode: u32,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            allowed_directories: default_allowed_directories(),
            enforce_path_validation: default_enforce_path_validation(),
            strip_dangerous_permissions: default_strip_dangerous_permissions(),
            max_file_mode: default_max_file_mode(),
        }
    }
}

/// Default value for unified diff format. Returns `true` to enable unified diffs by default.
const fn default_unified() -> bool {
    true
}

/// Default number of context lines for diffs. Returns 3 lines of context.
const fn default_context() -> usize {
    3
}

/// Default boolean value helper for serde. Returns `true` for boolean configuration options.
const fn default_true() -> bool {
    true
}

impl Default for CoreConfig {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        Self {
            repo_path: home.join(".dotman"),
            compression: CompressionType::Zstd,
            compression_level: 3,
            pager: None,
        }
    }
}

impl Default for RemoteConfig {
    fn default() -> Self {
        Self {
            remote_type: RemoteType::None,
            url: None,
        }
    }
}

impl Default for PerformanceConfig {
    fn default() -> Self {
        let cpu_count = num_cpus::get();
        Self {
            parallel_threads: cpu_count.min(8),
            mmap_threshold: 1_048_576, // 1MB
            use_hard_links: true,
        }
    }
}

impl Default for TrackingConfig {
    fn default() -> Self {
        Self {
            ignore_patterns: vec![
                ".git".to_string(),
                "*.swp".to_string(),
                "*.tmp".to_string(),
                "node_modules".to_string(),
                "__pycache__".to_string(),
            ],
            follow_symlinks: false,
            preserve_permissions: true,
            large_file_threshold: default_large_file_threshold(),
            warn_large_files: default_warn_large_files(),
        }
    }
}

impl Config {
    /// Get branch tracking information for a branch
    #[must_use]
    pub fn get_branch_tracking(&self, branch: &str) -> Option<&BranchTracking> {
        self.branches.tracking.get(branch)
    }

    /// Set branch tracking information
    pub fn set_branch_tracking(&mut self, branch: String, tracking: BranchTracking) {
        self.branches.tracking.insert(branch, tracking);
    }

    /// Remove branch tracking information
    pub fn remove_branch_tracking(&mut self, branch: &str) -> Option<BranchTracking> {
        self.branches.tracking.remove(branch)
    }

    /// Load configuration from a file
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Cannot create parent directories
    /// - Cannot read or parse the configuration file
    /// - Configuration file contains invalid TOML
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            // Create default config if it doesn't exist
            let config = Self::default();
            config.save(path)?;
            return Ok(config);
        }

        // Use our fast parser for loading
        parser::parse_config_file(path)
    }

    /// Save configuration to a file
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Cannot create parent directories
    /// - Cannot write to the file
    /// - TOML serialization fails
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let toml_str = toml::to_string_pretty(self)?;
        let mut file = std::fs::File::create(path)?;
        file.write_all(toml_str.as_bytes())?;
        Ok(())
    }

    /// Get a remote by name
    #[must_use]
    pub fn get_remote(&self, name: &str) -> Option<&RemoteConfig> {
        self.remotes.get(name)
    }

    /// Add or update a remote
    pub fn set_remote(&mut self, name: String, remote: RemoteConfig) {
        self.remotes.insert(name, remote);
    }

    /// Remove a remote
    pub fn remove_remote(&mut self, name: &str) -> Option<RemoteConfig> {
        self.remotes.remove(name)
    }

    /// Get a configuration value by key
    #[must_use]
    pub fn get(&self, key: &str) -> Option<String> {
        let parts: Vec<&str> = key.split('.').collect();
        if parts.len() != 2 {
            return None;
        }

        match (parts[0], parts[1]) {
            ("user", "name") => self.user.name.clone(),
            ("user", "email") => self.user.email.clone(),
            ("core", "compression") => Some(format!("{:?}", self.core.compression).to_lowercase()),
            ("core", "compression_level") => Some(self.core.compression_level.to_string()),
            ("core", "pager") => self.core.pager.clone(),
            ("performance", "parallel_threads") => {
                Some(self.performance.parallel_threads.to_string())
            }
            ("performance", "mmap_threshold") => Some(self.performance.mmap_threshold.to_string()),
            ("performance", "use_hard_links") => Some(self.performance.use_hard_links.to_string()),
            ("tracking", "follow_symlinks") => Some(self.tracking.follow_symlinks.to_string()),
            ("tracking", "preserve_permissions") => {
                Some(self.tracking.preserve_permissions.to_string())
            }
            ("tracking", "warn_large_files") => Some(self.tracking.warn_large_files.to_string()),
            _ => None,
        }
    }

    /// Set a configuration value by key
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The key format is invalid (must be section.key)
    /// - The key is unknown
    /// - The value is invalid for the key (e.g., invalid email)
    pub fn set(&mut self, key: &str, value: String) -> Result<()> {
        let parts: Vec<&str> = key.split('.').collect();
        if parts.len() != 2 {
            return Err(anyhow::anyhow!("Invalid configuration key: {key}"));
        }

        match (parts[0], parts[1]) {
            ("user", "name") => self.user.name = Some(value),
            ("user", "email") => {
                // Basic email validation
                if !value.contains('@') {
                    return Err(anyhow::anyhow!("Invalid email address: {value}"));
                }
                self.user.email = Some(value);
            }
            ("core", "compression_level") => {
                let level: i32 = value
                    .parse()
                    .with_context(|| format!("Invalid compression level: {value}"))?;
                if !(1..=22).contains(&level) {
                    return Err(anyhow::anyhow!(
                        "Compression level must be between 1 and 22"
                    ));
                }
                self.core.compression_level = level;
            }
            ("core", "pager") => self.core.pager = Some(value),
            ("performance", "parallel_threads") => {
                self.performance.parallel_threads = value
                    .parse()
                    .with_context(|| format!("Invalid number: {value}"))?;
            }
            ("performance", "mmap_threshold") => {
                self.performance.mmap_threshold = value
                    .parse()
                    .with_context(|| format!("Invalid number: {value}"))?;
            }
            ("performance", "use_hard_links") => {
                self.performance.use_hard_links = value
                    .parse()
                    .with_context(|| format!("Invalid boolean: {value}"))?;
            }
            ("tracking", "follow_symlinks") => {
                self.tracking.follow_symlinks = value
                    .parse()
                    .with_context(|| format!("Invalid boolean: {value}"))?;
            }
            ("tracking", "preserve_permissions") => {
                self.tracking.preserve_permissions = value
                    .parse()
                    .with_context(|| format!("Invalid boolean: {value}"))?;
            }
            ("tracking", "warn_large_files") => {
                self.tracking.warn_large_files = value
                    .parse()
                    .with_context(|| format!("Invalid boolean: {value}"))?;
            }
            _ => return Err(anyhow::anyhow!("Unknown configuration key: {key}")),
        }
        Ok(())
    }

    /// Unset a configuration value by key
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The key format is invalid (must be section.key)
    /// - The key is unknown or cannot be unset
    pub fn unset(&mut self, key: &str) -> Result<()> {
        let parts: Vec<&str> = key.split('.').collect();
        if parts.len() != 2 {
            return Err(anyhow::anyhow!("Invalid configuration key: {key}"));
        }

        match (parts[0], parts[1]) {
            ("user", "name") => self.user.name = None,
            ("user", "email") => self.user.email = None,
            ("core", "pager") => self.core.pager = None,
            _ => return Err(anyhow::anyhow!("Cannot unset configuration key: {key}")),
        }
        Ok(())
    }
}

/// Cached number of available CPUs/threads on the system.
///
/// This static is lazily initialized on first access and caches the result
/// of querying the system's available parallelism. Falls back to 1 if the
/// query fails.
static NUM_CPUS: std::sync::LazyLock<usize> = std::sync::LazyLock::new(|| {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
});

/// Internal module for CPU count queries.
///
/// Provides a simple interface to query the cached number of available CPUs
/// without directly exposing the static.
mod num_cpus {
    use super::NUM_CPUS;

    /// Returns the number of available CPUs/threads on the system.
    ///
    /// # Returns
    ///
    /// The number of available CPUs, determined at first call and cached.
    /// Returns 1 if the system query fails.
    pub fn get() -> usize {
        *NUM_CPUS
    }
}

/// Returns the default repository path for dotman.
///
/// This function is used by serde as the default value provider for the
/// repository path configuration field.
///
/// # Returns
///
/// A `PathBuf` pointing to `~/.dotman`, or `/tmp/.dotman` if the home
/// directory cannot be determined.
fn default_repo_path() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".dotman")
}

/// Returns the default compression type.
///
/// This function is used by serde as the default value provider for the
/// compression type configuration field.
///
/// # Returns
///
/// `CompressionType::Zstd` - Zstandard compression as the default algorithm.
const fn default_compression() -> CompressionType {
    CompressionType::Zstd
}

/// Returns the default compression level.
///
/// This function is used by serde as the default value provider for the
/// compression level configuration field.
///
/// # Returns
///
/// `3` - A balanced compression level providing good compression ratio
/// with reasonable performance.
const fn default_compression_level() -> i32 {
    3
}

/// Returns the default number of parallel threads.
///
/// This function is used by serde as the default value provider for the
/// parallel threads configuration field. It caps the thread count at 8
/// to prevent excessive resource usage even on high-core-count systems.
///
/// # Returns
///
/// The minimum of the system's available CPU count and 8.
fn default_parallel_threads() -> usize {
    num_cpus::get().min(8)
}

/// Returns the default memory-mapped I/O threshold.
///
/// This function is used by serde as the default value provider for the
/// mmap threshold configuration field. Files larger than this threshold
/// will use memory-mapped I/O for better performance.
///
/// # Returns
///
/// `1_048_576` (1 MB) - Files larger than 1 MB will use mmap.
const fn default_mmap_threshold() -> usize {
    1_048_576 // 1MB
}

/// Returns the default setting for using hard links.
///
/// This function is used by serde as the default value provider for the
/// `use_hard_links` configuration field.
///
/// # Returns
///
/// `true` - Hard links are enabled by default to save disk space.
const fn default_use_hard_links() -> bool {
    true
}

/// Returns the default large file threshold.
///
/// This function is used by serde as the default value provider for the
/// large file threshold configuration field. Files larger than this size
/// may be handled differently (e.g., with special warnings or processing).
///
/// # Returns
///
/// `104_857_600` (100 MB) - Files larger than 100 MB are considered large.
const fn default_large_file_threshold() -> u64 {
    100 * 1024 * 1024 // 100 MB
}

/// Returns the default setting for large file warnings.
///
/// This function is used by serde as the default value provider for the
/// `warn_large_files` configuration field.
///
/// # Returns
///
/// `true` - Large file warnings are enabled by default for safety.
const fn default_warn_large_files() -> bool {
    true
}

/// Returns the default allowed directories for path validation.
///
/// This function is used by serde as the default value provider for the
/// allowed directories configuration field.
///
/// # Returns
///
/// A vector containing only the user's home directory. If the home directory
/// cannot be determined, falls back to `/home` as a safe default.
fn default_allowed_directories() -> Vec<PathBuf> {
    vec![dirs::home_dir().unwrap_or_else(|| PathBuf::from("/home"))]
}

/// Returns the default setting for path validation enforcement.
///
/// This function is used by serde as the default value provider for the
/// `enforce_path_validation` configuration field.
///
/// # Returns
///
/// `true` - Path validation is enabled by default for security.
const fn default_enforce_path_validation() -> bool {
    true
}

/// Returns the default setting for stripping dangerous permission bits.
///
/// This function is used by serde as the default value provider for the
/// `strip_dangerous_permissions` configuration field.
///
/// # Returns
///
/// `true` - Dangerous permission bits are stripped by default for security.
const fn default_strip_dangerous_permissions() -> bool {
    true
}

/// Returns the default maximum file mode.
///
/// This function is used by serde as the default value provider for the
/// `max_file_mode` configuration field.
///
/// # Returns
///
/// `0o777` - Only standard rwxrwxrwx permissions are allowed by default.
const fn default_max_file_mode() -> u32 {
    0o777
}
