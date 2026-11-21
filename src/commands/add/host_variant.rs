use super::{node_hold, record_ahead, stored_mode};
use crate::commands::apply::{self, Classified};
use crate::commands::forget::reviewed::{self, Source};
use crate::commands::review::Stored;
use crate::context::Context;
#[cfg(target_os = "linux")]
use crate::engine::safe_mode;
use crate::engine::status::{Row, Variants, Worktree};
use crate::engine::{Tuple, parents, read_node};
use crate::secrets;
use crate::state::{Entry, IdentitySnapshot};
use crate::variant::{self, Flag, Selection, Sources};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub key: String,
    pub target: String,
    pub selected: String,
    pub identity: IdentitySnapshot,
    pub repo_id: String,
    home_hash: String,
    pub home_mode: u32,
    variants: Vec<Source>,
    policy_hash: String,
    git_hash: String,
    parents: Vec<(String, u64, u64, u32)>,
    pub recipients: Vec<String>,
    pub encrypted: bool,
    pub config_path: std::path::PathBuf,
    pub config_before: ConfigObservation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfigObservation {
    Missing,
    File {
        hash: String,
        mode: u32,
        length: u64,
        device: u64,
        inode: u64,
    },
}

#[derive(Debug)]
pub struct TerminalOnly;

impl std::fmt::Display for TerminalOnly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("mode metadata requires a settings change; this variant is terminal-only")
    }
}

impl std::error::Error for TerminalOnly {}

fn git_snapshot(ctx: &Context, selected: &str, target: &str) -> Result<String> {
    let git = ctx.git.staging_review(&[], &[])?;
    ensure!(
        git.head.contains_key(Path::new(selected)) && git.index.contains_key(Path::new(selected)),
        "source is not tracked"
    );
    ensure!(
        !git.head.contains_key(Path::new(target)) && !git.index.contains_key(Path::new(target)),
        "target exists in git"
    );
    let describe = |entries: &std::collections::BTreeMap<std::path::PathBuf, crate::git::Entry>| {
        entries
            .iter()
            .map(|(path, entry)| {
                (
                    path.as_os_str().as_bytes().to_vec(),
                    entry.mode.clone(),
                    entry.oid.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    Ok(Tuple::of_file(
        &serde_json::to_vec(&(
            ctx.git.head()?,
            describe(&git.head),
            describe(&git.index),
            git.filemode,
        ))?,
        0,
    )
    .hash)
}

fn snapshot(ctx: &Context, key: &str, host: &str) -> Result<(Review, Vec<u8>)> {
    ctx.check_key(key, true)?;
    let identity = crate::identity::current(&ctx.machine);
    ensure!(
        identity == *ctx.identity() && identity.host == host,
        "machine identity changed"
    );
    let repo_id = ctx.repo_id()?;
    let (config_before, config) = config_observation(ctx)?;
    let state = ctx.state(&repo_id)?;
    let sources = ctx.sources()?;
    let variants = Variants::new(
        Worktree::new(&ctx.paths.repo, &config.modes),
        &sources,
        ctx.identity(),
        ctx.template_vars(),
        ctx.age(),
    );
    let Classified::Item(item) = apply::classify_key(ctx, &variants, &state, key)? else {
        anyhow::bail!("file cannot be selected");
    };
    ensure!(
        matches!(item.row(), Row::ChangedLocally | Row::Clean { .. }) && !item.identity_changed(),
        "file has an unsupported status"
    );
    let (selected, _, encrypted, template) = item
        .source_metadata()
        .ok_or_else(|| anyhow::anyhow!("file has no selected source"))?;
    ensure!(!template, "templates cannot be captured as host variants");
    ensure!(
        state
            .files
            .get(key)
            .is_some_and(|entry| entry.source == selected),
        "recorded source differs"
    );
    let Stored::File {
        bytes: source_bytes,
        ..
    } = Stored::read(&ctx.paths.repo, Path::new(selected))?
    else {
        anyhow::bail!("source is not regular");
    };
    ensure!(
        encrypted || secrets::held(&source_bytes, key, &ctx.machine.guard.allow).is_none(),
        "selected source is guarded"
    );
    let inventory = reviewed::inventory(ctx, key)?;
    let target = target(ctx, key, host, encrypted, &sources, &identity)?;
    let git_hash = git_snapshot(ctx, selected, &target)?;
    let node = read_node(&ctx.paths.home, key)?;
    ensure!(
        node_hold(ctx, &node, key, &target, encrypted).is_none(),
        "content guard refused the file"
    );
    let Stored::File { bytes, mode } = Stored::read(&ctx.paths.home, Path::new(key))? else {
        anyhow::bail!("home file is not regular");
    };
    ensure!(
        encrypted || secrets::held(&bytes, key, &ctx.machine.guard.allow).is_none(),
        "content guard refused the file"
    );
    ensure!(
        ctx.keys().alias(&ctx.paths.home, key, true).is_none(),
        "home file aliases dotman's files"
    );
    let (recipients, record_binding) = recipients(ctx, &config, encrypted)?;
    let directories = directory_snapshot(ctx, key)?;
    let machine = machine_snapshot(ctx)?;
    check_modes(ctx, &config, key, &target, mode, encrypted)?;
    let policy_hash = Tuple::of_file(
        &serde_json::to_vec(&(&config, &state, &ctx.machine, machine, record_binding))?,
        0,
    )
    .hash;
    Ok((
        Review {
            key: key.to_owned(),
            target,
            selected: selected.to_owned(),
            identity,
            repo_id,
            home_hash: Tuple::of_file(&bytes, mode).hash,
            home_mode: mode,
            variants: inventory,
            policy_hash,
            git_hash,
            parents: directories,
            recipients,
            encrypted,
            config_path: ctx.repo_config_file(),
            config_before,
        },
        bytes,
    ))
}

fn config_observation(
    ctx: &Context,
) -> Result<(ConfigObservation, crate::repo_config::RepoConfig)> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    const MAX_CONFIG: u64 = 1_048_576;
    let path = ctx.repo_config_file();
    let before = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                ConfigObservation::Missing,
                crate::repo_config::RepoConfig::default(),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        before.is_file() && before.len() <= MAX_CONFIG,
        "repository config is not a bounded regular file"
    );
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    let opened = file.metadata()?;
    ensure!(
        opened.is_file()
            && before.dev() == opened.dev()
            && before.ino() == opened.ino()
            && opened.len() <= MAX_CONFIG,
        "repository config changed"
    );
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_CONFIG.saturating_add(1))
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let current = std::fs::symlink_metadata(&path)?;
    ensure!(
        after.is_file()
            && after.len() <= MAX_CONFIG
            && u64::try_from(bytes.len()).ok() == Some(after.len())
            && after.len() == opened.len()
            && after.dev() == opened.dev()
            && after.ino() == opened.ino()
            && current.dev() == opened.dev()
            && current.ino() == opened.ino()
            && after.mode() == opened.mode()
            && after.mtime() == opened.mtime()
            && after.mtime_nsec() == opened.mtime_nsec()
            && after.ctime() == opened.ctime()
            && after.ctime_nsec() == opened.ctime_nsec(),
        "repository config changed"
    );
    let config = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| crate::repo_config::RepoConfig::parse(text).ok())
        .ok_or_else(|| anyhow::anyhow!("repository config cannot be parsed"))?;
    let mode = after.mode() & 0o7777;
    Ok((
        ConfigObservation::File {
            hash: Tuple::of_file(&bytes, mode).hash,
            mode,
            length: after.len(),
            device: after.dev(),
            inode: after.ino(),
        },
        config,
    ))
}

fn check_modes(
    ctx: &Context,
    config: &crate::repo_config::RepoConfig,
    key: &str,
    target: &str,
    mode: u32,
    encrypted: bool,
) -> Result<()> {
    let mut planned = config.clone();
    let (_, remove_later) = record_ahead(
        &mut planned,
        &mut std::collections::BTreeMap::new(),
        &ctx.paths.home,
        key,
        target,
        Some(mode),
        encrypted,
    )?;
    let default = if encrypted { 0o600 } else { stored_mode(mode) };
    if planned.modes != config.modes
        || (remove_later && crate::engine::recorded_mode(&config.modes, target)? != Some(default))
    {
        return Err(TerminalOnly.into());
    }
    Ok(())
}

fn target(
    ctx: &Context,
    key: &str,
    host: &str,
    encrypted: bool,
    sources: &Sources,
    identity: &IdentitySnapshot,
) -> Result<String> {
    let target = variant::variant_source(key, &format!("host={host}"))?;
    let target = if encrypted {
        variant::with_age(&target)?
    } else {
        target
    };
    ctx.check_key(&target, true)?;
    let proposed = Sources::group([target.as_str()]);
    ensure!(
        variant::names_host(&target, host)
            && proposed.sources_of(key).eq([target.as_str()])
            && proposed.flag(key, &target) == if encrypted { Flag::Age } else { Flag::Plain },
        "target does not name this host and key"
    );
    let after = Sources::group(sources.sources_of(key).chain([target.as_str()]));
    ensure!(
        matches!(after.select(key, identity), Selection::Winner(winner) if winner == target),
        "host variant would not be selected"
    );
    ensure!(
        matches!(
            Stored::read(&ctx.paths.repo, Path::new(&target))?,
            Stored::Missing
        ),
        "target already exists"
    );
    Ok(target)
}

type RecipientBinding = (Vec<String>, Option<(Vec<u8>, u32)>);
fn recipients(
    ctx: &Context,
    config: &crate::repo_config::RepoConfig,
    encrypted: bool,
) -> Result<RecipientBinding> {
    Ok(if encrypted {
        let Stored::File {
            bytes: record,
            mode,
        } = Stored::read(&ctx.paths.repo, Path::new(secrets::RECORD))?
        else {
            anyhow::bail!("recipient record is not regular");
        };
        let recipients = secrets::recipient_set(&config.age.recipients);
        ensure!(
            !recipients.is_empty()
                && secrets::recipient_set(
                    &std::str::from_utf8(&record)?
                        .lines()
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                ) == recipients,
            "recipient record differs"
        );
        (recipients, Some((record, mode)))
    } else {
        (Vec::new(), None)
    })
}

fn directory_snapshot(ctx: &Context, key: &str) -> Result<Vec<(String, u64, u64, u32)>> {
    let mut directories = Vec::new();
    for root in [&ctx.paths.home, &ctx.paths.repo] {
        for parent in parents(key) {
            let meta = std::fs::symlink_metadata(root.join(parent))?;
            ensure!(meta.is_dir(), "parent is not a directory");
            directories.push((
                root.join(parent).to_string_lossy().into_owned(),
                meta.dev(),
                meta.ino(),
                meta.mode(),
            ));
        }
    }
    Ok(directories)
}

fn machine_snapshot(ctx: &Context) -> Result<Vec<u8>> {
    Ok(
        match Stored::read(Path::new("/"), ctx.paths.config_file.strip_prefix("/")?)? {
            Stored::Missing => {
                ensure!(
                    ctx.machine == crate::config::Config::default(),
                    "machine config changed"
                );
                Vec::new()
            }
            Stored::File { bytes, mode } => {
                let parsed = std::str::from_utf8(&bytes)
                    .ok()
                    .and_then(|text| crate::config::Config::parse(text).ok())
                    .ok_or_else(|| anyhow::anyhow!("machine config cannot be parsed"))?;
                ensure!(parsed.0 == ctx.machine, "machine config changed");
                serde_json::to_vec(&(bytes, mode))?
            }
            _ => anyhow::bail!("machine config is not regular"),
        },
    )
}

pub fn review(ctx: &Context, key: &str, host: &str) -> Result<Review> {
    snapshot(ctx, key, host).map(|(review, _)| review)
}

// The caller holds the process lock and saves the returned entry.
#[cfg(target_os = "linux")]
pub fn execute(ctx: &Context, review: &Review) -> Result<Entry> {
    use std::ffi::CString;
    use std::io::Write;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let (current, bytes) = snapshot(ctx, &review.key, &review.identity.host)?;
    ensure!(current == *review, "review is stale");
    let content = if review.encrypted {
        ctx.age().encrypt(&bytes, &review.recipients)?
    } else {
        bytes.clone()
    };
    ensure!(
        snapshot(ctx, &review.key, &review.identity.host)?.0 == *review,
        "review changed during preparation"
    );
    let path = Path::new(&review.target);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing parent"))?;
    let mut dir: std::os::fd::OwnedFd = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&ctx.paths.repo)?
        .into();
    for part in parent.components() {
        dir = crate::sys::openat_dir(dir.as_fd(), &CString::new(part.as_os_str().as_bytes())?)?;
    }
    let mut temporary =
        tempfile::NamedTempFile::new_in(format!("/proc/self/fd/{}", dir.as_raw_fd()))?;
    temporary.write_all(&content)?;
    temporary
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(if review.encrypted {
            0o644
        } else {
            stored_mode(review.home_mode)
        }))?;
    temporary.as_file().sync_all()?;
    ensure!(
        snapshot(ctx, &review.key, &review.identity.host)?.0 == *review,
        "review changed before publication"
    );
    let pinned = std::fs::File::from(dir.try_clone()?).metadata()?;
    let current = std::fs::symlink_metadata(ctx.paths.repo.join(parent))?;
    ensure!(
        current.is_dir() && current.dev() == pinned.dev() && current.ino() == pinned.ino(),
        "repository parent changed"
    );
    let temp = CString::new(
        temporary
            .path()
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("missing temporary name"))?
            .as_bytes(),
    )?;
    let leaf = CString::new(
        path.file_name()
            .ok_or_else(|| anyhow::anyhow!("missing target name"))?
            .as_bytes(),
    )?;
    let (observed, config) = config_observation(ctx)?;
    ensure!(
        observed == review.config_before,
        "repository config changed"
    );
    let entry = if review.encrypted {
        Tuple::of_file(&bytes, safe_mode(review.home_mode)).entry(&review.target)
    } else {
        let mode = crate::engine::recorded_mode(&config.modes, &review.target)?
            .unwrap_or_else(|| stored_mode(review.home_mode));
        super::captured_entry(
            &review.target,
            &Tuple::of_file(&bytes, mode),
            Some(review.home_mode),
        )
    };
    crate::sys::rename_noreplace(dir.as_fd(), &temp, dir.as_fd(), &leaf)?;
    Ok(entry)
}

#[cfg(not(target_os = "linux"))]
pub fn execute(_ctx: &Context, _review: &Review) -> Result<Entry> {
    anyhow::bail!("host variant publication is not supported on this platform")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn fixture(encrypted: bool) -> Result<(tempfile::TempDir, Context, String)> {
        let root = tempfile::tempdir()?;
        let home = root.path().join("home");
        let repo = root.path().join("repo");
        fs::create_dir_all(&home)?;
        fs::create_dir_all(&repo)?;
        let env = crate::paths::Env::from_fn(|name| match name {
            "HOME" => Some(home.clone().into_os_string()),
            "DOTMAN_REPO_PATH" => Some(repo.clone().into_os_string()),
            _ => None,
        });
        let paths = crate::paths::Paths::resolve(&env, crate::sys::geteuid())?;
        let ctx = Context::new(paths, crate::config::Config::default(), "test");
        let git = |args: &[&str]| -> Result<()> {
            let status = crate::proc::command("git")
                .args(args)
                .current_dir(&repo)
                .env("HOME", &home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .status()?;
            ensure!(status.success(), "git failed");
            Ok(())
        };
        git(&["init", "-q", "--template="])?;
        fs::write(repo.join(".git/dotman-id"), "fixture")?;
        let source = if encrypted {
            "settings{age}"
        } else {
            "settings"
        };
        if encrypted {
            fs::create_dir_all(
                ctx.paths
                    .age_identity
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("identity parent"))?,
            )?;
            let out = crate::proc::command("age-keygen")
                .arg("-o")
                .arg(&ctx.paths.age_identity)
                .output()?;
            ensure!(out.status.success(), "age-keygen failed");
            let out = crate::proc::command("age-keygen")
                .arg("-y")
                .arg(&ctx.paths.age_identity)
                .output()?;
            ensure!(out.status.success(), "age-keygen failed");
            let recipient = String::from_utf8(out.stdout)?.trim().to_owned();
            fs::create_dir_all(repo.join(".dotman"))?;
            fs::write(repo.join(secrets::RECORD), format!("{recipient}\n"))?;
            fs::write(
                repo.join(".dotman.toml"),
                format!("[age]\nrecipients = [\"{recipient}\"]\n"),
            )?;
            fs::write(repo.join(source), ctx.age().encrypt(b"base", &[recipient])?)?;
        } else {
            fs::write(repo.join(source), b"base")?;
            let target =
                variant::variant_source("settings", &format!("host={}", ctx.identity().host))?;
            fs::write(
                repo.join(".dotman.toml"),
                format!(
                    "# keep this comment\n# {}\n[modes]\n\"other\" = \"0640\" # keep this mode\n\"{target}\" = \"0600\"\n",
                    concat!("private", "-comment-marker")
                ),
            )?;
            fs::set_permissions(repo.join(".dotman.toml"), fs::Permissions::from_mode(0o600))?;
        }
        fs::write(home.join("settings"), b"local")?;
        fs::set_permissions(home.join("settings"), fs::Permissions::from_mode(0o600))?;
        git(&["add", "-A"])?;
        git(&["commit", "-qm", "base"])?;
        let mut state = ctx.state("fixture")?;
        state.files.insert(
            "settings".into(),
            Tuple::of_file(b"base", if encrypted { 0o600 } else { 0o644 }).entry(source),
        );
        state.identity = Some(ctx.identity().clone());
        state.save(&ctx.paths.state_file(), false)?;
        Ok((root, ctx, source.to_owned()))
    }

    #[test]
    fn plain_variant_preserves_base_and_other_host() -> Result<()> {
        let (_root, ctx, base) = fixture(false)?;
        let host = &ctx.identity().host;
        let before = fs::read(ctx.paths.repo.join(&base))?;
        let config_before = fs::read(ctx.repo_config_file())?;
        let inode = fs::metadata(ctx.repo_config_file())?.ino();
        let review = review(&ctx, "settings", host)?;
        assert!(!serde_json::to_string(&review)?.contains(concat!("private", "-comment-marker")));
        let entry = execute(&ctx, &review)?;
        assert_eq!(entry.source, review.target);
        assert_eq!(entry.mode, "0600");
        assert_eq!(fs::read(ctx.paths.repo.join(&review.target))?, b"local");
        assert_eq!(fs::read(ctx.paths.repo.join(&base))?, before);
        assert_eq!(fs::read(ctx.paths.home.join("settings"))?, b"local");
        assert_eq!(fs::read(ctx.repo_config_file())?, config_before);
        assert_eq!(fs::metadata(ctx.repo_config_file())?.ino(), inode);
        let config_text = fs::read_to_string(ctx.repo_config_file())?;
        assert!(config_text.contains("# keep this comment"));
        assert!(config_text.contains("\"other\" = \"0640\" # keep this mode"));
        assert_eq!(fs::metadata(ctx.repo_config_file())?.mode() & 0o777, 0o600);
        assert_eq!(
            ctx.repo_config()?
                .modes
                .get(&review.target)
                .map(String::as_str),
            Some("0600")
        );
        let mut other = ctx.identity().clone();
        other.host = "other-host".into();
        assert_eq!(
            ctx.sources()?.select("settings", &other).winner().copied(),
            Some(base.as_str())
        );
        Ok(())
    }

    #[test]
    fn stale_home_and_existing_target_are_refused() -> Result<()> {
        let (_root, ctx, _) = fixture(false)?;
        let review = review(&ctx, "settings", &ctx.identity().host)?;
        fs::write(ctx.paths.home.join("settings"), b"changed")?;
        let config = fs::read(ctx.repo_config_file())?;
        let base = fs::read(ctx.paths.repo.join("settings"))?;
        assert!(execute(&ctx, &review).is_err());
        assert_eq!(fs::read(ctx.repo_config_file())?, config);
        assert_eq!(fs::read(ctx.paths.repo.join("settings"))?, base);
        fs::write(ctx.paths.home.join("settings"), b"local")?;
        fs::write(ctx.paths.repo.join(&review.target), b"existing")?;
        assert!(execute(&ctx, &review).is_err());
        assert_eq!(fs::read(ctx.repo_config_file())?, config);
        assert_eq!(fs::read(ctx.paths.repo.join(&review.target))?, b"existing");
        Ok(())
    }

    #[test]
    fn stale_source_mode_index_and_machine_are_refused() -> Result<()> {
        let (_root, ctx, base) = fixture(false)?;
        let host = ctx.identity().host.clone();
        assert!(review(&ctx, "settings", "another-host").is_err());
        let review = review(&ctx, "settings", &host)?;
        let source = ctx.paths.repo.join(&base);
        fs::set_permissions(&source, fs::Permissions::from_mode(0o600))?;
        assert!(execute(&ctx, &review).is_err());
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644))?;
        fs::set_permissions(
            ctx.paths.home.join("settings"),
            fs::Permissions::from_mode(0o640),
        )?;
        assert!(execute(&ctx, &review).is_err());
        fs::set_permissions(
            ctx.paths.home.join("settings"),
            fs::Permissions::from_mode(0o600),
        )?;
        let status = crate::proc::command("git")
            .args(["rm", "--cached", "-q", "--", &base])
            .current_dir(&ctx.paths.repo)
            .status()?;
        assert!(status.success());
        assert!(execute(&ctx, &review).is_err());
        assert!(!ctx.paths.repo.join(&review.target).exists());
        Ok(())
    }

    #[test]
    fn raw_config_change_is_stale_even_if_settings_are_unchanged() -> Result<()> {
        let (_root, ctx, _) = fixture(false)?;
        let review = review(&ctx, "settings", &ctx.identity().host)?;
        let path = ctx.repo_config_file();
        let mut config = fs::read(&path)?;
        config.extend_from_slice(b"# another comment\n");
        fs::write(&path, &config)?;
        assert!(execute(&ctx, &review).is_err());
        assert_eq!(fs::read(path)?, config);
        assert_eq!(fs::read(ctx.paths.repo.join("settings"))?, b"base");
        Ok(())
    }

    #[test]
    fn same_config_bytes_on_a_new_inode_are_stale() -> Result<()> {
        let (_root, ctx, _) = fixture(false)?;
        let review = review(&ctx, "settings", &ctx.identity().host)?;
        let path = ctx.repo_config_file();
        let before = fs::read(&path)?;
        let replacement = ctx.paths.repo.join("replacement");
        fs::write(&replacement, &before)?;
        fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600))?;
        fs::rename(&replacement, &path)?;
        assert!(execute(&ctx, &review).is_err());
        assert_eq!(fs::read(path)?, before);
        assert!(!ctx.paths.repo.join(&review.target).exists());
        Ok(())
    }

    #[test]
    fn default_mode_does_not_rewrite_config() -> Result<()> {
        let (_root, ctx, _) = fixture(false)?;
        fs::set_permissions(
            ctx.paths.home.join("settings"),
            fs::Permissions::from_mode(0o644),
        )?;
        fs::write(
            ctx.repo_config_file(),
            b"# unchanged\n[modes]\n\"other\" = \"0640\"\n",
        )?;
        let before = fs::read(ctx.repo_config_file())?;
        let inode = fs::metadata(ctx.repo_config_file())?.ino();
        let review = review(&ctx, "settings", &ctx.identity().host)?;
        execute(&ctx, &review)?;
        assert_eq!(fs::read(ctx.repo_config_file())?, before);
        assert_eq!(fs::metadata(ctx.repo_config_file())?.ino(), inode);
        assert_eq!(fs::metadata(ctx.repo_config_file())?.mode() & 0o777, 0o600);
        Ok(())
    }

    #[test]
    fn existing_default_mode_is_preserved_and_inexact_mode_is_refused() -> Result<()> {
        let (_root, ctx, _) = fixture(false)?;
        fs::set_permissions(
            ctx.paths.home.join("settings"),
            fs::Permissions::from_mode(0o644),
        )?;
        let target = variant::variant_source("settings", &format!("host={}", ctx.identity().host))?;
        let path = ctx.repo_config_file();
        fs::write(&path, format!("[modes]\n\"{target}\" = \"0600\"\n"))?;
        assert!(
            review(&ctx, "settings", &ctx.identity().host)
                .unwrap_err()
                .is::<TerminalOnly>()
        );
        fs::write(&path, format!("[modes]\n\"{target}\" = \"0644\"\n"))?;
        let before = fs::read(&path)?;
        let inode = fs::metadata(&path)?.ino();
        let review = review(&ctx, "settings", &ctx.identity().host)?;
        execute(&ctx, &review)?;
        assert_eq!(fs::read(&path)?, before);
        assert_eq!(fs::metadata(&path)?.ino(), inode);
        Ok(())
    }

    #[test]
    fn missing_mode_is_terminal_only_without_any_effects() -> Result<()> {
        let (_root, ctx, _) = fixture(false)?;
        let original_review = review(&ctx, "settings", &ctx.identity().host)?;
        fs::write(
            ctx.repo_config_file(),
            b"# unchanged\n[modes]\n\"other\" = \"0640\"\n",
        )?;
        let before = fs::read(ctx.repo_config_file())?;
        let inode = fs::metadata(ctx.repo_config_file())?.ino();
        let error = review(&ctx, "settings", &ctx.identity().host).unwrap_err();
        assert!(error.is::<TerminalOnly>());
        assert!(execute(&ctx, &original_review).is_err());
        assert_eq!(fs::read(ctx.repo_config_file())?, before);
        assert_eq!(fs::metadata(ctx.repo_config_file())?.ino(), inode);
        assert_eq!(fs::read(ctx.paths.repo.join("settings"))?, b"base");
        assert!(!ctx.paths.repo.join(&original_review.target).exists());
        Ok(())
    }

    #[test]
    fn missing_parent_mode_is_terminal_only() -> Result<()> {
        let (_root, ctx, _) = fixture(false)?;
        let parent = ctx.paths.home.join("private");
        fs::create_dir(&parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        let mut config = crate::repo_config::RepoConfig::default();
        config
            .modes
            .insert("private/settings{host=box}".into(), "0600".into());
        let key = "private/settings";
        let target = "private/settings{host=box}";
        assert!(
            check_modes(&ctx, &config, key, target, 0o600, false)
                .unwrap_err()
                .is::<TerminalOnly>()
        );
        config.modes.insert("private/".into(), "0700".into());
        check_modes(&ctx, &config, key, target, 0o600, false)?;
        Ok(())
    }

    #[test]
    fn encrypted_variant_uses_configured_recipient_without_changing_base() -> Result<()> {
        let (_root, ctx, base) = fixture(true)?;
        let before = fs::read(ctx.paths.repo.join(&base))?;
        let review = review(&ctx, "settings", &ctx.identity().host)?;
        let config = fs::read(ctx.repo_config_file())?;
        let inode = fs::metadata(ctx.repo_config_file())?.ino();
        let entry = execute(&ctx, &review)?;
        assert_eq!(fs::read(ctx.repo_config_file())?, config);
        assert_eq!(fs::metadata(ctx.repo_config_file())?.ino(), inode);
        assert_eq!(entry.source, review.target);
        assert_eq!(entry.mode, "0600");
        assert_eq!(fs::read(ctx.paths.repo.join(&base))?, before);
        let ciphertext = fs::read(ctx.paths.repo.join(&review.target))?;
        assert!(!ciphertext.windows(5).any(|part| part == b"local"));
        assert_eq!(
            ctx.age().decrypt(&ctx.paths.repo.join(&review.target))?,
            b"local"
        );
        assert_eq!(fs::read(ctx.paths.home.join("settings"))?, b"local");
        assert_eq!(
            ctx.sources()?
                .select("settings", ctx.identity())
                .winner()
                .copied(),
            Some(review.target.as_str())
        );
        Ok(())
    }

    #[test]
    fn encrypted_existing_mode_metadata_is_preserved() -> Result<()> {
        use std::fmt::Write as _;
        let (_root, ctx, _) = fixture(true)?;
        let target = variant::with_age(&variant::variant_source(
            "settings",
            &format!("host={}", ctx.identity().host),
        )?)?;
        let path = ctx.repo_config_file();
        let mut config = fs::read_to_string(&path)?;
        writeln!(config, "\n[modes]\n\"{target}\" = \"0600\"")?;
        fs::write(&path, &config)?;
        let inode = fs::metadata(&path)?.ino();
        let review = review(&ctx, "settings", &ctx.identity().host)?;
        execute(&ctx, &review)?;
        assert_eq!(fs::read_to_string(&path)?, config);
        assert_eq!(fs::metadata(&path)?.ino(), inode);
        Ok(())
    }
}
