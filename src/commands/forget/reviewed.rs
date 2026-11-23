#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fixture() -> (tempfile::TempDir, Context) {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().as_os_str().to_owned();
        let env = crate::paths::Env::from_fn(|name| (name == "HOME").then(|| home.clone()));
        let paths = crate::paths::Paths::resolve(&env, crate::sys::geteuid()).unwrap();
        std::fs::create_dir_all(&paths.repo).unwrap();
        let status = crate::proc::command("git")
            .args(["init", "--quiet", "--template="])
            .current_dir(&paths.repo)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(paths.repo.join(".git/dotman-id"), "test").unwrap();
        let ctx = Context::new(paths, crate::config::Config::default(), "remedy");
        (temp, ctx)
    }

    #[test]
    fn exact_inventory_includes_variants_but_not_descendants() {
        let (_temp, ctx) = fixture();
        std::fs::create_dir_all(ctx.paths.repo.join("settings")).unwrap();
        for name in [
            "settings.conf",
            "settings{host=laptop}.conf",
            "settings/child",
        ] {
            std::fs::write(ctx.paths.repo.join(name), b"same").unwrap();
        }
        let review = inventory(&ctx, "settings.conf").unwrap();
        assert_eq!(
            review
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            ["settings.conf", "settings{host=laptop}.conf"]
        );
        assert!(inventory(&ctx, "settings").is_err());
    }

    #[test]
    fn mode_change_changes_inventory() {
        let (_temp, ctx) = fixture();
        let path = ctx.paths.repo.join("settings");
        std::fs::write(&path, b"same").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let before = inventory(&ctx, "settings").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert_ne!(before, inventory(&ctx, "settings").unwrap());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn forget_refuses_backup_state_inside_repo_without_changes() -> anyhow::Result<()> {
        let (temp, ctx) = fixture();
        std::fs::write(ctx.paths.repo.join("file"), b"repo-only edit")?;
        std::fs::write(ctx.paths.home.join("file"), b"home copy")?;
        std::fs::create_dir_all(&ctx.paths.state_dir)?;
        std::fs::set_permissions(&ctx.paths.state_dir, std::fs::Permissions::from_mode(0o700))?;
        let review = review_forget(&ctx, "file")?;
        assert!(!ctx.paths.state_dir.join("backups").exists());

        let mut paths = ctx.paths.clone();
        paths.state_dir = ctx.paths.repo.join("private-state");
        std::fs::create_dir(&paths.state_dir)?;
        std::fs::set_permissions(&paths.state_dir, std::fs::Permissions::from_mode(0o700))?;
        let inside = Context::new(paths, ctx.machine.clone(), "remedy");
        let source = std::fs::read(inside.paths.repo.join("file"))?;
        let home = std::fs::read(inside.paths.home.join("file"))?;
        let index = std::fs::read(inside.paths.repo.join(".git/index")).ok();
        let state = std::fs::read(inside.paths.state_file()).ok();
        assert!(review_forget(&inside, "file").is_err());
        assert!(execute_forget(&inside, &review).is_err());
        assert!(!inside.paths.state_dir.join("backups").exists());
        assert_eq!(std::fs::read(inside.paths.repo.join("file"))?, source);
        assert_eq!(std::fs::read(inside.paths.home.join("file"))?, home);
        assert_eq!(
            std::fs::read(inside.paths.repo.join(".git/index")).ok(),
            index
        );
        assert_eq!(std::fs::read(inside.paths.state_file()).ok(), state);

        let alias = temp.path().join("state-alias");
        std::os::unix::fs::symlink(&inside.paths.state_dir, &alias)?;
        let mut paths = ctx.paths.clone();
        paths.state_dir = alias;
        let aliased = Context::new(paths, ctx.machine, "remedy");
        assert!(review_forget(&aliased, "file").is_err());
        assert!(execute_forget(&aliased, &review).is_err());
        assert!(!inside.paths.state_dir.join("backups").exists());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn forget_review_does_not_create_missing_backup_root() -> anyhow::Result<()> {
        let (_temp, ctx) = fixture();
        std::fs::write(ctx.paths.repo.join("file"), b"repo")?;
        std::fs::write(ctx.paths.home.join("file"), b"home")?;
        std::fs::create_dir_all(&ctx.paths.state_dir)?;
        std::fs::set_permissions(&ctx.paths.state_dir, std::fs::Permissions::from_mode(0o700))?;
        let backup_root = ctx.paths.state_dir.join("backups");
        let review = review_forget(&ctx, "file")?;
        assert!(!backup_root.exists());
        let receipt = execute_forget(&ctx, &review)?;
        assert_eq!(
            std::fs::read(
                backup_root
                    .join(receipt.backup_run)
                    .join(".dotman/repo/file")
            )?,
            b"repo"
        );
        assert_eq!(std::fs::read(ctx.paths.home.join("file"))?, b"home");
        assert!(!ctx.paths.repo.join("file").exists());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn forget_refuses_a_backup_root_alias_into_the_repo() -> anyhow::Result<()> {
        let (_temp, ctx) = fixture();
        std::fs::write(ctx.paths.repo.join("file"), b"repo")?;
        std::fs::write(ctx.paths.home.join("file"), b"home")?;
        std::fs::create_dir_all(&ctx.paths.state_dir)?;
        std::fs::set_permissions(&ctx.paths.state_dir, std::fs::Permissions::from_mode(0o700))?;
        let review = review_forget(&ctx, "file")?;
        let target = ctx.paths.repo.join("inside-backups");
        std::fs::create_dir(&target)?;
        std::os::unix::fs::symlink(&target, ctx.paths.state_dir.join("backups"))?;
        let index = std::fs::read(ctx.paths.repo.join(".git/index")).ok();
        let state = std::fs::read(ctx.paths.state_file()).ok();
        assert!(review_forget(&ctx, "file").is_err());
        assert!(execute_forget(&ctx, &review).is_err());
        assert_eq!(std::fs::read(ctx.paths.repo.join("file"))?, b"repo");
        assert_eq!(std::fs::read(ctx.paths.home.join("file"))?, b"home");
        assert_eq!(std::fs::read(ctx.paths.repo.join(".git/index")).ok(), index);
        assert_eq!(std::fs::read(ctx.paths.state_file()).ok(), state);
        assert_eq!(std::fs::read_dir(target)?.count(), 0);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn quarantine_preserves_reviewed_file_and_home() {
        let (_temp, ctx) = fixture();
        std::fs::create_dir_all(ctx.paths.repo.join("nested")).unwrap();
        std::fs::write(ctx.paths.repo.join("nested/file"), b"repo").unwrap();
        std::fs::create_dir_all(ctx.paths.home.join("nested")).unwrap();
        std::fs::write(ctx.paths.home.join("nested/file"), b"home").unwrap();
        let reviewed = inventory(&ctx, "nested/file").unwrap();
        let saved = quarantine(&ctx, "nested/file", reviewed.first().unwrap()).unwrap();
        assert_eq!(std::fs::read(&saved).unwrap(), b"repo");
        assert!(!ctx.paths.repo.join("nested/file").exists());
        assert_eq!(
            std::fs::read(ctx.paths.home.join("nested/file")).unwrap(),
            b"home"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn changed_source_is_restored() {
        let (_temp, ctx) = fixture();
        let path = ctx.paths.repo.join("file");
        std::fs::write(&path, b"before").unwrap();
        let reviewed = inventory(&ctx, "file").unwrap();
        std::fs::write(&path, b"after").unwrap();
        assert!(quarantine(&ctx, "file", reviewed.first().unwrap()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"after");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn changed_source_and_replacement_are_preserved() {
        let (_temp, ctx) = fixture();
        let path = ctx.paths.repo.join("file");
        std::fs::write(&path, b"before").unwrap();
        let reviewed = inventory(&ctx, "file").unwrap();
        std::fs::write(&path, b"changed").unwrap();
        let error = quarantine_inner(&ctx, "file", reviewed.first().unwrap(), || {
            std::fs::write(&path, b"replacement").unwrap();
        })
        .unwrap_err();
        let saved = std::fs::read_dir(ctx.paths.repo.join(".git"))
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".dotman-forget-")
            })
            .unwrap()
            .path()
            .join("source");
        assert!(error.to_string().contains(&saved.display().to_string()));
        assert_eq!(std::fs::read(saved).unwrap(), b"changed");
        assert_eq!(std::fs::read(path).unwrap(), b"replacement");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn forgets_only_reviewed_variants_and_retains_home_and_backups() {
        let (_temp, ctx) = fixture();
        std::fs::create_dir_all(ctx.paths.repo.join("dir")).unwrap();
        std::fs::create_dir_all(ctx.paths.home.join("dir")).unwrap();
        std::fs::write(ctx.paths.home.join("dir/a"), b"home").unwrap();
        std::fs::write(ctx.paths.repo.join("dir/a"), b"repo").unwrap();
        std::fs::write(ctx.paths.repo.join("dir/a{host=x}"), b"variant").unwrap();
        std::fs::write(ctx.paths.repo.join("dir/other"), b"other").unwrap();
        std::fs::write(
            ctx.repo_config_file(),
            "# preserved comment\n[dirs]\npaths = [\"dir\"] # tracked directory\n[modes]\n\"dir/a\" = \"0600\"\n\"dir/other\" = \"0640\" # unrelated mode\n",
        )
        .unwrap();
        std::fs::set_permissions(
            ctx.repo_config_file(),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let mut state = crate::state::State::load(&ctx.paths.state_file(), "test").unwrap();
        state.files.insert(
            "dir/a".into(),
            Tuple::of_file(b"home", 0o600).entry("dir/a"),
        );
        state.files.insert(
            "dir/other".into(),
            Tuple::of_file(b"other", 0o640).entry("dir/other"),
        );
        state.save(&ctx.paths.state_file(), false).unwrap();
        let review = review_forget(&ctx, "dir/a").unwrap();
        let receipt = execute_forget(&ctx, &review).unwrap();
        assert_eq!(receipt.quarantines.len(), 0);
        assert!(
            ctx.paths
                .state_dir
                .join("backups")
                .join(&receipt.backup_run)
                .is_dir()
        );
        assert!(!ctx.paths.repo.join("dir/a").exists());
        assert!(!ctx.paths.repo.join("dir/a{host=x}").exists());
        assert_eq!(
            std::fs::read(ctx.paths.home.join("dir/a")).unwrap(),
            b"home"
        );
        assert_eq!(
            std::fs::read(ctx.paths.repo.join("dir/other")).unwrap(),
            b"other"
        );
        let config = ctx.repo_config().unwrap();
        let text = std::fs::read_to_string(ctx.repo_config_file()).unwrap();
        assert!(text.contains("# preserved comment"));
        assert!(text.contains("# tracked directory"));
        assert!(text.contains("# unrelated mode"));
        assert!(!config.modes.contains_key("dir/a"));
        assert_eq!(
            config.modes.get("dir/other").map(String::as_str),
            Some("0640")
        );
        assert!(config.ignore.patterns.contains(&"dir/a".to_string()));
        let state = crate::state::State::load(&ctx.paths.state_file(), "test").unwrap();
        assert!(!state.files.contains_key("dir/a"));
        assert!(state.files.contains_key("dir/other"));
        let backup_files: Vec<_> = walkdir::WalkDir::new(ctx.paths.state_dir.join("backups"))
            .into_iter()
            .map(Result::unwrap)
            .filter(|e| e.file_type().is_file())
            .collect();
        assert_eq!(backup_files.len(), 2);
        assert!(
            !std::fs::read_dir(ctx.paths.repo.join(".git"))
                .unwrap()
                .map(Result::unwrap)
                .any(|e| e
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".dotman-forget-"))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn added_variant_refuses_before_writing() {
        let (_temp, ctx) = fixture();
        std::fs::write(ctx.paths.repo.join("file"), b"one").unwrap();
        let review = review_forget(&ctx, "file").unwrap();
        std::fs::write(ctx.paths.repo.join("file{host=x}"), b"two").unwrap();
        assert!(execute_forget(&ctx, &review).is_err());
        assert_eq!(std::fs::read(ctx.paths.repo.join("file")).unwrap(), b"one");
        assert_eq!(
            std::fs::read(ctx.paths.repo.join("file{host=x}")).unwrap(),
            b"two"
        );
        assert!(!ctx.paths.state_dir.join("backups").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn serialized_review_refuses_a_changed_home_or_config_mode() {
        let (_temp, ctx) = fixture();
        std::fs::write(ctx.paths.repo.join("file"), b"repo-content-unique").unwrap();
        std::fs::write(ctx.paths.home.join("file"), b"home-content-unique").unwrap();
        std::fs::write(ctx.repo_config_file(), "[modes]\n\"file\" = \"0600\"\n").unwrap();
        let review = review_forget(&ctx, "file").unwrap();
        let encoded = serde_json::to_string(&review).unwrap();
        assert!(!encoded.contains("home-content-unique"));
        assert!(!encoded.contains("repo-content-unique"));
        let review: ForgetReview = serde_json::from_str(&encoded).unwrap();
        std::fs::write(ctx.paths.home.join("file"), b"changed").unwrap();
        assert!(execute_forget(&ctx, &review).is_err());
        std::fs::write(ctx.paths.home.join("file"), b"home-content-unique").unwrap();
        std::fs::set_permissions(
            ctx.repo_config_file(),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert!(execute_forget(&ctx, &review).is_err());
        assert_eq!(
            std::fs::read(ctx.paths.repo.join("file")).unwrap(),
            b"repo-content-unique"
        );
        assert!(!ctx.paths.state_dir.join("backups").exists());
    }

    #[cfg(target_os = "linux")]
    fn recovery_fixture() -> (tempfile::TempDir, Context) {
        let (temp, ctx) = fixture();
        let backup = ctx.paths.state_dir.join("backups");
        crate::paths::create_private_dir(&backup, false).unwrap();
        for path in [&ctx.paths.state_dir, &backup] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        (temp, ctx)
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_moves_original_inode_outside_repo() {
        use std::os::unix::fs::MetadataExt;
        let (_temp, ctx) = recovery_fixture();
        let source = ctx.paths.repo.join("file");
        std::fs::write(&source, b"original").unwrap();
        let inode = std::fs::metadata(&source).unwrap().ino();
        let review = inventory(&ctx, "file").unwrap();
        let entry = review.first().unwrap();
        let location = recovery_preflight(&ctx, entry).unwrap();
        let backup = move_to_recovery(&ctx, "file", entry, &location).unwrap();
        assert!(backup.starts_with(ctx.paths.state_dir.join("backups")));
        assert_eq!(std::fs::metadata(&backup).unwrap().ino(), inode);
        assert_eq!(std::fs::read(backup).unwrap(), b"original");
        assert!(!source.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_refuses_changed_location_and_preserves_replaced_source() {
        use std::os::unix::fs::MetadataExt;
        let (_temp, ctx) = recovery_fixture();
        let path = ctx.paths.repo.join("file");
        std::fs::write(&path, b"before").unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        let review = inventory(&ctx, "file").unwrap();
        let entry = review.first().unwrap();
        let location = recovery_preflight(&ctx, entry).unwrap();
        let mut wrong = location.clone();
        wrong.device = wrong.device.wrapping_add(1);
        assert!(move_to_recovery(&ctx, "file", entry, &wrong).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"before");
        std::fs::write(&path, b"changed").unwrap();
        let error = move_to_recovery_inner(&ctx, "file", entry, &location, || {
            std::fs::write(&path, b"replacement").unwrap();
        })
        .unwrap_err();
        let directory = std::fs::read_dir(ctx.paths.state_dir.join("backups"))
            .unwrap()
            .map(Result::unwrap)
            .next()
            .unwrap()
            .path();
        let backup = directory.join("source");
        let partial = error.downcast_ref::<RecoveryMovePartial>().unwrap();
        assert_eq!(partial.recovery, backup);
        let run = directory.file_name().unwrap().to_str().unwrap();
        assert!(run.starts_with("recovered-") && run.len() <= 128);
        assert!(
            run.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        );
        assert!(!run.contains("file"));
        assert_eq!(std::fs::metadata(&backup).unwrap().ino(), inode);
        assert!(error.to_string().contains(&backup.display().to_string()));
        assert_eq!(std::fs::read(backup).unwrap(), b"changed");
        assert_eq!(std::fs::read(path).unwrap(), b"replacement");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_refuses_a_hard_linked_source() {
        let (_temp, ctx) = recovery_fixture();
        let source = ctx.paths.repo.join("file");
        let other = ctx.paths.home.join("other-link");
        std::fs::write(&source, b"original").unwrap();
        std::fs::hard_link(&source, &other).unwrap();
        let review = inventory(&ctx, "file").unwrap();
        assert!(recovery_preflight(&ctx, review.first().unwrap()).is_err());
        assert_eq!(std::fs::read(source).unwrap(), b"original");
        assert_eq!(std::fs::read(other).unwrap(), b"original");
        assert_eq!(
            std::fs::read_dir(ctx.paths.state_dir.join("backups"))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_refuses_an_identical_replacement_inode() {
        let (temp, ctx) = recovery_fixture();
        let source = ctx.paths.repo.join("file");
        std::fs::write(&source, b"original").unwrap();
        let review = inventory(&ctx, "file").unwrap();
        let entry = review.first().unwrap();
        let location = recovery_preflight(&ctx, entry).unwrap();
        let saved = temp.path().join("original-inode");
        std::fs::rename(&source, &saved).unwrap();
        std::fs::write(&source, b"original").unwrap();
        assert!(move_to_recovery(&ctx, "file", entry, &location).is_err());
        assert_eq!(std::fs::read(source).unwrap(), b"original");
        assert_eq!(std::fs::read(saved).unwrap(), b"original");
        assert_eq!(
            std::fs::read_dir(ctx.paths.state_dir.join("backups"))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn recovery_restores_a_source_linked_after_move() {
        let (_temp, ctx) = recovery_fixture();
        let source = ctx.paths.repo.join("file");
        let other = ctx.paths.home.join("new-link");
        std::fs::write(&source, b"original").unwrap();
        let review = inventory(&ctx, "file").unwrap();
        let entry = review.first().unwrap();
        let location = recovery_preflight(&ctx, entry).unwrap();
        let error = move_to_recovery_inner(&ctx, "file", entry, &location, || {
            let directory = std::fs::read_dir(ctx.paths.state_dir.join("backups"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            std::fs::hard_link(directory.join("source"), &other).unwrap();
        })
        .unwrap_err();
        assert!(error.to_string().contains("restored"));
        assert_eq!(std::fs::read(&source).unwrap(), b"original");
        assert_eq!(std::fs::read(&other).unwrap(), b"original");
        assert_eq!(
            std::fs::read_dir(ctx.paths.state_dir.join("backups"))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn protected_metadata_refuses_symlink_and_oversize() {
        let (_temp, ctx) = fixture();
        std::fs::write(ctx.paths.repo.join("file"), b"source").unwrap();
        let config = ctx.repo_config_file();
        std::os::unix::fs::symlink(ctx.paths.home.join("target"), &config).unwrap();
        assert!(review_forget(&ctx, "file").is_err());
        assert!(file_bytes(&config).is_err());
        std::fs::remove_file(&config).unwrap();
        let file = std::fs::File::create(&config).unwrap();
        file.set_len(16_777_217).unwrap();
        assert!(file_bytes(&config).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn partial_batch_reports_only_private_directory_names() {
        let (_temp, ctx) = fixture();
        std::fs::create_dir_all(ctx.paths.repo.join("dir")).unwrap();
        std::fs::write(ctx.paths.repo.join("dir/hidden"), b"first-content").unwrap();
        std::fs::write(ctx.paths.repo.join("dir/hidden{host=x}"), b"second-content").unwrap();
        crate::state::State::load(&ctx.paths.state_file(), "test")
            .unwrap()
            .save(&ctx.paths.state_file(), false)
            .unwrap();
        let review = review_forget(&ctx, "dir/hidden").unwrap();
        let error =
            execute_forget_inner(&ctx, &review, |_| anyhow::bail!("injected failure")).unwrap_err();
        let partial = error.downcast_ref::<ForgetPartial>().unwrap();
        let encoded = serde_json::to_string(&partial.receipt).unwrap();
        assert!(!encoded.contains("dir/hidden"));
        assert!(!encoded.contains("first-content"));
        assert!(!encoded.contains(ctx.paths.repo.to_str().unwrap()));
        assert_eq!(partial.receipt.quarantines.len(), 1);
        assert!(
            ctx.paths
                .state_dir
                .join("backups")
                .join(&partial.receipt.backup_run)
                .join(".dotman/repo/dir/hidden")
                .exists()
        );
        assert!(
            ctx.paths
                .repo
                .join(".git")
                .join(partial.receipt.quarantines.first().unwrap())
                .join("source")
                .exists()
        );
        assert_eq!(
            std::fs::read(ctx.paths.repo.join("dir/hidden{host=x}")).unwrap(),
            b"second-content"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pinned_state_is_parsed_after_the_path_changes() {
        let (_temp, ctx) = fixture();
        let path = ctx.paths.state_file();
        crate::state::State::load(&path, "test")
            .unwrap()
            .save(&path, false)
            .unwrap();
        let (bytes, _) = control_file(&path).unwrap().unwrap();
        std::fs::write(&path, b"invalid state").unwrap();
        let state = crate::state::State::parse_bytes(&path, &bytes).unwrap();
        assert_eq!(state.repo_id, "test");
        assert!(crate::state::State::parse_bytes(&path, b"invalid state").is_err());
    }
}
use crate::commands::review::Stored;
use crate::context::Context;
use crate::engine::Tuple;
use crate::state::Kind;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgetReview {
    pub(crate) key: String,
    pub(crate) repo_id: String,
    pub(crate) sources: Vec<Source>,
    pub(crate) home: Option<Source>,
    pub(crate) holds: Vec<(String, String)>,
    context: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ForgetReceipt {
    pub backup_run: String,
    pub quarantines: Vec<String>,
}

#[derive(Debug)]
pub struct ForgetPartial {
    pub receipt: ForgetReceipt,
}

impl std::fmt::Display for ForgetPartial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("forget did not finish; inspect the private recovery locations")
    }
}

impl std::error::Error for ForgetPartial {}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct QuarantineConflict {
    directory: String,
    path: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
impl std::fmt::Display for QuarantineConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "source could not be restored; recover it from {}",
            self.path.display()
        )
    }
}

#[cfg(target_os = "linux")]
impl std::error::Error for QuarantineConflict {}

fn digest<T: Serialize>(value: &T) -> Result<String> {
    Ok(format!(
        "{:032x}",
        xxhash_rust::xxh3::xxh3_128(&serde_json::to_vec(value)?)
    ))
}

#[cfg(target_os = "linux")]
fn control_file(path: &Path) -> Result<ControlFile> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::OpenOptionsExt;

    const LIMIT: u64 = 16 * 1024 * 1024;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("metadata has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("metadata has no name"))?;
    let dir = match open_absolute_dir(parent) {
        Ok(dir) => dir,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let path = std::path::PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd())).join(name);
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= LIMIT,
        "unsupported metadata file"
    );
    let mut bytes = Vec::new();
    file.by_ref()
        .take(LIMIT.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(
        u64::try_from(bytes.len())? <= LIMIT,
        "metadata file is too large"
    );
    let after = file.metadata()?;
    ensure!(
        meta.dev() == after.dev()
            && meta.ino() == after.ino()
            && meta.mode() == after.mode()
            && meta.len() == after.len()
            && meta.mtime() == after.mtime()
            && meta.mtime_nsec() == after.mtime_nsec(),
        "metadata changed while being read"
    );
    Ok(Some((bytes, (meta.mode(), true, false))))
}

#[cfg(not(target_os = "linux"))]
fn control_file(_path: &Path) -> Result<ControlFile> {
    anyhow::bail!("reviewed forget requires protected metadata reads on Linux")
}

#[cfg(target_os = "linux")]
fn file_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    Ok(control_file(path)?.map(|(bytes, _)| bytes))
}

#[cfg(target_os = "linux")]
fn file_metadata(path: &Path) -> Result<Option<(u32, bool, bool)>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("metadata has no parent"))?;
    let name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("metadata has no name"))?;
    let dir = match open_absolute_dir(parent) {
        Ok(dir) => dir,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let anchored =
        std::path::PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd())).join(name);
    match std::fs::symlink_metadata(anchored) {
        Ok(meta) => Ok(Some((meta.mode(), meta.is_file(), meta.is_symlink()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(target_os = "linux")]
fn edit_config(
    bytes: Option<&[u8]>,
    before: &crate::repo_config::RepoConfig,
    after: &crate::repo_config::RepoConfig,
) -> Result<String> {
    let text = std::str::from_utf8(bytes.unwrap_or_default())?;
    let mut doc = text.parse::<toml_edit::DocumentMut>()?;
    if let Some(modes) = doc
        .get_mut("modes")
        .and_then(toml_edit::Item::as_table_like_mut)
    {
        for key in before
            .modes
            .keys()
            .filter(|key| !after.modes.contains_key(*key))
        {
            modes.remove(key);
        }
    }
    if before.ignore != after.ignore {
        if !doc.contains_key("ignore") {
            doc.as_table_mut()
                .insert("ignore", toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let ignore = doc
            .get_mut("ignore")
            .and_then(toml_edit::Item::as_table_like_mut)
            .ok_or_else(|| anyhow::anyhow!("ignore is not a table"))?;
        if let Some(array) = ignore
            .get_mut("patterns")
            .and_then(toml_edit::Item::as_array_mut)
        {
            for pattern in after
                .ignore
                .patterns
                .iter()
                .skip(before.ignore.patterns.len())
            {
                array.push(pattern.as_str());
            }
        } else {
            let mut array = toml_edit::Array::new();
            for pattern in &after.ignore.patterns {
                array.push(pattern.as_str());
            }
            ignore.insert("patterns", toml_edit::value(array));
        }
    }
    let result = doc.to_string();
    ensure!(
        crate::repo_config::RepoConfig::parse(&result)? == *after,
        "cannot preserve repository configuration"
    );
    Ok(result)
}

type ControlFile = Option<(Vec<u8>, (u32, bool, bool))>;

struct Controls {
    #[cfg(target_os = "linux")]
    config: crate::repo_config::RepoConfig,
    #[cfg(target_os = "linux")]
    state: crate::state::State,
    #[cfg(target_os = "linux")]
    config_file: ControlFile,
    #[cfg(target_os = "linux")]
    state_file: ControlFile,
    #[cfg(target_os = "linux")]
    index_file: ControlFile,
    #[cfg(target_os = "linux")]
    head: String,
}

fn head_digest(ctx: &Context) -> Result<String> {
    let head = control_file(&ctx.paths.repo.join(".git/HEAD"))?
        .ok_or_else(|| anyhow::anyhow!("HEAD is missing"))?;
    let text = std::str::from_utf8(&head.0)?.trim();
    let reference = if let Some(reference) = text.strip_prefix("ref: ") {
        ensure!(
            reference.starts_with("refs/heads/")
                && reference.split('/').all(|part| !part.is_empty()
                    && part != "."
                    && part != ".."
                    && part
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
            "HEAD reference is unsupported"
        );
        control_file(&ctx.paths.repo.join(".git").join(reference))?
    } else {
        ensure!(
            matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit()),
            "HEAD is unsupported"
        );
        None
    };
    let packed = control_file(&ctx.paths.repo.join(".git/packed-refs"))?;
    digest(&(head, reference, packed))
}

pub fn review_forget(ctx: &Context, key: &str) -> Result<ForgetReview> {
    Ok(review_pinned(ctx, key)?.0)
}

fn review_pinned(ctx: &Context, key: &str) -> Result<(ForgetReview, Controls)> {
    use std::os::unix::ffi::OsStrExt;
    check_backup_location(ctx)?;
    let id = control_file(&ctx.paths.repo.join(".git/dotman-id"))?
        .ok_or_else(|| anyhow::anyhow!("repository identity is missing"))?;
    let repo_id = std::str::from_utf8(&id.0)?.trim().to_owned();
    ensure!(!repo_id.is_empty(), "repository identity is empty");
    let config_file = control_file(&ctx.repo_config_file())?;
    let state_file = control_file(&ctx.paths.state_file())?;
    let machine_file = control_file(&ctx.paths.config_file)?;
    let index_file = control_file(&ctx.paths.repo.join(".git/index"))?;
    let config = match &config_file {
        Some((bytes, _)) => crate::repo_config::RepoConfig::parse(std::str::from_utf8(bytes)?)?,
        None => crate::repo_config::RepoConfig::default(),
    };
    let state = match &state_file {
        Some((bytes, _)) => {
            let state = crate::state::State::parse_bytes(&ctx.paths.state_file(), bytes)?;
            ensure!(
                state.repo_id == repo_id,
                "state belongs to another repository"
            );
            state
        }
        None => crate::state::State {
            repo_id: repo_id.clone(),
            capture_dirs: Some(std::collections::BTreeSet::new()),
            ..crate::state::State::default()
        },
    };
    let machine = match &machine_file {
        Some((bytes, _)) => {
            let (machine, unknown) = crate::config::Config::parse(std::str::from_utf8(bytes)?)?;
            ensure!(
                unknown.is_empty(),
                "machine config has unsupported settings"
            );
            machine
        }
        None => crate::config::Config::default(),
    };
    ensure!(
        machine == ctx.machine,
        "machine config changed; review again"
    );
    let sources = exact_inventory(ctx, key, &config)?;
    let home = match Stored::read(&ctx.paths.home, Path::new(key))? {
        Stored::Missing => None,
        stored => {
            let actual = tuple(&stored)?;
            Some(Source {
                path: key.to_owned(),
                kind: actual.kind,
                hash: actual.hash,
                mode: actual.mode,
            })
        }
    };
    let holds = source_holds(ctx, key, &sources)?;
    let paths = (
        ctx.paths.home.as_os_str().as_bytes(),
        ctx.paths.repo.as_os_str().as_bytes(),
        ctx.paths.state_dir.as_os_str().as_bytes(),
        ctx.paths.config_file.as_os_str().as_bytes(),
    );
    let head = head_digest(ctx)?;
    let context = digest(&(
        paths,
        &ctx.machine,
        ctx.identity(),
        &state,
        &config,
        &id,
        &machine_file,
        &config_file,
        &state_file,
        &index_file,
        &head,
    ))?;
    let review = ForgetReview {
        key: key.to_owned(),
        repo_id,
        sources,
        home,
        holds,
        context,
    };
    #[cfg(target_os = "linux")]
    let controls = Controls {
        config,
        state,
        config_file,
        state_file,
        index_file,
        head,
    };
    #[cfg(not(target_os = "linux"))]
    let controls = Controls {};
    Ok((review, controls))
}

fn source_holds(ctx: &Context, key: &str, sources: &[Source]) -> Result<Vec<(String, String)>> {
    use std::os::unix::ffi::OsStrExt;
    let mut holds = Vec::new();
    for source in sources {
        let stored = read_exact_source(ctx, &source.path)?;
        let actual = tuple(&stored)?;
        ensure!(
            actual.kind == source.kind && actual.hash == source.hash && actual.mode == source.mode,
            "source changed; review again"
        );
        let found = match stored {
            Stored::File { bytes, .. } => crate::secrets::held_name(&source.path)
                .or_else(|| crate::secrets::held(&bytes, key, &ctx.machine.guard.allow)),
            Stored::Link(target) => {
                crate::secrets::held_link(target.as_os_str().as_bytes(), &source.path)
            }
            Stored::Missing | Stored::Ciphertext => anyhow::bail!("source changed; review again"),
        };
        if let Some(finding) = found {
            holds.push((source.path.clone(), finding.to_string()));
        }
    }
    Ok(holds)
}

#[cfg(target_os = "linux")]
pub fn execute_forget(ctx: &Context, review: &ForgetReview) -> Result<ForgetReceipt> {
    execute_forget_inner(ctx, review, |_| Ok(()))
}

#[cfg(target_os = "linux")]
fn execute_forget_inner(
    ctx: &Context,
    review: &ForgetReview,
    mut after_move: impl FnMut(usize) -> Result<()>,
) -> Result<ForgetReceipt> {
    use std::collections::BTreeSet;

    let (actual, controls) = review_pinned(ctx, &review.key)?;
    ensure!(actual == *review, "review changed; review again");
    let original_config = controls.config;
    let original_state = controls.state;
    let (config_bytes, config_mode) = controls
        .config_file
        .map_or((None, None), |(b, m)| (Some(b), Some(m)));
    let (state_bytes, state_mode) = controls
        .state_file
        .map_or((None, None), |(b, m)| (Some(b), Some(m)));
    let index_path = ctx.paths.repo.join(".git/index");
    let index = controls.index_file;
    let head = controls.head;
    let keys = BTreeSet::from([review.key.clone()]);
    let grouped = crate::variant::Sources::group(review.sources.iter().map(|s| s.path.as_str()));
    let mut config = original_config.clone();
    let mut state = original_state.clone();
    super::remove_metadata(&mut config, &mut state, &keys, &grouped)?;
    let config_edit = if config == original_config {
        None
    } else {
        Some(edit_config(
            config_bytes.as_deref(),
            &original_config,
            &config,
        )?)
    };
    if config != original_config {
        safe_metadata_mode(config_mode, 0o644)?;
    }
    if state != original_state {
        safe_metadata_mode(state_mode, 0o600)?;
    }
    let copies = source_copies(ctx, &review.sources)?;
    let backup_root = ensure_private_backup_root(ctx)?;
    recheck_backup_root(ctx, &backup_root)?;
    let backup_run = next_backup_run(ctx)?;
    let backup_dir = create_backup_run(&backup_root, &backup_run)?;
    let backups = {
        use std::os::fd::AsRawFd;
        std::path::PathBuf::from(format!("/proc/self/fd/{}", backup_dir.as_raw_fd()))
    };
    let mut receipt = ForgetReceipt {
        backup_run: backup_run.clone(),
        quarantines: Vec::new(),
    };
    let mut quarantined = Vec::new();
    let operation = (|| -> Result<()> {
        for (source, bytes) in review.sources.iter().zip(copies) {
            recheck_backup_root(ctx, &backup_root)?;
            recheck_backup_run(&backup_root, &backup_run, &backup_dir)?;
            ensure_outside_repo(ctx, &backup_dir)?;
            write_reviewed_backup(ctx, &backup_dir, &source.path, &bytes)?;
        }
        recheck_backup_root(ctx, &backup_root)?;
        recheck_backup_run(&backup_root, &backup_run, &backup_dir)?;
        ensure!(
            review_forget(ctx, &review.key)? == *review,
            "review changed"
        );
        quarantine_batch(ctx, review, &mut receipt, &mut quarantined, &mut after_move)?;
        ensure!(
            file_bytes(&ctx.repo_config_file())? == config_bytes,
            "repo config changed"
        );
        ensure!(
            file_metadata(&ctx.repo_config_file())? == config_mode,
            "repo config mode changed"
        );
        ensure!(
            file_bytes(&ctx.paths.state_file())? == state_bytes,
            "state changed"
        );
        ensure!(
            file_metadata(&ctx.paths.state_file())? == state_mode,
            "state mode changed"
        );
        ensure!(control_file(&index_path)? == index, "index changed");
        ensure!(head_digest(ctx)? == head, "HEAD changed");
        if let Some(text) = &config_edit {
            crate::paths::write_atomic(&ctx.repo_config_file(), text.as_bytes(), 0o644)?;
        }
        if state != original_state {
            ensure!(
                file_bytes(&ctx.paths.state_file())? == state_bytes,
                "state changed"
            );
            state.save(&ctx.paths.state_file(), false)?;
        }
        Ok(())
    })();
    finish_forget(
        operation.is_err(),
        receipt,
        &quarantined,
        &review.sources,
        &backups,
    )
}

#[cfg(target_os = "linux")]
fn next_backup_run(ctx: &Context) -> Result<String> {
    let path = crate::engine::write::backup_run_dir(
        &ctx.paths.state_dir.join("backups"),
        std::time::SystemTime::now(),
    )?;
    Ok(path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| anyhow::anyhow!("invalid backup run"))?
        .to_owned())
}

#[cfg(target_os = "linux")]
fn finish_forget(
    failed: bool,
    mut receipt: ForgetReceipt,
    quarantined: &[std::path::PathBuf],
    sources: &[Source],
    backups: &Path,
) -> Result<ForgetReceipt> {
    for path in quarantined {
        if let Some(name) = path
            .parent()
            .and_then(Path::file_name)
            .and_then(std::ffi::OsStr::to_str)
        {
            receipt.quarantines.push(name.to_owned());
        }
    }
    if failed || clear_quarantine(quarantined, sources, backups).is_err() {
        return Err(ForgetPartial { receipt }.into());
    }
    receipt.quarantines.clear();
    Ok(receipt)
}

#[cfg(target_os = "linux")]
fn quarantine_batch(
    ctx: &Context,
    review: &ForgetReview,
    receipt: &mut ForgetReceipt,
    quarantined: &mut Vec<std::path::PathBuf>,
    after_move: &mut impl FnMut(usize) -> Result<()>,
) -> Result<()> {
    for (index, source) in review.sources.iter().enumerate() {
        match quarantine(ctx, &review.key, source) {
            Ok(path) => quarantined.push(path),
            Err(error) => {
                if let Some(conflict) = error.downcast_ref::<QuarantineConflict>() {
                    receipt.quarantines.push(conflict.directory.clone());
                }
                return Err(error);
            }
        }
        after_move(index)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn source_copies(ctx: &Context, sources: &[Source]) -> Result<Vec<Vec<u8>>> {
    sources
        .iter()
        .map(|source| {
            let Stored::File { bytes, mode } = read_exact_source(ctx, &source.path)? else {
                anyhow::bail!("reviewed forget does not yet support symlinks");
            };
            let actual = Tuple::of_file(&bytes, mode);
            ensure!(
                actual.kind == source.kind
                    && actual.hash == source.hash
                    && actual.mode == source.mode,
                "source changed; review again"
            );
            Ok(bytes)
        })
        .collect()
}

#[cfg(target_os = "linux")]
pub(super) fn clear_quarantine(
    paths: &[std::path::PathBuf],
    sources: &[Source],
    backups: &Path,
) -> Result<()> {
    use anyhow::Context as _;
    for (path, source) in paths.iter().zip(sources) {
        let parent = path.parent().context("quarantine has no parent")?;
        let actual = tuple(&Stored::read(parent, Path::new("source"))?)?;
        ensure!(
            actual.kind == source.kind && actual.hash == source.hash && actual.mode == source.mode,
            "quarantined source changed; recover it at {} (backup at {})",
            path.display(),
            backups.display()
        );
    }
    for path in paths {
        std::fs::remove_file(path)
            .with_context(|| format!("remove quarantine at {}", path.display()))?;
        if let Some(parent) = path.parent() {
            std::fs::remove_dir(parent)
                .with_context(|| format!("remove empty quarantine at {}", parent.display()))?;
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn safe_metadata_mode(meta: Option<(u32, bool, bool)>, expected: u32) -> Result<()> {
    if let Some((mode, regular, link)) = meta {
        ensure!(
            regular && !link && mode & 0o7777 == expected,
            "metadata has an unsupported kind or mode; nothing was forgotten"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn ensure_private_backup_root(ctx: &Context) -> Result<std::fs::File> {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;

    check_backup_location(ctx)?;
    let state = open_absolute_dir(&ctx.paths.state_dir)?;
    ensure_outside_repo(ctx, &state)?;
    let meta = state.metadata()?;
    ensure!(
        meta.uid() == crate::sys::geteuid() && meta.mode() & 0o7777 == 0o700,
        "state directory is not private"
    );
    let created = match crate::sys::mkdirat(state.as_fd(), c"backups", 0o700) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error.into()),
    };
    let backups = std::fs::File::from(crate::sys::openat_dir(state.as_fd(), c"backups")?);
    ensure_outside_repo(ctx, &backups)?;
    if created {
        use std::os::unix::fs::PermissionsExt;
        backups.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    }
    let meta = backups.metadata()?;
    ensure!(
        meta.uid() == crate::sys::geteuid() && meta.mode() & 0o7777 == 0o700,
        "backup directory is not private"
    );
    Ok(backups)
}

#[cfg(target_os = "linux")]
fn check_backup_location(ctx: &Context) -> Result<()> {
    for path in [&ctx.paths.state_dir, &ctx.paths.state_dir.join("backups")] {
        let mut existing = path.as_path();
        loop {
            match open_absolute_dir(existing) {
                Ok(dir) => {
                    ensure_outside_repo(ctx, &dir)?;
                    break;
                }
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
                {
                    existing = existing
                        .parent()
                        .ok_or_else(|| anyhow::anyhow!("missing backup ancestor"))?;
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn check_backup_location(_ctx: &Context) -> Result<()> {
    anyhow::bail!("reviewed forget requires Linux")
}

#[cfg(target_os = "linux")]
fn mount_id(file: &std::fs::File) -> Result<u64> {
    use std::os::fd::AsRawFd;
    let text = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .ok_or_else(|| anyhow::anyhow!("cannot identify directory mount"))?;
    Ok(value.trim().parse()?)
}

#[cfg(target_os = "linux")]
fn ensure_outside_repo(ctx: &Context, directory: &std::fs::File) -> Result<()> {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    let repo = open_absolute_dir(&ctx.paths.repo)?;
    let repo_meta = repo.metadata()?;
    let target_meta = directory.metadata()?;
    ensure!(
        target_meta.dev() != repo_meta.dev() || mount_id(directory)? == mount_id(&repo)?,
        "backup location may alias the repository"
    );
    let mut current = directory.try_clone()?;
    for _ in 0..1024 {
        let meta = current.metadata()?;
        ensure!(
            meta.dev() != repo_meta.dev() || meta.ino() != repo_meta.ino(),
            "backup location is inside the repository"
        );
        let parent = std::fs::File::from(crate::sys::openat_dir(current.as_fd(), c"..")?);
        let parent_meta = parent.metadata()?;
        if meta.dev() == parent_meta.dev() && meta.ino() == parent_meta.ino() {
            return Ok(());
        }
        current = parent;
    }
    anyhow::bail!("backup ancestry is too deep")
}

#[cfg(target_os = "linux")]
fn create_backup_run(root: &std::fs::File, name: &str) -> Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::AsFd;
    use std::os::unix::fs::PermissionsExt;
    let name = CString::new(name)?;
    crate::sys::mkdirat(root.as_fd(), &name, 0o700)?;
    let dir = std::fs::File::from(crate::sys::openat_dir(root.as_fd(), &name)?);
    dir.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    Ok(dir)
}

#[cfg(target_os = "linux")]
fn recheck_backup_root(ctx: &Context, root: &std::fs::File) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    check_backup_location(ctx)?;
    let current = open_absolute_dir(&ctx.paths.state_dir.join("backups"))?.metadata()?;
    let expected = root.metadata()?;
    ensure!(
        current.dev() == expected.dev() && current.ino() == expected.ino(),
        "backup location changed"
    );
    ensure_outside_repo(ctx, root)
}

#[cfg(target_os = "linux")]
fn recheck_backup_run(root: &std::fs::File, name: &str, run: &std::fs::File) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    let current = std::fs::File::from(crate::sys::openat_dir(root.as_fd(), &CString::new(name)?)?)
        .metadata()?;
    let expected = run.metadata()?;
    ensure!(
        current.dev() == expected.dev() && current.ino() == expected.ino(),
        "backup run changed"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn write_reviewed_backup(
    ctx: &Context,
    root: &std::fs::File,
    source: &str,
    bytes: &[u8],
) -> Result<()> {
    use std::ffi::CString;
    use std::io::Write;
    use std::os::fd::AsFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Component;

    let path = Path::new(".dotman/repo").join(source);
    let mut parts = path.components().peekable();
    let mut directory = root.try_clone()?;
    while let Some(Component::Normal(part)) = parts.next() {
        let name = CString::new(part.as_bytes())?;
        ensure_outside_repo(ctx, &directory)?;
        if parts.peek().is_none() {
            let mut file = crate::sys::create_excl(directory.as_fd(), &name, 0o600)?;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(bytes)?;
            file.sync_all()?;
            directory.sync_all()?;
            return Ok(());
        }
        let created = match crate::sys::mkdirat(directory.as_fd(), &name, 0o700) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(error.into()),
        };
        directory = std::fs::File::from(crate::sys::openat_dir(directory.as_fd(), &name)?);
        if created {
            directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        }
        let metadata = directory.metadata()?;
        ensure!(
            metadata.uid() == crate::sys::geteuid() && metadata.mode() & 0o7777 == 0o700,
            "backup directory is not private"
        );
    }
    anyhow::bail!("invalid backup source")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub(crate) path: String,
    pub(crate) kind: Kind,
    pub(crate) hash: String,
    pub(crate) mode: u32,
}

fn tuple(stored: &Stored) -> Result<Tuple> {
    match stored {
        Stored::File { bytes, mode } => Ok(Tuple::of_file(bytes, *mode)),
        Stored::Link(target) => Ok(Tuple::of_link(target)),
        Stored::Missing | Stored::Ciphertext => anyhow::bail!("source is not present"),
    }
}

#[cfg(target_os = "linux")]
fn read_exact_source(ctx: &Context, source: &str) -> Result<Stored> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let (parent, name) = source_parent(ctx, source)?;
    let root = std::path::PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    Stored::read(
        &root,
        Path::new(std::ffi::OsStr::from_bytes(name.to_bytes())),
    )
}

#[cfg(not(target_os = "linux"))]
fn read_exact_source(_ctx: &Context, _source: &str) -> Result<Stored> {
    anyhow::bail!("reviewed forget requires Linux")
}

#[cfg(target_os = "linux")]
fn exact_inventory(
    ctx: &Context,
    key: &str,
    config: &crate::repo_config::RepoConfig,
) -> Result<Vec<Source>> {
    use std::os::fd::AsRawFd;
    ctx.check_key(key, true)?;
    let (parent, _) = source_parent(ctx, key)?;
    let anchor = std::path::PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    let prefix = Path::new(key).parent().unwrap_or_else(|| Path::new(""));
    let ignore = crate::commands::add::Ignore::compile(&config.ignore.patterns)?;
    let mut sources = Vec::new();
    for item in std::fs::read_dir(anchor)? {
        let item = item?;
        let name = item.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("source directory contains an unsupported name"))?;
        let path = prefix.join(name);
        let path = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("source path is not UTF-8"))?;
        if !crate::variant::Sources::group([path]).contains(key) {
            continue;
        }
        let kind = item.file_type()?;
        if kind.is_dir() {
            continue;
        }
        ensure!(
            kind.is_file() || kind.is_symlink(),
            "source is not a file or symlink"
        );
        ensure!(
            !name.starts_with(crate::context::TEMP_PREFIX),
            "temporary source is unsupported"
        );
        ensure!(
            !ignore.matches(path, false)
                && !crate::engine::parents(path).any(|dir| ignore.matches(dir, true)),
            "ignored source requires a separate review"
        );
        ctx.check_key(path, true)?;
        let observed = tuple(&read_exact_source(ctx, path)?)?;
        sources.push(Source {
            path: path.to_owned(),
            kind: observed.kind,
            hash: observed.hash,
            mode: observed.mode,
        });
    }
    ensure!(!sources.is_empty(), "{key} is not a managed source");
    sources.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(sources)
}

#[cfg(not(target_os = "linux"))]
fn exact_inventory(
    _ctx: &Context,
    _key: &str,
    _config: &crate::repo_config::RepoConfig,
) -> Result<Vec<Source>> {
    anyhow::bail!("reviewed forget requires Linux")
}

#[cfg(target_os = "linux")]
fn source_parent(ctx: &Context, source: &str) -> Result<(std::fs::File, std::ffi::CString)> {
    use std::ffi::CString;
    use std::os::fd::AsFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Component;

    ctx.check_key(source, true)?;
    let mut parts = Path::new(source).components().peekable();
    let mut parent = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&ctx.paths.repo)?;
    loop {
        let Some(Component::Normal(part)) = parts.next() else {
            anyhow::bail!("source path is not representable");
        };
        let name = CString::new(part.as_bytes())?;
        if parts.peek().is_none() {
            return Ok((parent, name));
        }
        parent = crate::sys::openat_dir(parent.as_fd(), &name)?.into();
    }
}

#[cfg(target_os = "linux")]
fn open_absolute_dir(path: &Path) -> Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::AsFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Component;

    ensure!(path.is_absolute(), "recovery directory must be absolute");
    let mut dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    for part in path.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => {
                dir = crate::sys::openat_dir(dir.as_fd(), &CString::new(name.as_bytes())?)?.into();
            }
            _ => anyhow::bail!("recovery directory is not normalized"),
        }
    }
    Ok(dir)
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryLocation {
    device: u64,
    inode: u64,
    source_device: u64,
    source_inode: u64,
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct RecoveryMovePartial {
    pub recovery: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
impl std::fmt::Display for RecoveryMovePartial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "source could not be restored; recover it from {}",
            self.recovery.display()
        )
    }
}

#[cfg(target_os = "linux")]
impl std::error::Error for RecoveryMovePartial {}

#[cfg(target_os = "linux")]
pub fn recovery_preflight(ctx: &Context, source: &Source) -> Result<RecoveryLocation> {
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let state = open_absolute_dir(&ctx.paths.state_dir)?;
    let backup = std::fs::File::from(crate::sys::openat_dir(state.as_fd(), c"backups")?);
    for metadata in [state.metadata()?, backup.metadata()?] {
        ensure!(
            metadata.uid() == crate::sys::geteuid() && metadata.mode() & 0o7777 == 0o700,
            "recovery directories must be private and owned by this user"
        );
    }
    ensure!(
        !std::fs::canonicalize(&ctx.paths.state_dir)?
            .starts_with(std::fs::canonicalize(&ctx.paths.repo)?),
        "recovery directory must be outside the repository"
    );
    let (parent, leaf) = source_parent(ctx, &source.path)?;
    let path = format!("/proc/self/fd/{}", parent.as_raw_fd());
    let path = Path::new(&path).join(std::ffi::OsStr::from_bytes(leaf.to_bytes()));
    let metadata = std::fs::symlink_metadata(path)?;
    let backup_meta = backup.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == crate::sys::geteuid()
            && metadata.nlink() == 1
            && metadata.dev() == backup_meta.dev(),
        "recovery requires an owned regular file with one link on the backup filesystem"
    );
    Ok(RecoveryLocation {
        device: backup_meta.dev(),
        inode: backup_meta.ino(),
        source_device: metadata.dev(),
        source_inode: metadata.ino(),
    })
}

#[cfg(target_os = "linux")]
pub fn move_to_recovery(
    ctx: &Context,
    key: &str,
    source: &Source,
    expected: &RecoveryLocation,
) -> Result<std::path::PathBuf> {
    move_to_recovery_inner(ctx, key, source, expected, || {})
}

#[cfg(target_os = "linux")]
fn move_to_recovery_inner(
    ctx: &Context,
    key: &str,
    source: &Source,
    expected: &RecoveryLocation,
    after_move: impl FnOnce(),
) -> Result<std::path::PathBuf> {
    use anyhow::Context as _;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    ctx.check_key(key, true)?;
    ensure!(
        crate::variant::Sources::group([source.path.as_str()]).contains(key),
        "source does not belong to the reviewed key"
    );
    ensure!(
        recovery_preflight(ctx, source)? == *expected,
        "recovery location changed"
    );
    let state = open_absolute_dir(&ctx.paths.state_dir)?;
    let state_meta = state.metadata()?;
    ensure!(
        state_meta.uid() == crate::sys::geteuid() && state_meta.mode() & 0o7777 == 0o700,
        "recovery location changed"
    );
    let backup = std::fs::File::from(crate::sys::openat_dir(state.as_fd(), c"backups")?);
    let metadata = backup.metadata()?;
    ensure!(
        metadata.dev() == expected.device
            && metadata.ino() == expected.inode
            && metadata.uid() == crate::sys::geteuid()
            && metadata.mode() & 0o7777 == 0o700,
        "recovery location changed"
    );
    let (parent, leaf) = source_parent(ctx, &source.path)?;
    let anchor = format!("/proc/self/fd/{}", backup.as_raw_fd());
    let temp = tempfile::Builder::new()
        .prefix("recovered-")
        .tempdir_in(anchor)?;
    let name = temp
        .path()
        .file_name()
        .context("recovery directory has no name")?
        .to_owned();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700))?;
    let dir = std::fs::File::from(crate::sys::openat_dir(
        backup.as_fd(),
        &std::ffi::CString::new(name.as_bytes())?,
    )?);
    let display = ctx
        .paths
        .state_dir
        .join("backups")
        .join(name)
        .join("source");
    let held_path = temp.keep();
    if let Err(error) = crate::sys::rename_noreplace(parent.as_fd(), &leaf, dir.as_fd(), c"source")
    {
        std::fs::remove_dir(&held_path)?;
        return Err(error).context("cannot move source to recovery");
    }
    after_move();
    let moved_meta = std::fs::symlink_metadata(held_path.join("source"));
    let private = moved_meta.as_ref().is_ok_and(|meta| {
        meta.is_file()
            && meta.uid() == crate::sys::geteuid()
            && meta.nlink() == 1
            && meta.dev() == expected.source_device
            && meta.ino() == expected.source_inode
    });
    let observed = Stored::read(&held_path, Path::new("source")).and_then(|value| tuple(&value));
    if !private
        || !observed.as_ref().is_ok_and(|actual| {
            actual.kind == source.kind && actual.hash == source.hash && actual.mode == source.mode
        })
    {
        if crate::sys::rename_noreplace(dir.as_fd(), c"source", parent.as_fd(), &leaf).is_err() {
            return Err(RecoveryMovePartial { recovery: display }.into());
        }
        std::fs::remove_dir(&held_path)?;
        anyhow::bail!("source changed; restored without overwriting");
    }
    Ok(display)
}

pub fn inventory(ctx: &Context, key: &str) -> Result<Vec<Source>> {
    ctx.check_key(key, true)?;
    let sources = ctx.sources()?;
    let mut entries = Vec::new();
    for path in sources.sources_of(key) {
        let observed = tuple(&Stored::read(&ctx.paths.repo, Path::new(path))?)?;
        entries.push(Source {
            path: path.to_owned(),
            kind: observed.kind,
            hash: observed.hash,
            mode: observed.mode,
        });
    }
    ensure!(!entries.is_empty(), "{key} is not a managed source");
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

#[cfg(target_os = "linux")]
pub fn quarantine(ctx: &Context, key: &str, source: &Source) -> Result<std::path::PathBuf> {
    quarantine_inner(ctx, key, source, || {})
}

#[cfg(target_os = "linux")]
fn quarantine_inner(
    ctx: &Context,
    key: &str,
    source: &Source,
    after_move: impl FnOnce(),
) -> Result<std::path::PathBuf> {
    use anyhow::Context as _;
    use std::fs::OpenOptions;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::fs::OpenOptionsExt;

    ctx.check_key(key, true)?;
    let grouped = crate::variant::Sources::group([source.path.as_str()]);
    ensure!(
        grouped.contains(key),
        "source does not belong to the reviewed key"
    );
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&ctx.paths.repo)?;
    let git_dir = crate::sys::openat_dir(root.as_fd(), c".git")?;
    let (parent, leaf) = source_parent(ctx, &source.path)?;

    let anchor = std::path::PathBuf::from(format!("/proc/self/fd/{}", git_dir.as_raw_fd()));
    let temp = tempfile::Builder::new()
        .prefix(".dotman-forget-")
        .tempdir_in(&anchor)?;
    let name = temp
        .path()
        .file_name()
        .context("quarantine directory has no name")?
        .to_owned();
    let display = ctx.paths.repo.join(".git").join(&name);
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(temp.path())?;
    let temp_path = temp.keep();
    let item = c"source";
    if let Err(error) = crate::sys::rename_noreplace(parent.as_fd(), &leaf, dir.as_fd(), item) {
        std::fs::remove_dir(&temp_path)?;
        return Err(error).context("cannot quarantine reviewed source");
    }
    after_move();
    let observed = Stored::read(&temp_path, Path::new("source")).and_then(|value| tuple(&value));
    let matches = observed.as_ref().is_ok_and(|actual| {
        actual.kind == source.kind && actual.hash == source.hash && actual.mode == source.mode
    });
    if !matches {
        if crate::sys::rename_noreplace(dir.as_fd(), item, parent.as_fd(), &leaf).is_err() {
            let directory = name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid quarantine directory"))?
                .to_owned();
            return Err(QuarantineConflict {
                directory,
                path: display.join("source"),
            }
            .into());
        }
        std::fs::remove_dir(&temp_path)?;
        anyhow::bail!("source changed during review; restored without overwriting");
    }
    Ok(display.join("source"))
}
