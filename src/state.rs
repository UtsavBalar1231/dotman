//! `state.toml`: what dotman last applied on this machine. Never synced.

use crate::paths::{create_private_dir, write_atomic};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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
    /// The `[dirs]` entries under which this machine captures new files. `None` in a state written
    /// before approval existed, until loading through the context approves the current entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_dirs: Option<BTreeSet<String>>,
    /// Every `[ignore]` pattern this machine has seen in `.dotman.toml`. The capture scan keeps
    /// applying one after the file drops it, until `sync --drop-unlisted-ignores` drops it here.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub ignore_seen: BTreeSet<String>,
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
    /// The `[hooks]` table in `.dotman.toml` that this machine approved, as compact JSON of the
    /// ordered table. An older dotman kept an xxh3 hex hash here, which approves nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approved: Option<String>,
    /// Changed keys whose hooks were not run because the table was not approved. Once a table is
    /// approved, its hooks that match one of them run once, and the set is emptied.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub pending: BTreeSet<String>,
    /// The table, in the approval's form, last shown with `pending`. Another table is shown again
    /// before an approval could run it for them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_table: Option<String>,
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
    /// The repo's mode, when this machine keeps its own `mode` instead; absent when `mode` is the
    /// repo's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_mode: Option<String>,
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
            capture_dirs: None,
            ignore_seen: BTreeSet::new(),
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
    /// A missing file gives an empty state. A file that is empty or does not parse is an error and
    /// is never treated as empty. A file written for another repo keeps only its identity snapshot, so
    /// every entry counts as absent.
    pub(crate) fn load(path: &Path, repo_id: &str) -> Result<Self> {
        Ok(Self::read(path)?.map_or_else(
            || Self::fresh(repo_id),
            |state| Self::for_repo(state, repo_id),
        ))
    }

    /// The file as written, whatever repo it belongs to; `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// Fails when the file exists but cannot be read, is empty, is not valid UTF-8 or TOML, or has
    /// an unsupported version.
    pub fn read(path: &Path) -> Result<Option<Self>> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        // `save` always writes `version`, so an empty file is a truncated one. Read as empty, it
        // would make every applied file look new.
        if bytes.is_empty() {
            let shown = shell_words::quote(&path.to_string_lossy()).into_owned();
            bail!(
                "{} is empty, so what was applied on this machine is unknown; move it aside with `mv {shown} {shown}.broken`, and the next `{} apply` rebuilds it, asking about files that differ from the repo",
                path.display(),
                crate::name::bin()
            );
        }
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
        // Checked here so that a bad entry names this file, not just the key.
        for (key, entry) in state.files.iter().chain(&state.system) {
            let modes = std::iter::once(&entry.mode).chain(&entry.repo_mode);
            for mode in modes.filter(|_| entry.kind == Kind::File) {
                if let Err(e) = crate::engine::parse_mode(mode) {
                    bail!(corrupt(path, &format!("{key}: {e}")));
                }
            }
        }
        for pattern in &state.ignore_seen {
            let body = pattern.strip_suffix('/').unwrap_or(pattern);
            if let Err(e) = glob::Pattern::new(body) {
                let shown = crate::output::escape_line(pattern);
                bail!(corrupt(path, &format!("ignore_seen `{shown}`: {e}")));
            }
        }
        Ok(Some(state))
    }

    fn for_repo(state: Self, repo_id: &str) -> Self {
        if state.repo_id == repo_id {
            return state;
        }
        Self {
            identity: state.identity,
            ..Self::fresh(repo_id)
        }
    }

    /// A state no dotman has written for this repo yet, which approves no `[dirs]` entry.
    fn fresh(repo_id: &str) -> Self {
        Self {
            repo_id: repo_id.to_owned(),
            capture_dirs: Some(BTreeSet::new()),
            ..Self::default()
        }
    }

    /// Approves every entry of `dirs` when the state predates approval: that machine has been
    /// capturing new files under them already. A state that records approvals is left alone.
    pub(crate) fn grandfather_dirs(&mut self, dirs: &[String]) {
        self.capture_dirs
            .get_or_insert_with(|| dirs.iter().cloned().collect());
    }

    pub(crate) fn approve_dirs<'a>(&mut self, dirs: impl IntoIterator<Item = &'a String>) {
        self.capture_dirs
            .get_or_insert_with(BTreeSet::new)
            .extend(dirs.into_iter().cloned());
    }

    pub(crate) fn dirs_approved(&self) -> BTreeSet<String> {
        self.capture_dirs.clone().unwrap_or_default()
    }

    /// The patterns this machine has seen in `[ignore]` that `listed` no longer has.
    pub(crate) fn unlisted_ignores(&self, listed: &[String]) -> Vec<String> {
        self.ignore_seen
            .iter()
            .filter(|seen| !listed.contains(seen))
            .cloned()
            .collect()
    }

    /// Writes the state atomically with mode 0600, creating its directory at 0700. A dry run writes nothing.
    ///
    /// # Errors
    ///
    /// Fails when the directory cannot be created or the file cannot be written.
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
        "{} cannot be used: {detail}; run `{} doctor`",
        path.display(),
        crate::name::bin()
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
            mode: "0600".to_owned(),
            repo_mode: Some("0644".to_owned()),
        };
        let link = Entry {
            kind: Kind::Symlink,
            mode: "0777".to_owned(),
            repo_mode: None,
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
                pending: BTreeSet::from([".config/app/a.conf".to_owned()]),
                pending_table: Some(r#"{"a":"x"}"#.to_owned()),
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
    fn only_a_state_that_predates_dir_approval_lacks_it() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.toml");
        let fresh = State::load(&path, ID_A).unwrap();
        assert_eq!(fresh.capture_dirs, Some(BTreeSet::new()));

        fs::write(&path, format!("version = 1\nrepo_id = \"{ID_A}\"\n")).unwrap();
        let mut old = State::load(&path, ID_A).unwrap();
        assert_eq!(old.capture_dirs, None);
        let other = State::load(&path, ID_B).unwrap();
        assert_eq!(other.capture_dirs, Some(BTreeSet::new()));

        old.grandfather_dirs(&[".config/nvim".to_owned()]);
        old.grandfather_dirs(&[".kube".to_owned()]);
        old.save(&path, false).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("capture_dirs = [\".config/nvim\"]"), "{text}");
        fresh.save(&path, false).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("capture_dirs = []"), "{text}");
        let mut reloaded = State::load(&path, ID_A).unwrap();
        reloaded.grandfather_dirs(&[".kube".to_owned()]);
        assert_eq!(reloaded.dirs_approved(), BTreeSet::new());
    }

    #[test]
    fn corrupt_files_are_errors_naming_doctor() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state.toml");
        let bad_inputs: [&[u8]; 7] = [
            b"not = [toml",
            b"\xff\xfe",
            b"version = 2\n",
            b"version = \"1\"\n",
            b"version = 1\n[files.\".a\"]\nsource = \".a\"\nkind = \"file\"\nhash = \"x\"\nmode = \"rw\"\n",
            b"version = 1\n[files.\".a\"]\nsource = \".a\"\nkind = \"file\"\nhash = \"x\"\nmode = \"0600\"\nrepo_mode = \"rw\"\n",
            b"version = 1\nignore_seen = [\"**/[\"]\n",
        ];
        for bad in bad_inputs {
            fs::write(&path, bad).unwrap();
            let err = State::load(&path, ID_A).unwrap_err().to_string();
            assert!(err.contains("dotman doctor"), "{err}");
        }
        fs::write(&path, b"").unwrap();
        let err = State::load(&path, ID_A).unwrap_err().to_string();
        assert!(err.contains("is empty"), "{err}");
        assert!(err.contains(".broken"), "{err}");
    }

    #[test]
    fn dry_run_save_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("dotman/state.toml");
        sample().save(&path, true).unwrap();
        assert!(!tmp.path().join("dotman").exists());
    }
}
