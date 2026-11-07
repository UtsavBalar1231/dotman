//! `state.toml`: what dotman last applied on this machine. Never synced.

use crate::paths::{create_private_dir, write_atomic};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

/// The only `version` this code reads and writes.
pub const VERSION: u32 = 1;

/// Parsed `state.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Format version; loading refuses any value other than [`VERSION`].
    pub version: u32,
    /// Contents of the repo's `.git/dotman-id` when this state was written.
    pub repo_id: String,
    /// Whether the remote was seen to be public.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_public: Option<RemotePublic>,
    /// URL that `remote_public` was checked against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    /// Unix seconds of the last visibility check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_checked_at: Option<u64>,
    /// Machine identity recorded on the first run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<IdentitySnapshot>,
    /// Hook approval.
    pub hooks: Hooks,
    /// Applied home files, keyed by repo key.
    pub files: BTreeMap<String, Entry>,
    /// Applied system files, keyed by absolute path.
    pub system: BTreeMap<String, Entry>,
}

/// Result of a remote visibility check; a private remote is recorded as absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemotePublic {
    /// The remote answered an anonymous request.
    #[serde(rename = "true")]
    True,
    /// The check could not decide.
    #[serde(rename = "unknown")]
    Unknown,
}

/// Machine identity plus profiles, as recorded by the first run. Values are lowercased.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
// reason: the fields are the identity keys, named by the struct doc.
#[allow(missing_docs)]
pub struct IdentitySnapshot {
    pub host: String,
    pub user: String,
    pub os: String,
    pub distro: String,
    pub like: String,
    pub arch: String,
    pub profiles: Vec<String>,
}

/// The `[hooks]` table.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hooks {
    /// xxh3 of the `[hooks]` table in `.dotman.toml` that this machine approved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved: Option<String>,
}

/// One applied path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Repo-relative path of the variant or template that was applied.
    pub source: String,
    /// What was written.
    pub kind: Kind,
    /// xxh3-128 as 32 hex chars, of the file bytes or of the link target.
    pub hash: String,
    /// Permission bits as an octal string such as `0644`.
    pub mode: String,
}

/// What an applied path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A regular file; the hash covers its bytes.
    File,
    /// A symbolic link; the hash covers its target.
    Symlink,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: VERSION,
            repo_id: String::new(),
            remote_public: None,
            remote_url: None,
            remote_checked_at: None,
            identity: None,
            hooks: Hooks::default(),
            files: BTreeMap::new(),
            system: BTreeMap::new(),
        }
    }
}

impl State {
    /// Loads the state for the repo whose `.git/dotman-id` is `repo_id` (already trimmed).
    ///
    /// A missing file gives an empty state. A file that does not parse is an error and is never
    /// treated as empty. A file written for another repo keeps only its identity snapshot, so
    /// every entry counts as absent.
    pub fn load(path: &Path, repo_id: &str) -> Result<Self> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self::for_repo(Self::default(), repo_id));
            }
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        let state = std::str::from_utf8(&bytes)
            .map_err(|e| anyhow!(e))
            .and_then(|text| toml::from_str::<Self>(text).map_err(|e| anyhow!(e)))
            .map_err(|e| corrupt(path, &format!("{e}")))?;
        if state.version != VERSION {
            bail!(corrupt(
                path,
                &format!("version {} is not supported", state.version)
            ));
        }
        Ok(Self::for_repo(state, repo_id))
    }

    fn for_repo(state: Self, repo_id: &str) -> Self {
        if state.repo_id == repo_id {
            return state;
        }
        Self {
            repo_id: repo_id.to_owned(),
            identity: state.identity,
            ..Self::default()
        }
    }

    /// Writes the state atomically with mode 0600, creating its directory at 0700. A dry run writes nothing.
    pub fn save(&self, path: &Path, dry_run: bool) -> Result<()> {
        if dry_run {
            return Ok(());
        }
        if let Some(dir) = path.parent() {
            create_private_dir(dir, false)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        write_atomic(path, toml::to_string_pretty(self)?.as_bytes(), 0o600)
    }
}

fn corrupt(path: &Path, detail: &str) -> anyhow::Error {
    anyhow!(
        "{} cannot be used: {detail}; run `dot doctor`",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    const ID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn sample() -> State {
        let entry = Entry {
            source: ".bashrc##host.box".to_owned(),
            kind: Kind::File,
            hash: "0123456789abcdef0123456789abcdef".to_owned(),
            mode: "0644".to_owned(),
        };
        let link = Entry {
            kind: Kind::Symlink,
            mode: "0777".to_owned(),
            ..entry.clone()
        };
        State {
            repo_id: ID_A.to_owned(),
            remote_public: Some(RemotePublic::Unknown),
            remote_url: Some("https://example.org/dots.git".to_owned()),
            remote_checked_at: Some(1_700_000_000),
            identity: Some(IdentitySnapshot {
                host: "box".to_owned(),
                profiles: vec!["work".to_owned()],
                ..IdentitySnapshot::default()
            }),
            hooks: Hooks {
                approved: Some("deadbeef".to_owned()),
            },
            files: BTreeMap::from([
                (".bashrc".to_owned(), entry),
                (".config/a.b".to_owned(), link.clone()),
            ]),
            system: BTreeMap::from([("/etc/hosts".to_owned(), link)]),
            ..State::default()
        }
    }

    #[test]
    fn missing_file_is_an_empty_state_for_this_repo() {
        let tmp = TempDir::new().unwrap();
        let state = State::load(&tmp.path().join("state.toml"), ID_A).unwrap();
        assert_eq!(state.repo_id, ID_A);
        assert_eq!(state.version, 1);
        assert!(state.files.is_empty() && state.system.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("dotman/state.toml");
        let state = sample();
        state.save(&path, false).unwrap();
        assert_eq!(State::load(&path, ID_A).unwrap(), state);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn repo_id_mismatch_drops_entries_and_keeps_identity() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.toml");
        let state = sample();
        state.save(&path, false).unwrap();

        let loaded = State::load(&path, ID_B).unwrap();
        assert!(loaded.files.is_empty());
        assert!(loaded.system.is_empty());
        assert_eq!(loaded.identity, state.identity);
        assert_eq!(loaded.repo_id, ID_B);
        assert_eq!(loaded.hooks.approved, None);
        assert_eq!(loaded.remote_url, None);
    }

    #[test]
    fn corrupt_files_are_errors_naming_doctor() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.toml");
        let bad_inputs: [&[u8]; 4] = [
            b"not = [toml",
            b"\xff\xfe",
            b"version = 2\n",
            b"version = \"1\"\n",
        ];
        for bad in bad_inputs {
            fs::write(&path, bad).unwrap();
            let err = State::load(&path, ID_A).unwrap_err().to_string();
            assert!(err.contains("dot doctor"), "{err}");
        }
        fs::write(&path, b"").unwrap();
        assert!(State::load(&path, ID_A).is_ok());
    }

    #[test]
    fn dry_run_save_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("dotman/state.toml");
        sample().save(&path, true).unwrap();
        assert!(!tmp.path().join("dotman").exists());
    }
}
