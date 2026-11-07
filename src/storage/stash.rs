use super::{FileEntry, FileStatus};
use crate::utils::serialization;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use zstd::stream::{decode_all, encode_all};

/// Represents a single stash entry containing saved workspace state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StashEntry {
    /// Unique identifier for this stash (timestamp-based)
    pub id: String,
    /// Optional user-provided message describing the stash
    pub message: String,
    /// Unix timestamp when this stash was created
    pub timestamp: i64,
    /// The commit ID this stash was based on
    pub parent_commit: String,
    /// Files that were stashed (path -> file data)
    pub files: HashMap<PathBuf, StashFile>,
    /// State of the index when stash was created
    pub index_state: Vec<FileEntry>,
}

/// Represents a single file in a stash
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StashFile {
    /// Content hash of the file
    pub hash: String,
    /// File permissions/mode
    pub mode: u32,
    /// Status of the file (Added/Modified/Deleted)
    pub status: FileStatus,
    /// Actual file content (for modified/added files)
    pub content: Option<Vec<u8>>,
}

/// Manages stash operations for the repository
pub struct StashManager {
    /// Path to the dotman repository directory
    repo_path: PathBuf,
    /// Zstandard compression level (1-22, higher = better compression but slower)
    compression_level: i32,
}

impl StashManager {
    /// Creates a new `StashManager` instance
    ///
    /// # Arguments
    ///
    /// * `repo_path` - Path to the dotman repository directory (typically `~/.dotman`)
    /// * `compression_level` - Zstandard compression level (1-22, higher = better compression but slower)
    ///
    /// # Returns
    ///
    /// A new `StashManager` configured with the specified repository path and compression level
    #[must_use]
    pub const fn new(repo_path: PathBuf, compression_level: i32) -> Self {
        Self {
            repo_path,
            compression_level,
        }
    }

    /// Get the stash directory path
    fn stash_dir(&self) -> PathBuf {
        self.repo_path.join("stash")
    }

    /// Get the stash entries directory
    fn entries_dir(&self) -> PathBuf {
        self.stash_dir().join("entries")
    }

    /// Get the stash refs directory
    fn refs_dir(&self) -> PathBuf {
        self.stash_dir().join("refs")
    }

    /// Get the path to the stash stack file
    fn stack_file(&self) -> PathBuf {
        self.refs_dir().join("stash")
    }

    /// Initialize stash directories if they don't exist
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Failed to create stash directories
    pub fn init_stash_dirs(&self) -> Result<()> {
        fs::create_dir_all(self.entries_dir())?;
        fs::create_dir_all(self.refs_dir())?;
        Ok(())
    }

    /// Generate a unique stash ID based on timestamp and random suffix
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - System time is before UNIX epoch
    pub fn generate_stash_id(&self) -> Result<String> {
        use rand::Rng;
        use std::time::{SystemTime, UNIX_EPOCH};

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| anyhow::anyhow!("System time error: {e}"))?
            .as_secs();

        // Add some randomness to ensure uniqueness
        let mut rng = rand::rng();
        let random: u32 = rng.random();
        Ok(format!("stash_{timestamp:x}_{random:08x}"))
    }

    /// Save a stash entry to disk
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Failed to create stash directories
    /// - Failed to serialize or compress the stash
    /// - Failed to write stash to disk
    /// - Failed to update the stash stack
    pub fn save_stash(&self, entry: &StashEntry) -> Result<()> {
        self.init_stash_dirs()?;

        let entry_path = self.entries_dir().join(format!("{}.zst", entry.id));

        // Serialize and compress
        let serialized = serialization::serialize(entry)?;
        let compressed = encode_all(&serialized[..], self.compression_level)?;

        // Write to disk
        fs::write(&entry_path, compressed)
            .with_context(|| format!("Failed to write stash entry: {}", entry.id))?;

        self.push_to_stack(&entry.id)?;

        Ok(())
    }

    /// Load a stash entry from disk
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The stash entry does not exist
    /// - Failed to read or decompress the stash file
    /// - Failed to deserialize the stash data
    pub fn load_stash(&self, stash_id: &str) -> Result<StashEntry> {
        let entry_path = self.entries_dir().join(format!("{stash_id}.zst"));

        if !entry_path.exists() {
            return Err(anyhow::anyhow!("Stash entry not found: {stash_id}"));
        }

        // Read and decompress
        let compressed = fs::read(&entry_path)?;
        let decompressed = decode_all(&compressed[..])?;

        // Deserialize
        let entry: StashEntry = serialization::deserialize(&decompressed)
            .with_context(|| format!("Failed to deserialize stash: {stash_id}"))?;

        Ok(entry)
    }

    /// Get the latest stash ID from the stack
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Failed to read the stash stack file
    pub fn get_latest_stash_id(&self) -> Result<Option<String>> {
        let stack_file = self.stack_file();

        if !stack_file.exists() {
            return Ok(None);
        }

        let stack = fs::read_to_string(&stack_file)?;
        let lines: Vec<&str> = stack.lines().collect();

        if lines.is_empty() {
            Ok(None)
        } else {
            Ok(Some(lines[0].to_string()))
        }
    }

    /// Get all stash IDs from the stack
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Failed to read the stash stack file
    pub fn list_stashes(&self) -> Result<Vec<String>> {
        let stack_file = self.stack_file();

        if !stack_file.exists() {
            return Ok(Vec::new());
        }

        let stack = fs::read_to_string(&stack_file)?;
        let stashes: Vec<String> = stack
            .lines()
            .map(std::string::ToString::to_string)
            .collect();

        Ok(stashes)
    }

    /// Push a stash ID to the top of the stack
    fn push_to_stack(&self, stash_id: &str) -> Result<()> {
        let stack_file = self.stack_file();

        // Read existing stack
        let stack = if stack_file.exists() {
            fs::read_to_string(&stack_file)?
        } else {
            String::new()
        };

        // Prepend new stash ID
        let new_stack = if stack.is_empty() {
            stash_id.to_string()
        } else {
            format!("{stash_id}\n{stack}")
        };

        // Write back
        fs::write(&stack_file, new_stack)?;

        Ok(())
    }

    /// Pop the latest stash ID from the stack
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Failed to read or write the stash stack file
    pub fn pop_from_stack(&self) -> Result<Option<String>> {
        let stack_file = self.stack_file();

        if !stack_file.exists() {
            return Ok(None);
        }

        let stack = fs::read_to_string(&stack_file)?;
        let mut lines: Vec<&str> = stack.lines().collect();

        if lines.is_empty() {
            return Ok(None);
        }

        // Remove first line (latest stash)
        let stash_id = lines.remove(0).to_string();

        // Write back remaining stack
        if lines.is_empty() {
            // Remove file if stack is empty
            fs::remove_file(&stack_file)?;
        } else {
            let new_stack = lines.join("\n");
            fs::write(&stack_file, new_stack)?;
        }

        Ok(Some(stash_id))
    }

    /// Remove a specific stash from the stack
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Failed to read or write the stash stack file
    pub fn remove_from_stack(&self, stash_id: &str) -> Result<bool> {
        let stack_file = self.stack_file();

        if !stack_file.exists() {
            return Ok(false);
        }

        let stack = fs::read_to_string(&stack_file)?;

        // Filter out the stash ID
        let new_lines: Vec<&str> = stack.lines().filter(|&line| line != stash_id).collect();

        if new_lines.is_empty() {
            // Remove file if stack is empty
            fs::remove_file(&stack_file)?;
        } else {
            let new_stack = new_lines.join("\n");
            fs::write(&stack_file, new_stack)?;
        }

        Ok(true)
    }

    /// Delete a stash entry from disk
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Failed to delete the stash file
    /// - Failed to update the stash stack
    pub fn delete_stash(&self, stash_id: &str) -> Result<()> {
        let entry_path = self.entries_dir().join(format!("{stash_id}.zst"));

        if entry_path.exists() {
            fs::remove_file(&entry_path)?;
        }

        // Also remove from stack
        self.remove_from_stack(stash_id)?;

        Ok(())
    }

    /// Clear all stashes
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Failed to remove stash directories
    pub fn clear_all_stashes(&self) -> Result<()> {
        if self.entries_dir().exists() {
            for entry in fs::read_dir(self.entries_dir())? {
                let entry = entry?;
                if entry.path().extension().and_then(|s| s.to_str()) == Some("zst") {
                    fs::remove_file(entry.path())?;
                }
            }
        }

        // Remove stack file
        let stack_file = self.stack_file();
        if stack_file.exists() {
            fs::remove_file(&stack_file)?;
        }

        Ok(())
    }

    /// Check if there are any stashes
    #[must_use]
    pub fn has_stashes(&self) -> bool {
        self.stack_file().exists()
    }
}
