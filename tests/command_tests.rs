#![allow(clippy::too_many_lines)]
#![allow(clippy::indexing_slicing)] // Safe in test environment

use anyhow::Result;
use dotman::commands::context::CommandContext;
use dotman::{DotmanContext, commands};
use serial_test::serial;
use std::fs;
use tempfile::TempDir;

// Unix-specific imports for permission and symlink tests
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

mod add_command_tests {
    use super::*;

    pub fn setup_test_repo() -> Result<(TempDir, DotmanContext)> {
        let temp_dir = TempDir::new()?;
        let repo_path = temp_dir.path().join(".dotman");
        let config_path = temp_dir.path().join(".config/dotman/config");

        // Create config directory
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write test config with temp directory in allowed_directories
        let config_content = format!(
            r#"[security]
allowed_directories = ["{}"]
enforce_path_validation = true
strip_dangerous_permissions = true
"#,
            temp_dir.path().display()
        );
        fs::write(&config_path, config_content)?;

        let ctx = DotmanContext::new_explicit(repo_path, config_path)?;
        ctx.ensure_repo_exists()?;

        // Initialize the repository properly
        let index = dotman::storage::index::Index::new();
        let index_path = ctx.repo_path.join("index.bin");
        index.save(&index_path)?;

        // Initialize refs structure (HEAD, branches)
        let ref_manager = dotman::refs::RefManager::new(ctx.repo_path.clone());
        ref_manager.init()?;

        Ok((temp_dir, ctx))
    }

    #[test]
    fn test_add_single_file() -> Result<()> {
        let (temp_dir, ctx) = setup_test_repo()?;

        // Create a test file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "test content")?;

        // Add the file
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;

        // Check that it's staged
        let index = CommandContext::load_concurrent_index(&ctx)?;
        let staged = index.staged_entries();
        assert_eq!(staged.len(), 1);

        Ok(())
    }

    #[test]
    fn test_add_directory_recursive() -> Result<()> {
        let (temp_dir, ctx) = setup_test_repo()?;

        // Create a directory with nested files
        let test_dir = temp_dir.path().join("test_dir");
        fs::create_dir_all(&test_dir)?;
        fs::write(test_dir.join("file1.txt"), "content 1")?;
        fs::write(test_dir.join("file2.txt"), "content 2")?;

        let nested_dir = test_dir.join("nested");
        fs::create_dir_all(&nested_dir)?;
        fs::write(nested_dir.join("file3.txt"), "content 3")?;

        // Add the directory
        commands::add::execute(&ctx, &[test_dir.to_string_lossy().into()], false, false)?;

        // Check that all files are staged
        let index = CommandContext::load_concurrent_index(&ctx)?;
        let staged = index.staged_entries();
        assert_eq!(staged.len(), 3);

        Ok(())
    }

    #[test]
    fn test_add_nonexistent_file_without_force() -> Result<()> {
        let (_temp_dir, ctx) = setup_test_repo()?;

        let result = commands::add::execute(&ctx, &["/nonexistent/file.txt".into()], false, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));

        Ok(())
    }

    #[test]
    fn test_add_nonexistent_file_with_force() -> Result<()> {
        let (temp_dir, ctx) = setup_test_repo()?;

        // Use a nonexistent file in an existing directory (parent must exist for path validation)
        let nonexistent_path = temp_dir.path().join("nonexistent_file.txt");

        // Should not error with force flag (skips non-existent paths)
        let result = commands::add::execute(
            &ctx,
            &[nonexistent_path.to_string_lossy().into()],
            true,
            false,
        );
        assert!(result.is_ok());

        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn test_add_symlink() -> Result<()> {
        let (temp_dir, ctx) = setup_test_repo()?;

        // Create a file and a symlink to it
        let target = temp_dir.path().join("target.txt");
        fs::write(&target, "target content")?;

        let symlink = temp_dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &symlink)?;

        // Add the symlink
        commands::add::execute(&ctx, &[symlink.to_string_lossy().into()], false, false)?;

        let index = CommandContext::load_concurrent_index(&ctx)?;
        let staged = index.staged_entries();

        // Verify the symlink (or target, depending on config) was added
        assert!(!staged.is_empty(), "Symlink should be added to staging");

        Ok(())
    }

    #[test]
    fn test_add_with_ignore_patterns() -> Result<()> {
        let (temp_dir, ctx) = setup_test_repo()?;

        // Create files that should be ignored
        let test_dir = temp_dir.path().join("test_dir");
        fs::create_dir_all(&test_dir)?;
        fs::write(test_dir.join("file.txt"), "normal file")?;
        fs::write(test_dir.join("file.swp"), "swap file")?;

        let git_dir = test_dir.join(".git");
        fs::create_dir_all(&git_dir)?;
        fs::write(git_dir.join("config"), "git config")?;

        // Add the directory
        commands::add::execute(&ctx, &[test_dir.to_string_lossy().into()], false, false)?;

        // Only non-ignored files should be staged
        let index = CommandContext::load_concurrent_index(&ctx)?;
        let staged = index.staged_entries();

        // Should only have file.txt, not .swp or .git files
        assert_eq!(staged.len(), 1);

        Ok(())
    }

    #[test]
    fn test_add_updates_existing_staged_entry() -> Result<()> {
        let (temp_dir, ctx) = setup_test_repo()?;

        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "initial content")?;

        // Add the file
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;

        // Modify the file
        fs::write(&test_file, "modified content")?;

        // Add it again
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;

        let index = CommandContext::load_concurrent_index(&ctx)?;
        let staged = index.staged_entries();
        assert_eq!(staged.len(), 1);

        // The hash should be different
        let (_, entry) = &staged[0];
        assert_eq!(entry.size, 16); // "modified content" length

        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn test_add_preserves_permissions() -> Result<()> {
        let (temp_dir, ctx) = setup_test_repo()?;

        let test_file = temp_dir.path().join("executable.sh");
        fs::write(&test_file, "#!/bin/bash\necho hello")?;

        // Make it executable
        let mut perms = fs::metadata(&test_file)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&test_file, perms)?;

        // Add the file
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;

        let index = CommandContext::load_concurrent_index(&ctx)?;
        let staged = index.staged_entries();
        assert!(!staged.is_empty(), "File should be staged");
        let (_, entry) = staged
            .first()
            .expect("Should have at least one staged entry");

        // Check that executable bit is preserved
        assert_eq!(
            entry.mode & 0o111,
            0o111,
            "Executable bits should be preserved"
        );

        Ok(())
    }

    #[test]
    fn test_add_unchanged_file_not_marked_modified() -> Result<()> {
        let (temp_dir, ctx) = setup_test_repo()?;

        // Create and commit a file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "original content")?;
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Initial commit", false)?;

        // Clear staged entries after commit
        let mut index = CommandContext::load_concurrent_index(&ctx)?;
        assert!(
            index.staged_entries().is_empty(),
            "Staged entries should be empty after commit"
        );

        // Add the same file again without modifying it
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;

        // File should NOT be staged because it's unchanged
        index = CommandContext::load_concurrent_index(&ctx)?;
        let staged = index.staged_entries();
        assert!(
            staged.is_empty(),
            "Unchanged file should not be staged, but found {} staged file(s)",
            staged.len()
        );

        Ok(())
    }
}

mod commit_command_tests {
    use super::*;
    use dotman::utils::commit::generate_commit_id;

    fn setup_repo_with_staged_files() -> Result<(TempDir, DotmanContext)> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create and stage some files
        let file1 = temp_dir.path().join("file1.txt");
        let file2 = temp_dir.path().join("file2.txt");
        fs::write(&file1, "content 1")?;
        fs::write(&file2, "content 2")?;

        commands::add::execute(
            &ctx,
            &[
                file1.to_string_lossy().into(),
                file2.to_string_lossy().into(),
            ],
            false,
            false,
        )?;

        Ok((temp_dir, ctx))
    }

    #[test]
    fn test_commit_creates_snapshot() -> Result<()> {
        let (_temp_dir, ctx) = setup_repo_with_staged_files()?;

        // Make a commit
        commands::commit::execute(&ctx, "Initial commit", false)?;

        // Check that commit was created
        let commits_dir = ctx.repo_path.join("commits");
        assert!(commits_dir.exists());

        let commits: Vec<_> = fs::read_dir(commits_dir)?.collect();
        assert_eq!(commits.len(), 1);

        // Check that index was cleared
        let index = CommandContext::load_concurrent_index(&ctx)?;
        assert!(index.staged_entries().is_empty());

        Ok(())
    }

    #[test]
    fn test_commit_with_no_staged_files() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Try to commit without staging anything
        let result = commands::commit::execute(&ctx, "Empty commit", false);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No changes staged for commit")
        );

        Ok(())
    }

    #[test]
    fn test_commit_id_generation_deterministic() {
        let tree_hash = "abcd1234567890abcdef1234567890abcdef1234";
        let parents = &["parent1234567890abcdef1234567890abcdef12"];
        let message = "Test commit message";
        let author = "Test User <test@example.com>";
        let timestamp = 1_234_567_890;
        let nanos = 123_456_789;

        let id1 = generate_commit_id(tree_hash, parents, message, author, timestamp, nanos);
        let id2 = generate_commit_id(tree_hash, parents, message, author, timestamp, nanos);

        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 32); // xxHash produces 32-char hex
    }

    #[test]
    fn test_commit_id_unique_for_different_content() {
        let tree_hash = "abcd1234567890abcdef1234567890abcdef1234";
        let author = "Test User <test@example.com>";
        let timestamp = 1_234_567_890;
        let nanos = 123_456_789;

        let id1 = generate_commit_id(tree_hash, &[], "Message 1", author, timestamp, nanos);
        let id2 = generate_commit_id(tree_hash, &[], "Message 2", author, timestamp, nanos);

        assert_ne!(id1, id2);
    }

    #[test]
    fn test_commit_updates_head() -> Result<()> {
        let (_temp_dir, ctx) = setup_repo_with_staged_files()?;

        commands::commit::execute(&ctx, "First commit", false)?;

        // Check that HEAD was updated
        let head_commit = ctx.create_ref_resolver().resolve("HEAD")?;
        assert_ne!(head_commit, dotman::NULL_COMMIT_ID);

        Ok(())
    }

    #[test]
    fn test_commit_with_parent() -> Result<()> {
        let (temp_dir, ctx) = setup_repo_with_staged_files()?;

        // First commit
        commands::commit::execute(&ctx, "First commit", false)?;
        let first_commit = ctx.create_ref_resolver().resolve("HEAD")?;

        // Stage more files
        let file3 = temp_dir.path().join("file3.txt");
        fs::write(&file3, "content 3")?;
        commands::add::execute(&ctx, &[file3.to_string_lossy().into()], false, false)?;

        // Second commit
        commands::commit::execute(&ctx, "Second commit", false)?;
        let second_commit = ctx.create_ref_resolver().resolve("HEAD")?;

        assert_ne!(first_commit, second_commit);

        // Load the second commit and check it has the first as parent
        let snapshot_manager = dotman::storage::snapshots::SnapshotManager::new(ctx.repo_path, 3);
        let snapshot = snapshot_manager.load_snapshot(&second_commit)?;
        assert_eq!(snapshot.commit.parents.first().cloned(), Some(first_commit));

        Ok(())
    }
}

mod status_command_tests {
    use super::*;
    use std::collections::HashSet;

    // Test wrapper for trie-based untracked file scanning
    #[allow(clippy::needless_pass_by_value)]
    fn find_untracked_files_for_test(
        ctx: &DotmanContext,
        index: &dotman::storage::index::Index,
        home_dir: Option<std::path::PathBuf>,
    ) -> Result<Vec<std::path::PathBuf>> {
        let home = home_dir.as_ref().map_or_else(
            || dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Could not find home directory")),
            |h| Ok(h.clone()),
        )?;

        // Build trie and tracked files set
        let mut trie = dotman::scanner::DirTrie::new();
        let mut tracked_files = HashSet::new();

        // Get committed files from HEAD snapshot
        let ref_manager = dotman::refs::RefManager::new(ctx.repo_path.clone());
        if let Some(commit_id) = ref_manager.get_head_commit()?
            && commit_id != "0".repeat(40)
        {
            let snapshot_manager = dotman::storage::snapshots::SnapshotManager::new(
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
                    trie.insert_tracked_file(&abs_path, &home);
                    tracked_files.insert(abs_path);
                }
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

        // Find untracked files using shared scanner
        let untracked_files =
            dotman::scanner::find_untracked_files(&home, &ctx.repo_path, &trie, &tracked_files)?;

        // Filter by ignore patterns
        let untracked: Vec<std::path::PathBuf> = untracked_files
            .into_iter()
            .filter(|file| {
                let relative_path = file.strip_prefix(&home).unwrap_or(file);
                !dotman::utils::should_ignore(relative_path, &ctx.config.tracking.ignore_patterns)
            })
            .collect();

        Ok(untracked)
    }

    #[test]
    fn test_status_clean_repo() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Status should show clean - verify no staged files
        commands::status::execute(&ctx, false, true)?;

        let index = CommandContext::load_concurrent_index(&ctx)?;
        assert!(
            index.staged_entries().is_empty(),
            "Clean repo should have no staged files"
        );
        assert!(
            index.get_deleted_entries().is_empty(),
            "Clean repo should have no deleted files"
        );

        Ok(())
    }

    #[test]
    fn test_status_with_staged_files() -> Result<()> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        let test_file = temp_dir.path().join("staged.txt");
        fs::write(&test_file, "staged content")?;
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;

        // Status should show staged files
        commands::status::execute(&ctx, false, true)?;

        let index = CommandContext::load_concurrent_index(&ctx)?;
        assert_eq!(index.staged_entries().len(), 1);

        Ok(())
    }

    #[test]
    fn test_status_with_modified_files() -> Result<()> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create, add, and commit a file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "initial")?;
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;

        // Verify file is staged before commit
        let index_path = ctx.repo_path.join("index.bin");
        let index_before_commit = dotman::storage::index::Index::load(&index_path)?;
        assert!(
            !index_before_commit.staged_entries.is_empty(),
            "File should be staged before commit"
        );

        commands::commit::execute(&ctx, "Initial commit", false)?;

        // Verify file moved to committed entries after commit
        let index_after_commit = dotman::storage::index::Index::load(&index_path)?;
        assert!(
            index_after_commit.staged_entries.is_empty(),
            "Staged entries should be empty after commit"
        );

        // Verify initial state
        let initial_content = fs::read_to_string(&test_file)?;
        assert_eq!(initial_content, "initial");

        // Modify the file
        fs::write(&test_file, "modified")?;

        // Verify file was actually modified
        let modified_content = fs::read_to_string(&test_file)?;
        assert_eq!(modified_content, "modified");
        assert_ne!(modified_content, initial_content);

        // Status should detect the modification and not crash
        // This is the key test: status should see that the file was modified
        // The fix ensures that hash_file errors don't silently hide modifications
        commands::status::execute(&ctx, false, false)?;

        Ok(())
    }

    #[test]
    fn test_status_with_deleted_files() -> Result<()> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create, add, and commit a file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "content")?;
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file", false)?;

        // Delete the file
        fs::remove_file(&test_file)?;

        // Status should show deleted files
        commands::status::execute(&ctx, false, true)?;

        Ok(())
    }

    #[test]
    fn test_status_shows_untracked_in_tracked_dotfile_dir() -> Result<()> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create .config/kitty directory structure
        let config_dir = temp_dir.path().join(".config/kitty");
        fs::create_dir_all(&config_dir)?;

        // Create and track one file
        let tracked_file = config_dir.join("kitty.conf");
        fs::write(&tracked_file, "initial config")?;
        commands::add::execute(&ctx, &[tracked_file.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add kitty.conf", false)?;

        // Create an untracked file in the same directory
        let untracked_file = config_dir.join("kitty.conf.test");
        fs::write(&untracked_file, "test config")?;

        // Run status with untracked files enabled (default behavior)
        // This should show the untracked file because .config/kitty is a tracked directory
        let untracked_files = find_untracked_files_for_test(
            &ctx,
            &CommandContext::load_index(&ctx)?,
            Some(temp_dir.path().to_path_buf()),
        )?;

        // Verify the untracked file is found
        let untracked_relative: Vec<_> = untracked_files
            .iter()
            .map(|p| p.strip_prefix(temp_dir.path()).unwrap_or(p))
            .collect();

        assert!(
            untracked_relative
                .iter()
                .any(|p| p.ends_with("kitty.conf.test")),
            "Untracked file in tracked dotfile directory should be detected. Found: {untracked_relative:?}"
        );

        Ok(())
    }

    #[test]
    fn test_status_hides_untracked_in_untracked_dotfile_dir() -> Result<()> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create .random_hidden directory (not tracked)
        let hidden_dir = temp_dir.path().join(".random_hidden");
        fs::create_dir_all(&hidden_dir)?;

        // Create a file in the untracked dotfile directory
        let untracked_file = hidden_dir.join("secret.txt");
        fs::write(&untracked_file, "secret content")?;

        // Run status with untracked files enabled
        let untracked_files = find_untracked_files_for_test(
            &ctx,
            &CommandContext::load_index(&ctx)?,
            Some(temp_dir.path().to_path_buf()),
        )?;

        // Verify the file is NOT found (directory is skipped)
        let untracked_relative: Vec<_> = untracked_files
            .iter()
            .map(|p| p.strip_prefix(temp_dir.path()).unwrap_or(p))
            .collect();

        assert!(
            !untracked_relative.iter().any(|p| p.ends_with("secret.txt")),
            "Files in untracked dotfile directories should be hidden. Found: {untracked_relative:?}"
        );

        Ok(())
    }

    #[test]
    fn test_status_shows_untracked_in_nested_tracked_dirs() -> Result<()> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create nested directory structure .config/nvim/lua/plugins
        let plugins_dir = temp_dir.path().join(".config/nvim/lua/plugins");
        fs::create_dir_all(&plugins_dir)?;

        // Track a file deep in the hierarchy
        let tracked_file = plugins_dir.join("init.lua");
        fs::write(&tracked_file, "plugin config")?;
        commands::add::execute(&ctx, &[tracked_file.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add nvim plugin config", false)?;

        // Create untracked files at various levels of the hierarchy
        // NOTE: With focused dotfiles management, only files in the LEAF directory
        // (the directory that directly contains tracked files) should be shown.
        // Files in parent directories (.config/, .config/nvim/, etc.) should NOT be shown.
        let untracked_at_config = temp_dir.path().join(".config/test.txt");
        let untracked_at_nvim = temp_dir.path().join(".config/nvim/test.txt");
        let untracked_at_plugins = plugins_dir.join("new-plugin.lua");

        fs::write(&untracked_at_config, "test1")?;
        fs::write(&untracked_at_nvim, "test2")?;
        fs::write(&untracked_at_plugins, "new plugin")?;

        // Run status
        let untracked_files = find_untracked_files_for_test(
            &ctx,
            &CommandContext::load_index(&ctx)?,
            Some(temp_dir.path().to_path_buf()),
        )?;

        let untracked_relative: Vec<_> = untracked_files
            .iter()
            .map(|p| p.strip_prefix(temp_dir.path()).unwrap_or(p))
            .collect();

        // ONLY the file in the leaf directory should be found
        // Files in parent directories should be ignored for focused dotfiles management
        assert!(
            !untracked_relative
                .iter()
                .any(|p| p.ends_with(".config/test.txt")),
            "Should NOT find untracked file at .config level (parent dir, not leaf)"
        );
        assert!(
            !untracked_relative
                .iter()
                .any(|p| p.to_string_lossy().contains("nvim/test.txt")),
            "Should NOT find untracked file at nvim level (parent dir, not leaf)"
        );
        assert!(
            untracked_relative
                .iter()
                .any(|p| p.ends_with("new-plugin.lua")),
            "Should find untracked file in plugins directory (leaf dir with tracked files)"
        );

        Ok(())
    }

    #[test]
    fn test_status_modified_file_in_dotfile_dir() -> Result<()> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create .config/kitty directory
        let config_dir = temp_dir.path().join(".config/kitty");
        fs::create_dir_all(&config_dir)?;

        // Create, track, and commit a file
        let config_file = config_dir.join("kitty.conf");
        fs::write(&config_file, "initial")?;
        commands::add::execute(&ctx, &[config_file.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Initial kitty config", false)?;

        // Modify the file
        fs::write(&config_file, "modified")?;

        // Create an untracked file in the same directory
        let untracked_file = config_dir.join("new-file.conf");
        fs::write(&untracked_file, "new content")?;

        // Status should show both the modification and the untracked file
        // This is the exact scenario from the bug report
        let untracked_files = find_untracked_files_for_test(
            &ctx,
            &CommandContext::load_index(&ctx)?,
            Some(temp_dir.path().to_path_buf()),
        )?;

        let untracked_relative: Vec<_> = untracked_files
            .iter()
            .map(|p| p.strip_prefix(temp_dir.path()).unwrap_or(p))
            .collect();

        assert!(
            untracked_relative
                .iter()
                .any(|p| p.ends_with("new-file.conf")),
            "Should detect untracked file in same directory as modified file. Found: {untracked_relative:?}"
        );

        Ok(())
    }

    /// Regression test: staged file deleted from disk should show as both
    /// "new file" (staged) and "deleted" (working tree change)
    #[test]
    fn test_status_staged_file_deleted_from_disk() -> Result<()> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create and stage a file (but don't commit)
        let test_file = temp_dir.path().join("staged_file.txt");
        fs::write(&test_file, "content")?;
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;

        // Verify file is staged
        let index = CommandContext::load_concurrent_index(&ctx)?;
        assert_eq!(index.staged_entries().len(), 1, "File should be staged");

        // Delete the file from disk (not via dot rm)
        fs::remove_file(&test_file)?;

        // Status command should run without error and detect the deletion
        // The file is still in staged_entries but no longer exists on disk
        commands::status::execute(&ctx, false, false)?;

        // Verify index still has the staged entry (deletion was not auto-staged)
        let index_after = CommandContext::load_concurrent_index(&ctx)?;
        assert_eq!(
            index_after.staged_entries().len(),
            1,
            "File should still be in staged_entries after status"
        );

        Ok(())
    }

    /// Regression test: commit --all should stage deletions of tracked files
    #[test]
    fn test_commit_all_stages_deletions() -> Result<()> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create, stage, and commit a file
        let test_file = temp_dir.path().join("tracked_file.txt");
        fs::write(&test_file, "content")?;
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Initial commit", false)?;

        // Delete the file from disk (not via dot rm)
        fs::remove_file(&test_file)?;

        // Run commit with --all flag (this calls stage_all_tracked_files internally)
        // The deletion should be staged and committed
        commands::commit::execute(&ctx, "Delete file", true)?;

        // Load the new commit and verify file is not present
        let ref_manager = dotman::refs::RefManager::new(ctx.repo_path.clone());
        let head_commit = ref_manager.get_head_commit()?.expect("Should have HEAD");

        let snapshot_manager = ctx.create_snapshot_manager();
        let snapshot = snapshot_manager.load_snapshot(&head_commit)?;

        let relative_path = test_file
            .strip_prefix(temp_dir.path())
            .unwrap()
            .to_path_buf();

        assert!(
            !snapshot.files.contains_key(&relative_path),
            "Deleted file should not be in new commit snapshot"
        );

        Ok(())
    }
}

mod branch_command_tests {
    use super::*;

    #[test]
    fn test_create_branch() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create initial commit
        let temp_file = ctx.repo_path.parent().unwrap().join("temp.txt");
        fs::write(&temp_file, "content")?;
        commands::add::execute(&ctx, &[temp_file.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Initial commit", false)?;

        // Create a new branch
        commands::branch::create(&ctx, "feature", None)?;

        // Check that branch was created
        let branch_ref = ctx.repo_path.join("refs/heads/feature");
        assert!(branch_ref.exists());

        Ok(())
    }

    #[test]
    fn test_list_branches() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create some branches
        commands::branch::create(&ctx, "feature1", None)?;
        commands::branch::create(&ctx, "feature2", None)?;

        // List should work and return the branches
        let ref_manager = dotman::refs::RefManager::new(ctx.repo_path);
        let branches = ref_manager.list_branches()?;

        // Verify all expected branches exist (main + 2 created)
        assert!(
            branches.len() >= 3,
            "Should have at least main + 2 created branches"
        );
        assert!(
            branches.iter().any(|b| b == "main"),
            "Should have main branch"
        );
        assert!(
            branches.iter().any(|b| b == "feature1"),
            "Should have feature1 branch"
        );
        assert!(
            branches.iter().any(|b| b == "feature2"),
            "Should have feature2 branch"
        );

        Ok(())
    }

    #[test]
    fn test_delete_branch() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create and delete a branch
        commands::branch::create(&ctx, "temp-branch", None)?;
        commands::branch::delete(&ctx, "temp-branch", false)?;

        // Branch should not exist
        let branch_ref = ctx.repo_path.join("refs/heads/temp-branch");
        assert!(!branch_ref.exists());

        Ok(())
    }

    #[test]
    fn test_cannot_delete_current_branch() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Try to delete the current branch (main)
        let result = commands::branch::delete(&ctx, "main", false);
        assert!(result.is_err());

        Ok(())
    }

    #[test]
    fn test_rename_branch() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create and rename a branch
        commands::branch::create(&ctx, "old-name", None)?;
        commands::branch::rename(&ctx, Some("old-name"), "new-name")?;

        // Old branch should not exist, new one should
        assert!(!ctx.repo_path.join("refs/heads/old-name").exists());
        assert!(ctx.repo_path.join("refs/heads/new-name").exists());

        Ok(())
    }

    #[test]
    fn test_branch_with_b_flag() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create and checkout a new branch using the shorthand
        commands::branch::create(&ctx, "feature", None)?;
        commands::checkout::execute(&ctx, "feature", false, false)?;

        // Verify we're on the new branch
        let ref_manager = dotman::refs::RefManager::new(ctx.repo_path.clone());
        let current = ref_manager.current_branch()?;
        assert_eq!(current, Some("feature".to_string()));

        // Verify branch was created
        assert!(ctx.repo_path.join("refs/heads/feature").exists());

        Ok(())
    }

    #[test]
    fn test_branch_with_b_flag_and_start_point() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create and checkout feature from main
        commands::branch::create(&ctx, "feature", Some("main"))?;
        commands::checkout::execute(&ctx, "feature", false, false)?;

        // Verify we're on feature branch
        let ref_manager = dotman::refs::RefManager::new(ctx.repo_path);
        let current = ref_manager.current_branch()?;
        assert_eq!(current, Some("feature".to_string()));

        // Verify both branches point to the same commit
        let main_commit = ref_manager.get_branch_commit("main")?;
        let feature_commit = ref_manager.get_branch_commit("feature")?;
        assert_eq!(main_commit, feature_commit);

        Ok(())
    }
}

mod checkout_command_tests {
    use super::*;

    #[test]
    fn test_checkout_branch() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create a branch and checkout
        commands::branch::create(&ctx, "feature", None)?;
        commands::checkout::execute(&ctx, "feature", false, false)?;

        // Current branch should be feature
        let ref_manager = dotman::refs::RefManager::new(ctx.repo_path);
        let current = ref_manager.current_branch()?;
        assert_eq!(current, Some("feature".to_string()));

        Ok(())
    }

    #[test]
    fn test_checkout_with_uncommitted_changes() -> Result<()> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create a branch
        commands::branch::create(&ctx, "feature", None)?;

        // Stage a file
        let test_file = temp_dir.path().join("uncommitted.txt");
        fs::write(&test_file, "uncommitted")?;
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;

        // Checkout should fail without force
        let result = commands::checkout::execute(&ctx, "feature", false, false);
        assert!(result.is_err());

        // Force checkout should work
        let result = commands::checkout::execute(&ctx, "feature", true, false);
        assert!(result.is_ok());

        Ok(())
    }

    #[test]
    fn test_checkout_nonexistent_branch() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        let result = commands::checkout::execute(&ctx, "nonexistent", false, false);
        assert!(result.is_err());

        Ok(())
    }

    #[test]
    fn test_checkout_with_b_flag_from_head() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create and checkout a new branch from HEAD using -b flag
        commands::branch::create(&ctx, "feature", None)?;
        commands::checkout::execute(&ctx, "feature", false, false)?;

        // Verify we're on the new branch
        let ref_manager = dotman::refs::RefManager::new(ctx.repo_path.clone());
        let current = ref_manager.current_branch()?;
        assert_eq!(current, Some("feature".to_string()));

        // Verify branch was created
        assert!(ctx.repo_path.join("refs/heads/feature").exists());

        Ok(())
    }

    #[test]
    fn test_checkout_with_b_flag_from_branch() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create dev branch
        commands::branch::create(&ctx, "dev", None)?;

        // Create and checkout feature from dev
        commands::branch::create(&ctx, "feature", Some("dev"))?;
        commands::checkout::execute(&ctx, "feature", false, false)?;

        // Verify we're on feature branch
        let ref_manager = dotman::refs::RefManager::new(ctx.repo_path);
        let current = ref_manager.current_branch()?;
        assert_eq!(current, Some("feature".to_string()));

        Ok(())
    }

    #[test]
    fn test_checkout_with_b_flag_from_commit() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Since setup_test_repo creates an empty repo, just use main as start point
        // This tests that we can create a branch from an existing branch name
        commands::branch::create(&ctx, "feature", Some("main"))?;
        commands::checkout::execute(&ctx, "feature", false, false)?;

        // Verify we're on feature branch
        let ref_manager = dotman::refs::RefManager::new(ctx.repo_path.clone());
        let current = ref_manager.current_branch()?;
        assert_eq!(current, Some("feature".to_string()));

        // Verify branch was created
        assert!(ctx.repo_path.join("refs/heads/feature").exists());

        Ok(())
    }

    #[test]
    fn test_checkout_b_flag_branch_already_exists() -> Result<()> {
        let (_temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create a branch
        commands::branch::create(&ctx, "existing", None)?;

        // Try to create the same branch again with -b flag
        let result = commands::branch::create(&ctx, "existing", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));

        Ok(())
    }
}

mod reset_command_tests {
    use super::*;

    fn setup_repo_with_commits() -> Result<(TempDir, DotmanContext, Vec<String>)> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;
        let mut commits = Vec::new();

        // Create multiple commits
        for i in 1..=3 {
            let file = temp_dir.path().join(format!("file{i}.txt"));
            fs::write(&file, format!("content {i}"))?;
            commands::add::execute(&ctx, &[file.to_string_lossy().into()], false, false)?;
            commands::commit::execute(&ctx, &format!("Commit {i}"), false)?;
            commits.push(ctx.create_ref_resolver().resolve("HEAD")?);
        }

        Ok((temp_dir, ctx, commits))
    }

    #[test]
    fn test_reset_to_previous_commit() -> Result<()> {
        let (_temp_dir, ctx, commits) = setup_repo_with_commits()?;

        // Reset to first commit
        commands::reset::execute(
            &ctx,
            &commits[0],
            &commands::reset::ResetOptions::default(),
            &[],
        )?;

        // HEAD should point to first commit
        let head = ctx.create_ref_resolver().resolve("HEAD")?;
        assert_eq!(head, commits[0]);

        Ok(())
    }

    #[test]
    fn test_reset_soft() -> Result<()> {
        let (temp_dir, ctx, commits) = setup_repo_with_commits()?;

        // Add another file without committing
        let new_file = temp_dir.path().join("new.txt");
        fs::write(&new_file, "new content")?;
        commands::add::execute(&ctx, &[new_file.to_string_lossy().into()], false, false)?;

        // Soft reset should preserve staged changes
        commands::reset::execute(
            &ctx,
            &commits[0],
            &commands::reset::ResetOptions {
                soft: true,
                ..Default::default()
            },
            &[],
        )?;

        let index = CommandContext::load_concurrent_index(&ctx)?;
        assert!(!index.staged_entries().is_empty());

        Ok(())
    }

    #[test]
    fn test_reset_hard() -> Result<()> {
        let (temp_dir, ctx, commits) = setup_repo_with_commits()?;

        // Add another file without committing
        let new_file = temp_dir.path().join("new.txt");
        fs::write(&new_file, "new content")?;
        commands::add::execute(&ctx, &[new_file.to_string_lossy().into()], false, false)?;

        // Hard reset should discard staged changes
        commands::reset::execute(
            &ctx,
            &commits[0],
            &commands::reset::ResetOptions {
                hard: true,
                ..Default::default()
            },
            &[],
        )?;

        let index = CommandContext::load_concurrent_index(&ctx)?;
        assert!(index.staged_entries().is_empty());

        Ok(())
    }

    #[test]
    fn test_reset_with_head_notation() -> Result<()> {
        let (_temp_dir, ctx, commits) = setup_repo_with_commits()?;

        // Get HEAD before reset
        let resolver = dotman::refs::resolver::RefResolver::new(ctx.repo_path.clone());
        let head_before = resolver.resolve("HEAD")?;
        assert_eq!(head_before, commits[2], "HEAD should point to commit 3");

        // Reset to HEAD~1
        commands::reset::execute(
            &ctx,
            "HEAD~1",
            &commands::reset::ResetOptions::default(),
            &[],
        )?;

        // Verify HEAD now points to commit 2
        let head_after = resolver.resolve("HEAD")?;
        assert_eq!(
            head_after, commits[1],
            "HEAD should point to commit 2 after reset"
        );
        assert_ne!(head_before, head_after, "HEAD should have changed");

        Ok(())
    }

    #[test]
    fn test_reset_mixed_then_status_detects_modifications() -> Result<()> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create, add, and commit a file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "original content")?;
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Initial commit", false)?;

        // Verify file is committed (staged entries should be empty after commit)
        let index_path = ctx.repo_path.join("index.bin");
        let index_after_commit = dotman::storage::index::Index::load(&index_path)?;
        assert!(
            index_after_commit.staged_entries.is_empty(),
            "Staged entries should be empty after commit"
        );

        // Modify the file on disk
        fs::write(&test_file, "modified content")?;

        // Verify file was actually modified
        let modified_content = fs::read_to_string(&test_file)?;
        assert_eq!(modified_content, "modified content");

        // Do reset --mixed to HEAD (this resets index to match HEAD but leaves working directory)
        commands::reset::execute(
            &ctx,
            "HEAD",
            &commands::reset::ResetOptions {
                mixed: true,
                ..Default::default()
            },
            &[],
        )?;

        // Load index after reset - staged entries should be empty after mixed reset
        let index_after_reset = dotman::storage::index::Index::load(&index_path)?;

        // After mixed reset to HEAD, staged entries should be cleared
        assert!(
            index_after_reset.staged_entries.is_empty(),
            "Staged entries should be empty after mixed reset"
        );

        // Now status should detect that the file on disk differs from the index
        // This is the critical test: status must recompute the hash and detect the modification
        // Previously this would show "working tree clean" due to invalid cache
        commands::status::execute(&ctx, false, false)?;

        // We can't easily capture status output in tests, but we can verify the file
        // would be detected as modified by checking if it can be re-added
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;

        // After re-adding, it should be in staged_entries (because it was modified)
        let index_after_readd = dotman::storage::index::Index::load(&index_path)?;

        // Check if file is in staged entries (try both absolute and relative paths)
        let is_staged = index_after_readd.staged_entries.contains_key(&test_file)
            || test_file
                .strip_prefix(temp_dir.path())
                .is_ok_and(|rel| index_after_readd.staged_entries.contains_key(rel));

        assert!(is_staged, "Modified file should be staged after re-adding");

        Ok(())
    }
}

mod restore_command_tests {
    use super::*;

    #[test]
    fn test_restore_modified_file() -> Result<()> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create and commit a file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "original")?;
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file", false)?;

        // Modify the file
        fs::write(&test_file, "modified")?;

        // Restore it
        commands::restore::execute(&ctx, &[test_file.to_string_lossy().into()], None, false)?;

        // File should be back to original
        let content = fs::read_to_string(&test_file)?;
        assert_eq!(content, "original");

        Ok(())
    }

    #[test]
    fn test_restore_staged_file() -> Result<()> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create, stage, and commit a file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "original")?;
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add test file", false)?;

        // Modify and stage the file
        fs::write(&test_file, "modified")?;
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;

        // Restore from HEAD (should restore to "original")
        commands::restore::execute(&ctx, &[test_file.to_string_lossy().into()], None, false)?;

        // File should be restored to original content
        let content = fs::read_to_string(&test_file)?;
        assert_eq!(content, "original");

        Ok(())
    }

    #[test]
    fn test_restore_deleted_file() -> Result<()> {
        let (temp_dir, ctx) = super::add_command_tests::setup_test_repo()?;

        // Create and commit a file
        let test_file = temp_dir.path().join("test.txt");
        fs::write(&test_file, "content")?;
        commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file", false)?;

        // Delete the file
        fs::remove_file(&test_file)?;

        // Restore it
        commands::restore::execute(&ctx, &[test_file.to_string_lossy().into()], None, false)?;

        // File should exist again
        assert!(test_file.exists());
        let content = fs::read_to_string(&test_file)?;
        assert_eq!(content, "content");

        Ok(())
    }
}

mod regression_tests {
    use super::*;

    /// Regression test for bug: Index not persisting staged entries after multiple add operations
    /// This test verifies that the index properly accumulates and persists staged entries
    /// across multiple add calls, ensuring that the fix of changing `save_merge` to `save`
    /// in the add command properly persists the index state.
    #[test]
    fn test_index_consistency_after_multiple_adds() -> Result<()> {
        let (temp_dir, ctx) = add_command_tests::setup_test_repo()?;

        // Create multiple files
        let file1 = temp_dir.path().join("file1.txt");
        let file2 = temp_dir.path().join("file2.txt");
        let file3 = temp_dir.path().join("file3.txt");

        fs::write(&file1, "content1")?;
        fs::write(&file2, "content2")?;
        fs::write(&file3, "content3")?;

        // Add files one by one and verify index state after each add
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        let index1 = CommandContext::load_concurrent_index(&ctx)?;
        assert_eq!(
            index1.staged_entries().len(),
            1,
            "Should have 1 staged file"
        );

        commands::add::execute(&ctx, &[file2.to_string_lossy().into()], false, false)?;
        let index2 = CommandContext::load_concurrent_index(&ctx)?;
        assert_eq!(
            index2.staged_entries().len(),
            2,
            "Should have 2 staged files"
        );

        commands::add::execute(&ctx, &[file3.to_string_lossy().into()], false, false)?;
        let index3 = CommandContext::load_concurrent_index(&ctx)?;
        assert_eq!(
            index3.staged_entries().len(),
            3,
            "Should have 3 staged files"
        );

        Ok(())
    }
}

mod log_command_tests {
    use super::*;

    fn setup_test_repo_with_commits() -> Result<(TempDir, DotmanContext)> {
        let temp_dir = TempDir::new()?;
        let repo_path = temp_dir.path().join(".dotman");
        let config_path = temp_dir.path().join(".config/dotman/config");

        // Create config directory
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write test config with temp directory in allowed_directories
        let config_content = format!(
            r#"[security]
allowed_directories = ["{}"]
enforce_path_validation = true
strip_dangerous_permissions = true
"#,
            temp_dir.path().display()
        );
        fs::write(&config_path, config_content)?;

        let ctx = DotmanContext::new_explicit(repo_path, config_path)?;
        ctx.ensure_repo_exists()?;

        // Initialize the repository properly
        let index = dotman::storage::index::Index::new();
        let index_path = ctx.repo_path.join("index.bin");
        index.save(&index_path)?;

        // Initialize refs structure (HEAD, branches)
        let ref_manager = dotman::refs::RefManager::new(ctx.repo_path.clone());
        ref_manager.init()?;

        // Create and commit three test files
        for i in 1..=3 {
            let test_file = temp_dir.path().join(format!("test{i}.txt"));
            fs::write(&test_file, format!("content {i}"))?;

            // Add file
            commands::add::execute(&ctx, &[test_file.to_string_lossy().into()], false, false)?;

            // Commit
            commands::commit::execute(&ctx, &format!("Commit {i}"), false)?;
        }

        Ok((temp_dir, ctx))
    }

    #[test]
    fn test_log_displays_commits() -> Result<()> {
        let (_temp_dir, ctx) = setup_test_repo_with_commits()?;

        // Verify commits exist before testing log
        let resolver = dotman::refs::resolver::RefResolver::new(ctx.repo_path.clone());
        let head = resolver.resolve("HEAD")?;
        assert!(!head.is_empty(), "HEAD should point to a commit");

        // Verify we have the expected commit chain (3 commits)
        let commit1 = resolver.resolve("HEAD~2")?;
        let commit2 = resolver.resolve("HEAD~1")?;
        let commit3 = resolver.resolve("HEAD")?;
        assert_ne!(commit1, commit2, "Commits should be unique");
        assert_ne!(commit2, commit3, "Commits should be unique");

        // Log should work without errors
        commands::log::execute(&ctx, &[], &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    fn test_log_respects_limit() -> Result<()> {
        let (_temp_dir, ctx) = setup_test_repo_with_commits()?;

        // Verify we have 3 commits
        let resolver = dotman::refs::resolver::RefResolver::new(ctx.repo_path.clone());
        let _commit1 = resolver.resolve("HEAD~2")?;
        let _commit2 = resolver.resolve("HEAD~1")?;
        let _commit3 = resolver.resolve("HEAD")?;

        // Should be able to limit - command succeeds regardless of limit
        commands::log::execute(&ctx, &[], &[], 2, false, false)?;

        Ok(())
    }

    #[test]
    fn test_log_oneline_format() -> Result<()> {
        let (_temp_dir, ctx) = setup_test_repo_with_commits()?;

        // Verify commits exist
        let resolver = dotman::refs::resolver::RefResolver::new(ctx.repo_path.clone());
        let head = resolver.resolve("HEAD")?;
        assert!(!head.is_empty(), "HEAD should point to a commit");

        // Test oneline format - should succeed
        commands::log::execute(&ctx, &[], &[], 10, true, false)?;

        Ok(())
    }

    #[test]
    fn test_log_all_shows_orphaned_commits() -> Result<()> {
        let (_temp_dir, ctx) = setup_test_repo_with_commits()?;

        // Reset to commit 2 (making commit 3 orphaned)
        let resolver = dotman::refs::resolver::RefResolver::new(ctx.repo_path.clone());
        let commit2 = resolver.resolve("HEAD~1")?;
        commands::reset::execute(
            &ctx,
            &commit2,
            &commands::reset::ResetOptions {
                mixed: true,
                ..Default::default()
            },
            &[],
        )?;

        // Normal log should show 2 commits (reachable from HEAD)
        let result_normal = commands::log::execute(&ctx, &[], &[], 10, false, false);
        assert!(result_normal.is_ok());

        // Log --all should show all 3 commits (including orphaned)
        let result_all = commands::log::execute(&ctx, &[], &[], 10, false, true);
        assert!(result_all.is_ok());

        Ok(())
    }

    #[test]
    fn test_log_with_specific_target() -> Result<()> {
        let (_temp_dir, ctx) = setup_test_repo_with_commits()?;

        // Get commit references
        let resolver = dotman::refs::resolver::RefResolver::new(ctx.repo_path.clone());
        let commit2 = resolver.resolve("HEAD~1")?;
        let commit1 = resolver.resolve("HEAD~2")?;

        // Verify commit2 exists and has commit1 as ancestor
        assert!(!commit2.is_empty(), "HEAD~1 should resolve to a commit");
        assert_ne!(commit1, commit2, "Commits should be different");

        // Test starting from a specific commit - should succeed
        let args = vec![commit2];
        commands::log::execute(&ctx, &args, &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    fn test_log_with_head_reference() -> Result<()> {
        let (_temp_dir, ctx) = setup_test_repo_with_commits()?;

        // Test with HEAD reference
        let args = vec!["HEAD".to_string()];
        let result = commands::log::execute(&ctx, &args, &[], 10, false, false);
        assert!(result.is_ok());

        Ok(())
    }

    #[test]
    fn test_log_empty_repository() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let repo_path = temp_dir.path().join(".dotman");
        let config_path = temp_dir.path().join(".config/dotman/config");

        let ctx = DotmanContext::new_explicit(repo_path, config_path)?;
        ctx.ensure_repo_exists()?;

        let index = dotman::storage::index::Index::new();
        let index_path = ctx.repo_path.join("index.bin");
        index.save(&index_path)?;

        let ref_manager = dotman::refs::RefManager::new(ctx.repo_path.clone());
        ref_manager.init()?;

        // Log on empty repo should succeed (just show "No commits yet")
        let result = commands::log::execute(&ctx, &[], &[], 10, false, false);
        assert!(result.is_ok());

        Ok(())
    }

    // New tests for file path filtering

    #[test]
    #[serial]
    fn test_log_with_single_file_filter() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        // Create 3 commits touching different files
        let file1 = temp_dir.path().join("file1.txt");
        let file2 = temp_dir.path().join("file2.txt");

        // Commit 1: add file1
        fs::write(&file1, "content1")?;
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file1", false)?;

        // Commit 2: add file2 (doesn't touch file1)
        fs::write(&file2, "content2")?;
        commands::add::execute(&ctx, &[file2.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file2", false)?;

        // Commit 3: modify file1
        fs::write(&file1, "content1 modified")?;
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Modify file1", false)?;

        // Log filtering by file1 should show commits 1 and 3, skip commit 2
        let args = vec!["file1.txt".to_string()];
        commands::log::execute(&ctx, &args, &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_with_multiple_file_filters() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        let file1 = temp_dir.path().join("file1.txt");
        let file2 = temp_dir.path().join("file2.txt");
        let file3 = temp_dir.path().join("file3.txt");

        // Commit 1: file1 only
        fs::write(&file1, "v1")?;
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file1", false)?;

        // Commit 2: file2 only
        fs::write(&file2, "v1")?;
        commands::add::execute(&ctx, &[file2.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file2", false)?;

        // Commit 3: file3 only (not in filter)
        fs::write(&file3, "v1")?;
        commands::add::execute(&ctx, &[file3.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file3", false)?;

        // Filter by file1 OR file2 - should show commits 1 and 2, skip 3
        let args = vec!["file1.txt".to_string(), "file2.txt".to_string()];
        commands::log::execute(&ctx, &args, &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_with_ref_and_file() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        let file1 = temp_dir.path().join("file1.txt");

        // Create 3 commits
        for i in 1..=3 {
            fs::write(&file1, format!("v{i}"))?;
            commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
            commands::commit::execute(&ctx, &format!("Commit {i}"), false)?;
        }

        // Log from HEAD~1 filtered by file1
        // Should show commits 1 and 2, but not commit 3
        let args = vec!["HEAD~1".to_string(), "file1.txt".to_string()];
        commands::log::execute(&ctx, &args, &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_file_not_in_any_commit() -> Result<()> {
        let (_temp_dir, ctx) = setup_log_test_repo()?;

        // Filter by non-existent file - should show "No commits found"
        let args = vec!["nonexistent.txt".to_string()];
        commands::log::execute(&ctx, &args, &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_detects_file_deletion() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        let file1 = temp_dir.path().join("file1.txt");

        // Commit 1: add file
        fs::write(&file1, "content")?;
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file", false)?;

        // Commit 2: delete file
        let rm_options = dotman::commands::rm::RmOptions {
            cached: false,
            force: false,
            recursive: false,
            dry_run: false,
        };
        commands::rm::execute(&ctx, &[file1.to_string_lossy().into()], &rm_options)?;
        commands::commit::execute(&ctx, "Delete file", false)?;

        // Log should show both commits (add and delete are changes)
        let args = vec!["file1.txt".to_string()];
        commands::log::execute(&ctx, &args, &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_backward_compatibility() -> Result<()> {
        let (_temp_dir, ctx) = setup_log_test_repo()?;

        // All existing usage patterns should still work:

        // No args - from HEAD
        commands::log::execute(&ctx, &[], &[], 10, false, false)?;

        // Ref only
        let args = vec!["HEAD".to_string()];
        commands::log::execute(&ctx, &args, &[], 10, false, false)?;

        // Branch name
        let args = vec!["main".to_string()];
        commands::log::execute(&ctx, &args, &[], 10, false, false)?;

        // Short commit ID (if available)
        let resolver = dotman::refs::resolver::RefResolver::new(ctx.repo_path.clone());
        if let Ok(commit_id) = resolver.resolve("HEAD") {
            let short_id = &commit_id[..8];
            let args = vec![short_id.to_string()];
            commands::log::execute(&ctx, &args, &[], 10, false, false)?;
        }

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_separator_forces_path() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        // Create a file named "main" (same name as the default branch)
        let main_file = temp_dir.path().join("main");
        fs::write(&main_file, "content for main file")?;
        commands::add::execute(&ctx, &[main_file.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file named main", false)?;

        // Without separator: "main" resolves to branch, shows all commits from main branch
        let args = vec!["main".to_string()];
        commands::log::execute(&ctx, &args, &[], 10, false, false)?;

        // With separator: "-- main" forces "main" as a path, filters to commits touching that file
        let args = vec!["--".to_string(), "main".to_string()];
        commands::log::execute(&ctx, &args, &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_separator_with_ref_and_path() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        let file1 = temp_dir.path().join("file1.txt");

        // Create 3 commits
        for i in 1..=3 {
            fs::write(&file1, format!("version {i}"))?;
            commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
            commands::commit::execute(&ctx, &format!("Commit {i}"), false)?;
        }

        // dot log HEAD~1 -- file1.txt (explicit separator)
        let args = vec![
            "HEAD~1".to_string(),
            "--".to_string(),
            "file1.txt".to_string(),
        ];
        commands::log::execute(&ctx, &args, &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_separator_only_paths() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        let file1 = temp_dir.path().join("file1.txt");
        let file2 = temp_dir.path().join("file2.txt");

        // Commit 1: add file1
        fs::write(&file1, "content1")?;
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file1", false)?;

        // Commit 2: add file2
        fs::write(&file2, "content2")?;
        commands::add::execute(&ctx, &[file2.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file2", false)?;

        // dot log -- file1.txt file2.txt (no ref, paths only via separator)
        let args = vec![
            "--".to_string(),
            "file1.txt".to_string(),
            "file2.txt".to_string(),
        ];
        commands::log::execute(&ctx, &args, &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_separator_multiple_refs_union() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        let file1 = temp_dir.path().join("file1.txt");

        // Create initial commit on main
        fs::write(&file1, "initial")?;
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Initial commit", false)?;

        // Create a feature branch
        commands::branch::create(&ctx, "feature", None)?;

        // Add commit on main
        fs::write(&file1, "main change")?;
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Main branch commit", false)?;

        // Switch to feature branch and add commit
        commands::branch::checkout(&ctx, "feature", false)?;
        fs::write(&file1, "feature change")?;
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Feature branch commit", false)?;

        // dot log main feature -- file1.txt (union of both branches)
        let args = vec![
            "main".to_string(),
            "feature".to_string(),
            "--".to_string(),
            "file1.txt".to_string(),
        ];
        commands::log::execute(&ctx, &args, &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_empty_separator() -> Result<()> {
        let (_temp_dir, ctx) = setup_log_test_repo()?;

        // dot log -- (no ref, no paths - should show all from HEAD)
        let args = vec!["--".to_string()];
        commands::log::execute(&ctx, &args, &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_glob_star_pattern() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        // Create files with .txt extension and other extensions
        let file1 = temp_dir.path().join("file1.txt");
        let file2 = temp_dir.path().join("file2.rs");

        // Commit 1: add txt file
        fs::write(&file1, "content1")?;
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file1.txt", false)?;

        // Commit 2: add rs file
        fs::write(&file2, "content2")?;
        commands::add::execute(&ctx, &[file2.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file2.rs", false)?;

        // Pattern *.txt should only show commit 1
        commands::log::execute(&ctx, &["*.txt".to_string()], &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_glob_nested_match() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        // Create nested directory structure
        let subdir = temp_dir.path().join("subdir");
        fs::create_dir_all(&subdir)?;

        let file1 = temp_dir.path().join("root.txt");
        let file2 = subdir.join("nested.txt");

        // Commit 1: add root.txt
        fs::write(&file1, "root content")?;
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add root.txt", false)?;

        // Commit 2: add nested.txt
        fs::write(&file2, "nested content")?;
        commands::add::execute(&ctx, &[file2.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add nested.txt", false)?;

        // Git-style pattern *.txt should match both (at any depth)
        commands::log::execute(&ctx, &["*.txt".to_string()], &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_glob_character_class() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        let makefile1 = temp_dir.path().join("Makefile");
        let makefile2 = temp_dir.path().join("makefile");
        let other_file = temp_dir.path().join("other.txt");

        // Commit 1: add Makefile
        fs::write(&makefile1, "UPPER")?;
        commands::add::execute(&ctx, &[makefile1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add Makefile", false)?;

        // Commit 2: add makefile (lowercase)
        fs::write(&makefile2, "lower")?;
        commands::add::execute(&ctx, &[makefile2.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add makefile", false)?;

        // Commit 3: add other file (should not match)
        fs::write(&other_file, "other")?;
        commands::add::execute(&ctx, &[other_file.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add other.txt", false)?;

        // Pattern [Mm]akefile should match commits 1 and 2
        commands::log::execute(&ctx, &["[Mm]akefile".to_string()], &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_glob_question_mark() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        let file1 = temp_dir.path().join("file1.txt");
        let file2 = temp_dir.path().join("file2.txt");
        let file10 = temp_dir.path().join("file10.txt");

        // Commit 1: file1.txt
        fs::write(&file1, "1")?;
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file1.txt", false)?;

        // Commit 2: file2.txt
        fs::write(&file2, "2")?;
        commands::add::execute(&ctx, &[file2.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file2.txt", false)?;

        // Commit 3: file10.txt (should NOT match file?.txt)
        fs::write(&file10, "10")?;
        commands::add::execute(&ctx, &[file10.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file10.txt", false)?;

        // Pattern file?.txt should match file1.txt and file2.txt but not file10.txt
        commands::log::execute(&ctx, &["file?.txt".to_string()], &[], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_glob_mixed_exact_and_pattern() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        let file1 = temp_dir.path().join("exact.txt");
        let file2 = temp_dir.path().join("pattern.rs");
        let file3 = temp_dir.path().join("other.md");

        // Commit 1: exact.txt
        fs::write(&file1, "exact")?;
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add exact.txt", false)?;

        // Commit 2: pattern.rs
        fs::write(&file2, "pattern")?;
        commands::add::execute(&ctx, &[file2.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add pattern.rs", false)?;

        // Commit 3: other.md (should not match)
        fs::write(&file3, "other")?;
        commands::add::execute(&ctx, &[file3.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add other.md", false)?;

        // Mix of exact path and pattern - should show commits 1 and 2
        commands::log::execute(
            &ctx,
            &["exact.txt".to_string(), "*.rs".to_string()],
            &[],
            10,
            false,
            false,
        )?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_glob_with_separator() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        let file1 = temp_dir.path().join("code.rs");
        let file2 = temp_dir.path().join("notes.txt");

        // Commit 1: code.rs
        fs::write(&file1, "fn main() {}")?;
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add code.rs", false)?;

        // Commit 2: notes.txt
        fs::write(&file2, "notes")?;
        commands::add::execute(&ctx, &[file2.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add notes.txt", false)?;

        // Use explicit -- separator with glob pattern
        // refs = [], paths = ["*.rs"]
        commands::log::execute(&ctx, &[], &["*.rs".to_string()], 10, false, false)?;

        Ok(())
    }

    #[test]
    #[serial]
    fn test_log_glob_no_matches() -> Result<()> {
        let (temp_dir, ctx) = setup_log_test_repo()?;

        let file1 = temp_dir.path().join("file.txt");

        // Create a single commit
        fs::write(&file1, "content")?;
        commands::add::execute(&ctx, &[file1.to_string_lossy().into()], false, false)?;
        commands::commit::execute(&ctx, "Add file.txt", false)?;

        // Pattern that matches no files - should show "no commits found" message
        commands::log::execute(&ctx, &["*.nonexistent".to_string()], &[], 10, false, false)?;

        Ok(())
    }

    fn setup_log_test_repo() -> Result<(TempDir, DotmanContext)> {
        let temp_dir = TempDir::new()?;
        let repo_path = temp_dir.path().join(".dotman");
        let config_path = temp_dir.path().join(".config/dotman/config");

        // Create config directory
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write test config with temp directory in allowed_directories
        let config_content = format!(
            r#"[security]
allowed_directories = ["{}"]
enforce_path_validation = true
strip_dangerous_permissions = true
"#,
            temp_dir.path().display()
        );
        fs::write(&config_path, config_content)?;

        let ctx = DotmanContext::new_explicit(repo_path, config_path)?;
        ctx.ensure_repo_exists()?;

        // Initialize the repository properly
        let index = dotman::storage::index::Index::new();
        let index_path = ctx.repo_path.join("index.bin");
        index.save(&index_path)?;

        // Initialize refs structure (HEAD, branches)
        let ref_manager = dotman::refs::RefManager::new(ctx.repo_path.clone());
        ref_manager.init()?;

        Ok((temp_dir, ctx))
    }
}
