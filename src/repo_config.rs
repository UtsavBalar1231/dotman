//! `.dotman.toml`: the tracked config shared by every machine that uses the repo.
//!
//! Maps are `BTreeMap`s so saving is stable and a load/save round trip of a saved file is
//! byte-identical.

use crate::config::Allow;
use crate::engine::parse_mode;
use crate::name;
use crate::output;
use crate::paths::write_atomic;
use crate::system;
use crate::variant::Sources;
use anyhow::{Context, Result, anyhow};
use glob::Pattern;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Once;

const DEFAULT_IGNORE: &[&str] = &[
    "**/.git/",
    "**/*.swp",
    "**/*~",
    "**/.DS_Store",
    "**/__pycache__/",
    "**/node_modules/",
    "**/*.orig",
    "**/*.rej",
];

const DEFAULT_SECRET_NAMES: &[&str] = &[
    "**/.netrc",
    "**/.env",
    "**/.env.*",
    "**/*.pem",
    "**/*.key",
    "**/id_rsa*",
    "**/id_ed25519*",
    "**/id_ecdsa*",
    "**/*.kdbx",
    "**/*.gpg",
    "**/credentials*",
    "**/*token*",
    "**/*secret*",
    "**/gh/hosts.yml",
    "**/*.ppk",
    "**/rclone.conf",
];

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// Contents of `.dotman.toml`. A missing file or section gives the defaults.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoConfig {
    /// Files skipped under managed directories.
    pub ignore: Ignore,
    /// Name patterns held back as probable secrets.
    pub guard: Guard,
    /// Directories tracked as a whole.
    pub dirs: Dirs,
    /// Key to octal mode string. Directory keys end with `/`.
    pub modes: BTreeMap<String, String>,
    /// Key glob to the command run after apply, once approved on a machine.
    pub hooks: BTreeMap<String, String>,
    /// Encryption recipients.
    pub age: RepoAge,
    /// System files, keyed by absolute path.
    pub system: BTreeMap<String, SystemFile>,
}

/// The `[ignore]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ignore {
    /// Globs of files under managed directories that are never captured.
    pub patterns: Vec<String>,
}

impl Default for Ignore {
    fn default() -> Self {
        Self {
            patterns: strings(DEFAULT_IGNORE),
        }
    }
}

/// The `[guard]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Guard {
    /// Globs of new files under managed directories that are held back until added explicitly.
    pub secret_names: Vec<String>,
    /// Entries an older version recorded here. They count for nothing: the allow list is per
    /// machine, in the machine config, so a pushed entry cannot reach another machine. Parsed
    /// and saved as found, so the file still loads and round-trips.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<Allow>,
}

impl Guard {
    /// dotman's own patterns, then the ones `.dotman.toml` adds: a tracked file can add a pattern
    /// but never remove a default.
    pub(crate) fn effective_secret_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = DEFAULT_SECRET_NAMES.to_vec();
        for name in &self.secret_names {
            if !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
        names
    }
}

impl Default for Guard {
    fn default() -> Self {
        Self {
            secret_names: strings(DEFAULT_SECRET_NAMES),
            allow: Vec::new(),
        }
    }
}

/// The `[dirs]` table.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Dirs {
    /// Managed directories, as keys.
    pub paths: Vec<String>,
}

/// The `[age]` table.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RepoAge {
    /// Public keys that encrypted files are encrypted to.
    pub recipients: Vec<String>,
}

/// One `[system."<absolute path>"]` table.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SystemFile {
    /// Owner applied by the system write step.
    pub owner: String,
    /// Group applied by the system write step.
    pub group: String,
    /// Permission bits as an octal string such as `0644`.
    pub mode: String,
    /// Set by `--package-owned`: the file belongs to a package and is overwritten deliberately.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub package_owned: bool,
}

impl RepoConfig {
    /// Parses `.dotman.toml` text. Unknown keys are an error, because saving would drop them from a tracked file.
    /// A malformed `[modes]` value fails here rather than at write time, so dry runs catch it too.
    ///
    /// # Errors
    ///
    /// Fails when the text is not valid TOML, has an unknown key, has an `[ignore]` or `[guard]`
    /// pattern that is not a glob, or has a `[modes]` value that is not octal. What the error
    /// quotes from the text is escaped onto one line.
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text).map_err(|e| syntax_error(&e))?;
        // Compiled here, as `add` compiles them, so a bad one is named with its newlines escaped.
        let globs = config
            .ignore
            .patterns
            .iter()
            .map(|text| ("[ignore]", text, text.strip_suffix('/').unwrap_or(text)))
            .chain(
                config
                    .guard
                    .secret_names
                    .iter()
                    .map(|text| ("[guard]", text, text.as_str())),
            );
        for (table, text, body) in globs {
            Pattern::new(body)
                .map_err(|e| anyhow!("bad {table} pattern `{}`: {e}", output::escape_line(text)))?;
        }
        for (key, value) in &config.modes {
            parse_mode(value).map_err(|e| {
                anyhow!(
                    "[modes] entry `{}`: {}",
                    output::escape_line(key),
                    output::escape_line(&format!("{e:#}"))
                )
            })?;
        }
        Ok(config)
    }

    /// Loads the file; a missing file gives the defaults.
    ///
    /// # Errors
    ///
    /// Fails when the file exists but cannot be read or parsed. A missing file is not an error.
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => {
                let config = Self::parse(&text)
                    .with_context(|| format!("cannot parse {}", path.display()))?;
                warn_ignored_allow(path, &config.guard.allow);
                Ok(config)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Writes the file atomically with mode 0644. A dry run writes nothing.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be written.
    pub fn save(&self, path: &Path, dry_run: bool) -> Result<()> {
        if dry_run {
            return Ok(());
        }
        write_atomic(path, toml::to_string_pretty(self)?.as_bytes(), 0o644)
    }
}

const MODES: &str = "[modes]";
const HOOKS: &str = "[hooks]";
const SYSTEM: &str = "[system]";

/// Which answer settled a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Winner {
    Mine,
    Theirs,
    // reason: the edit answer that builds it comes with the terminal menu.
    #[cfg_attr(not(test), allow(dead_code))]
    Merged,
}

/// A source whose conflict was settled, for the follow rules of [`merge`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Settled {
    pub winner: Winner,
    /// The content came from `merge-file` or an edit, so both sides' modes count.
    pub text_merge: bool,
    /// The source's git tree mode on each side, such as `0o100644`, for a side with no entry.
    pub ours_mode: u32,
    pub theirs_mode: u32,
}

impl Settled {
    const fn stricter(&self) -> bool {
        self.text_merge || matches!(self.winner, Winner::Merged)
    }
}

/// A `[hooks]` or `[system]` key that both sides set differently. The merge keeps this machine's
/// value and leaves the question to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tie {
    pub table: &'static str,
    pub key: String,
}

impl Tie {
    /// Sets the tied key in `merged` to `theirs`' value, or removes it where `theirs` has none. A
    /// `[system]` table keeps the merged mode, the stricter of the two, and takes root's owner and
    /// group where `theirs` has none.
    pub(crate) fn take_theirs(&self, merged: &mut RepoConfig, theirs: &RepoConfig) {
        let key = self.key.as_str();
        match self.table {
            HOOKS => set_or_remove(&mut merged.hooks, key, theirs.hooks.get(key).cloned()),
            SYSTEM => {
                let Some(mode) = merged.system.get(key).map(|kept| kept.mode.clone()) else {
                    return;
                };
                let table = theirs
                    .system
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| root_table(0));
                merged
                    .system
                    .insert(key.to_owned(), SystemFile { mode, ..table });
            }
            _ => {}
        }
    }
}

/// Three-way merge of `.dotman.toml`: `mine` is this machine's side, `theirs` origin's. Lists keep
/// this machine's order, add what origin added and drop what either side removed. Maps merge per
/// key. A settled source's `[modes]` entry and `[system]` table follow `winners` instead, and
/// `host_copies` (shared source, variant) gives each variant this machine's `[modes]` entry. A
/// mode both sides set differently, in `[modes]` or a `[system]` table, takes the stricter one,
/// which no answer changes; `git_modes` gives a source's git tree mode on each side, for a side with no entry.
pub(crate) fn merge(
    base: &RepoConfig,
    mine: &RepoConfig,
    theirs: &RepoConfig,
    winners: &BTreeMap<String, Settled>,
    host_copies: &[(String, String)],
    git_modes: &dyn Fn(&str) -> (u32, u32),
) -> (RepoConfig, Vec<Tie>) {
    let mut ties = Vec::new();
    let mut modes = merge_map(MODES, &base.modes, &mine.modes, &theirs.modes, &mut ties);
    let hooks = merge_map(HOOKS, &base.hooks, &mine.hooks, &theirs.hooks, &mut ties);
    let mut system = merge_map(
        SYSTEM,
        &base.system,
        &mine.system,
        &theirs.system,
        &mut ties,
    );
    follow_modes(&mut modes, mine, theirs, winners);
    let settled_system = follow_system(&mut system, mine, theirs, winners);
    ties.retain(|tie| match tie.table {
        MODES => {
            if !winners.contains_key(&tie.key) {
                let (m, t) = (mine.modes.get(&tie.key), theirs.modes.get(&tie.key));
                let entry = stricter_entry(m, t, git_modes(&tie.key));
                set_or_remove(&mut modes, &tie.key, entry);
            }
            false
        }
        SYSTEM => {
            !settled_system.contains(&tie.key)
                && !stricter_table(
                    &mut system,
                    &tie.key,
                    mine,
                    theirs,
                    git_modes(&format!("{}{}", system::DIR, tie.key)),
                )
        }
        _ => true,
    });
    for (source, variant) in host_copies {
        if let Some(mode) = mine.modes.get(source) {
            modes.insert(variant.clone(), mode.clone());
        }
    }
    let merged = RepoConfig {
        ignore: Ignore {
            patterns: merge_list(
                &base.ignore.patterns,
                &mine.ignore.patterns,
                &theirs.ignore.patterns,
            ),
        },
        guard: Guard {
            secret_names: merge_list(
                &base.guard.secret_names,
                &mine.guard.secret_names,
                &theirs.guard.secret_names,
            ),
            allow: merge_list(&base.guard.allow, &mine.guard.allow, &theirs.guard.allow),
        },
        dirs: Dirs {
            paths: merge_list(&base.dirs.paths, &mine.dirs.paths, &theirs.dirs.paths),
        },
        modes,
        hooks,
        age: RepoAge {
            recipients: merge_list(
                &base.age.recipients,
                &mine.age.recipients,
                &theirs.age.recipients,
            ),
        },
        system,
    };
    (merged, ties)
}

fn merge_list<T: Clone + PartialEq>(base: &[T], mine: &[T], theirs: &[T]) -> Vec<T> {
    let mut merged: Vec<T> = mine
        .iter()
        .filter(|item| !base.contains(item) || theirs.contains(item))
        .cloned()
        .collect();
    for item in theirs {
        if !base.contains(item) && !merged.contains(item) {
            merged.push(item.clone());
        }
    }
    merged
}

fn merge_map<V: Clone + PartialEq>(
    table: &'static str,
    base: &BTreeMap<String, V>,
    mine: &BTreeMap<String, V>,
    theirs: &BTreeMap<String, V>,
    ties: &mut Vec<Tie>,
) -> BTreeMap<String, V> {
    let keys: BTreeSet<&String> = base
        .keys()
        .chain(mine.keys())
        .chain(theirs.keys())
        .collect();
    let mut merged = BTreeMap::new();
    for key in keys {
        let (b, m, t) = (base.get(key), mine.get(key), theirs.get(key));
        let value = if m == t || t == b {
            m
        } else if m == b {
            t
        } else {
            ties.push(Tie {
                table,
                key: key.clone(),
            });
            m
        };
        if let Some(value) = value {
            merged.insert(key.clone(), value.clone());
        }
    }
    merged
}

fn set_or_remove<V>(map: &mut BTreeMap<String, V>, key: &str, value: Option<V>) {
    match value {
        Some(value) => {
            map.insert(key.to_owned(), value);
        }
        None => {
            map.remove(key);
        }
    }
}

// Permission bits only: setuid, setgid and sticky never survive a merge. A side with no entry
// counts as git's mode, and an entry that does not parse as the strictest.
fn bits(entry: Option<&str>, git_mode: u32) -> u32 {
    entry.map_or(git_mode, |text| parse_mode(text).unwrap_or(0)) & 0o777
}

// A side with no entry counts as its git mode in `git`. The result is no entry when a side has
// none and both sides' git modes equal it, since an entry would skip the narrowing a file at
// git's mode gets. Where the git modes differ, the merged tree can carry the wider one.
fn stricter_entry(
    mine: Option<&String>,
    theirs: Option<&String>,
    (ours_git, theirs_git): (u32, u32),
) -> Option<String> {
    let both =
        bits(mine.map(String::as_str), ours_git) & bits(theirs.map(String::as_str), theirs_git);
    let git = (mine.is_none() || theirs.is_none()) && ours_git & 0o777 == theirs_git & 0o777;
    (!git || ours_git & 0o777 != both).then(|| format!("{both:04o}"))
}

// A system file with no `[system]` table is written as root's at its git mode.
fn root_table(git_mode: u32) -> SystemFile {
    SystemFile {
        owner: "root".to_owned(),
        group: "root".to_owned(),
        mode: format!("{:04o}", git_mode & 0o777),
        package_owned: false,
    }
}

// Gives a tied `[system]` key the stricter mode, with this machine's owner and group; a side with
// no table counts as root's at its git mode in `git`. True when the mode was all that differed,
// so no tie is left.
fn stricter_table(
    tables: &mut BTreeMap<String, SystemFile>,
    key: &str,
    mine: &RepoConfig,
    theirs: &RepoConfig,
    (ours_git, theirs_git): (u32, u32),
) -> bool {
    let side = |config: &RepoConfig, git_mode| {
        config
            .system
            .get(key)
            .cloned()
            .unwrap_or_else(|| root_table(git_mode))
    };
    let (m, t) = (side(mine, ours_git), side(theirs, theirs_git));
    let mode = format!("{:04o}", bits(Some(&m.mode), 0) & bits(Some(&t.mode), 0));
    let kept = SystemFile {
        mode: mode.clone(),
        ..m
    };
    let settled = kept == SystemFile { mode, ..t };
    tables.insert(key.to_owned(), kept);
    settled
}

fn follow_modes(
    modes: &mut BTreeMap<String, String>,
    mine: &RepoConfig,
    theirs: &RepoConfig,
    winners: &BTreeMap<String, Settled>,
) {
    for (source, settled) in winners {
        if source.starts_with(system::DIR) {
            continue;
        }
        let (m, t) = (mine.modes.get(source), theirs.modes.get(source));
        let entry = if settled.stricter() {
            (m.is_some() || t.is_some()).then(|| {
                let both = bits(m.map(String::as_str), settled.ours_mode)
                    & bits(t.map(String::as_str), settled.theirs_mode);
                format!("{both:04o}")
            })
        } else if settled.winner == Winner::Mine {
            m.cloned()
        } else {
            t.cloned()
        };
        set_or_remove(modes, source, entry);
    }
}

// The `[system]` key a `.dotman/system/<path>{...}` source belongs to: the absolute target, with
// the variant block stripped.
fn system_key(source: &str) -> Option<String> {
    let rel = source.strip_prefix(system::DIR)?.strip_prefix('/')?;
    Sources::group([rel]).keys().next().map(system::key_of)
}

// Returns the `[system]` keys that have a settled source.
fn follow_system(
    tables: &mut BTreeMap<String, SystemFile>,
    mine: &RepoConfig,
    theirs: &RepoConfig,
    winners: &BTreeMap<String, Settled>,
) -> BTreeSet<String> {
    let mut by_key: BTreeMap<String, Vec<&Settled>> = BTreeMap::new();
    for (source, settled) in winners {
        if let Some(key) = system_key(source) {
            by_key.entry(key).or_default().push(settled);
        }
    }
    for (key, settled) in &by_key {
        let (m, t) = (mine.system.get(key), theirs.system.get(key));
        let Some(first) = settled.first() else {
            continue;
        };
        let agree = settled.iter().all(|s| s.winner == first.winner);
        // Owner, group and `package_owned` never merge: the winner's table supplies them, this
        // machine's for a merged file and for variants settled with different winners.
        let (from, other) = if agree && first.winner == Winner::Theirs {
            (t, m)
        } else {
            (m, t)
        };
        let table = if agree && !settled.iter().any(|s| s.stricter()) && from.is_some() {
            from.cloned()
        } else {
            let git =
                |side: fn(&Settled) -> u32| settled.iter().fold(0o777, |all, s| all & side(s));
            // A side with no table would leave the file root's at git's mode, which can be wider
            // than the other side's table, so a table is kept at the stricter mode, root's when
            // the side that supplies owner and group has none.
            (from.is_some() || other.is_some()).then(|| SystemFile {
                mode: format!(
                    "{:04o}",
                    bits(m.map(|f| f.mode.as_str()), git(|s| s.ours_mode))
                        & bits(t.map(|f| f.mode.as_str()), git(|s| s.theirs_mode))
                ),
                ..from.cloned().unwrap_or_else(|| root_table(0))
            })
        };
        set_or_remove(tables, key, table);
    }
    by_key.into_keys().collect()
}

/// toml's error, its message on one line. The message quotes keys and values from the file, which
/// may hold a newline that would forge a line of output; the frame above it shows one line of the
/// file, after a line number.
fn syntax_error(e: &toml::de::Error) -> anyhow::Error {
    let message = output::escape_line(e.message().trim_end());
    let shown = e.to_string();
    let frame = shown
        .strip_suffix(&format!("{}\n", e.message()))
        .unwrap_or_default();
    anyhow!("{frame}{message}")
}

// Once per run, since a run loads the file several times.
fn warn_ignored_allow(path: &Path, allow: &[Allow]) {
    static WARNED: Once = Once::new();
    if allow.is_empty() {
        return;
    }
    WARNED.call_once(|| {
        let entries: Vec<String> = allow
            .iter()
            .map(|entry| format!("{} {}", entry.path, entry.rule))
            .collect();
        output::warning(&output::escape_line(&format!(
            "{}: [guard] allow is ignored ({}); the allow list is per machine now, in the machine config, where `{} add --allow-plain <file>` records an entry",
            path.display(),
            entries.join(", "),
            name::bin()
        )));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn missing_file_and_empty_text_give_the_defaults() {
        let tmp = TempDir::new().unwrap();
        let loaded = RepoConfig::load(&tmp.path().join(".dotman.toml")).unwrap();
        assert_eq!(loaded, RepoConfig::parse("").unwrap());
        assert_eq!(loaded.ignore.patterns.len(), 8);
        assert!(loaded.ignore.patterns.contains(&"**/*.orig".to_owned()));
        assert_eq!(loaded.guard.secret_names.len(), 16);
        assert!(
            loaded
                .guard
                .secret_names
                .contains(&"**/gh/hosts.yml".to_owned())
        );
        assert_eq!(loaded.dirs.paths, Vec::<String>::new());
    }

    #[test]
    fn partial_sections_keep_other_defaults() {
        let config =
            RepoConfig::parse("[dirs]\npaths = [\".config/nvim\"]\n[ignore]\npatterns = []\n")
                .unwrap();
        assert_eq!(config.ignore.patterns, Vec::<String>::new());
        assert_eq!(config.guard, Guard::default());
        assert_eq!(config.dirs.paths, [".config/nvim"]);
        let only_header = RepoConfig::parse("[guard]\n").unwrap();
        assert_eq!(only_header.guard.secret_names.len(), 16);
        assert_eq!(only_header.guard.allow, Vec::<Allow>::new());
    }

    #[test]
    fn guard_allow_entries_parse_inline_and_round_trip() {
        let config = RepoConfig::parse(
            "[guard]\nallow = [{ path = \".config/zsh/local.zsh\", rule = \"github-token\" }]\n",
        )
        .unwrap();
        let entry = Allow {
            path: ".config/zsh/local.zsh".to_owned(),
            rule: "github-token".to_owned(),
        };
        assert_eq!(config.guard.allow, [entry]);
        assert_eq!(config.guard.secret_names.len(), 16);
        let saved = toml::to_string_pretty(&config).unwrap();
        assert_eq!(RepoConfig::parse(&saved).unwrap(), config);
        assert!(
            !toml::to_string_pretty(&RepoConfig::default())
                .unwrap()
                .contains("allow"),
            "an empty allow list is not written"
        );
        assert!(
            RepoConfig::parse("[guard]\nallow = [{ path = \"x\", rule = \"r\", line = 2 }]\n")
                .is_err()
        );
    }

    #[test]
    fn secret_names_add_to_the_defaults_and_never_remove_one() {
        let emptied = RepoConfig::parse("[guard]\nsecret_names = []\n").unwrap();
        assert_eq!(emptied.guard.effective_secret_names(), DEFAULT_SECRET_NAMES);
        let added =
            RepoConfig::parse("[guard]\nsecret_names = [\"**/*.private\", \"**/*token*\"]\n")
                .unwrap();
        let names = added.guard.effective_secret_names();
        assert_eq!(names.len(), DEFAULT_SECRET_NAMES.len().saturating_add(1));
        assert_eq!(names.last(), Some(&"**/*.private"));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(RepoConfig::parse("surprise = 1").is_err());
        assert!(RepoConfig::parse("[ignore]\nglobs = []").is_err());
    }

    #[test]
    fn malformed_modes_are_rejected_for_files_and_dirs() {
        for bad in [
            "[modes]\n\".bashrc\" = \"rwx\"\n",
            "[modes]\n\".ssh/\" = \"0o700\"\n",
        ] {
            let err = format!("{:#}", RepoConfig::parse(bad).unwrap_err());
            assert!(err.contains("[modes] entry"), "{err}");
        }
        assert!(RepoConfig::parse("[modes]\n\".ssh/\" = \"0700\"\n\"x\" = \"4755\"\n").is_ok());
    }

    #[test]
    fn what_an_error_quotes_from_the_file_never_starts_a_line() {
        for (text, quoted) in [
            (
                "[modes]\n\"x\\nnext: forged\" = \"zz\"\n",
                "`x\\nnext: forged`",
            ),
            (
                "[modes]\n\"x\" = \"7\\nnext: forged\"\n",
                "`7\\nnext: forged`",
            ),
            ("[dirs]\n\"x\\nnext: forged\" = 1\n", "`x\\nnext: forged`"),
            (
                "[ignore]\npatterns = [\"x\\nnext: forged[\"]\n",
                "`x\\nnext: forged[`",
            ),
            (
                "[guard]\nsecret_names = [\"x\\nnext: forged[\"]\n",
                "`x\\nnext: forged[`",
            ),
            (
                "[ignore]\npatterns = \"\"\"\nnext: forged\n\"\"\"\n",
                "next: forged\\n",
            ),
        ] {
            let err = format!("{:#}", RepoConfig::parse(text).unwrap_err());
            assert!(err.contains(quoted), "{text:?}: {err}");
            assert!(
                !err.lines().any(|line| line.starts_with("next:")),
                "{text:?}: {err}"
            );
        }
        let err = format!("{:#}", RepoConfig::parse("surprise = 1\n").unwrap_err());
        assert!(
            err.contains("line 1, column 1\n"),
            "the frame is kept: {err}"
        );
        assert!(err.contains("\n1 | surprise = 1\n"), "{err}");
    }

    #[test]
    fn saving_is_stable_and_round_trips_byte_for_byte() {
        let text = r#"
[modes]
".ssh/" = "0700"
".ssh/config" = "0600"
"bin/run" = "0755"

[hooks]
".config/sway/**" = "swaymsg reload"

[age]
recipients = ["age1abc"]

[system."/etc/pacman.conf"]
owner = "root"
group = "root"
mode = "0644"
package_owned = true

[system."/etc/hosts"]
owner = "root"
group = "root"
mode = "0644"
"#;
        let config = RepoConfig::parse(text).unwrap();
        let tmp = TempDir::new().unwrap();
        let first = tmp.path().join("one.toml");
        let second = tmp.path().join("two.toml");
        config.save(&first, false).unwrap();
        RepoConfig::load(&first)
            .unwrap()
            .save(&second, false)
            .unwrap();
        assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
        assert_eq!(RepoConfig::load(&first).unwrap(), config);

        let saved = fs::read_to_string(&first).unwrap();
        let hosts = saved.find("/etc/hosts").unwrap();
        let pacman = saved.find("/etc/pacman.conf").unwrap();
        assert!(hosts < pacman, "system tables are sorted by path");
        assert_eq!(saved.matches("package_owned").count(), 1);
    }

    #[test]
    fn dry_run_save_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(".dotman.toml");
        RepoConfig::default().save(&path, true).unwrap();
        assert!(!path.exists());
    }

    fn cfg(text: &str) -> RepoConfig {
        RepoConfig::parse(text).unwrap()
    }

    fn settled(winner: Winner, text_merge: bool) -> Settled {
        Settled {
            winner,
            text_merge,
            ours_mode: 0o100_644,
            theirs_mode: 0o100_644,
        }
    }

    fn winners(entries: &[(&str, Settled)]) -> BTreeMap<String, Settled> {
        entries
            .iter()
            .map(|(source, settled)| ((*source).to_owned(), *settled))
            .collect()
    }

    fn git(_: &str) -> (u32, u32) {
        (0o100_644, 0o100_644)
    }

    fn plain(base: &RepoConfig, mine: &RepoConfig, theirs: &RepoConfig) -> (RepoConfig, Vec<Tie>) {
        merge(base, mine, theirs, &BTreeMap::new(), &[], &git)
    }

    fn system_of(config: &RepoConfig, key: &str) -> Option<SystemFile> {
        config.system.get(key).cloned()
    }

    fn mode_of(config: &RepoConfig, key: &str) -> Option<String> {
        config.modes.get(key).cloned()
    }

    #[test]
    fn each_sides_additions_and_removals_survive() {
        let base = cfg(
            "[ignore]\npatterns = [\"a\", \"b\", \"c\"]\n[dirs]\npaths = [\"x\", \"y\"]\n\
             [age]\nrecipients = [\"age1one\"]\n[guard]\nsecret_names = [\"s1\", \"s2\"]\n\
             [modes]\n\"m1\" = \"0600\"\n\"m2\" = \"0700\"\n[hooks]\n\"h1\" = \"one\"\n\"h2\" = \"two\"\n",
        );
        let mine = cfg(
            "[ignore]\npatterns = [\"c\", \"a\", \"mine\"]\n[dirs]\npaths = [\"y\", \"x\", \"mine\"]\n\
             [age]\nrecipients = [\"age1one\", \"age1mine\"]\n[guard]\nsecret_names = [\"s1\", \"s2\"]\n\
             [modes]\n\"m1\" = \"0600\"\n\"mine\" = \"0640\"\n[hooks]\n\"h1\" = \"one\"\n",
        );
        let theirs = cfg(
            "[ignore]\npatterns = [\"a\", \"b\", \"c\", \"theirs\"]\n[dirs]\npaths = [\"x\"]\n\
             [age]\nrecipients = [\"age1one\", \"age1theirs\"]\n[guard]\nsecret_names = [\"s1\", \"s2\", \"s3\"]\n\
             [modes]\n\"m1\" = \"0600\"\n\"m2\" = \"0700\"\n\"theirs\" = \"0644\"\n\
             [hooks]\n\"h1\" = \"one\"\n\"h2\" = \"two\"\n\"h3\" = \"three\"\n",
        );
        let (merged, ties) = plain(&base, &mine, &theirs);
        assert_eq!(ties, []);
        assert_eq!(merged.ignore.patterns, ["c", "a", "mine", "theirs"]);
        assert_eq!(merged.dirs.paths, ["x", "mine"]);
        assert_eq!(merged.age.recipients, ["age1one", "age1mine", "age1theirs"]);
        assert_eq!(merged.guard.secret_names, ["s1", "s2", "s3"]);
        let keys = |map: &BTreeMap<String, String>| map.keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys(&merged.modes), ["m1", "mine", "theirs"]);
        assert_eq!(keys(&merged.hooks), ["h1", "h3"]);
    }

    #[test]
    fn a_key_set_differently_on_both_sides_is_a_tie() {
        let base = cfg("[hooks]\n\"h\" = \"one\"\n\
             [system.\"/etc/x\"]\nowner = \"root\"\ngroup = \"root\"\nmode = \"0644\"\n");
        let mine = cfg("[hooks]\n\"h\" = \"mine\"\n\
             [system.\"/etc/x\"]\nowner = \"root\"\ngroup = \"wheel\"\nmode = \"0644\"\n");
        let theirs = cfg("[hooks]\n\"h\" = \"theirs\"\n\
             [system.\"/etc/x\"]\nowner = \"root\"\ngroup = \"adm\"\nmode = \"0644\"\n");
        let (mut merged, ties) = plain(&base, &mine, &theirs);
        let tie = |table, key: &str| Tie {
            table,
            key: key.to_owned(),
        };
        assert_eq!(ties, [tie(HOOKS, "h"), tie(SYSTEM, "/etc/x")]);
        assert_eq!(merged.hooks.get("h").map(String::as_str), Some("mine"));
        assert_eq!(
            system_of(&merged, "/etc/x").map(|f| f.group).as_deref(),
            Some("wheel")
        );
        for tie in &ties {
            tie.take_theirs(&mut merged, &theirs);
        }
        assert_eq!(merged.hooks.get("h").map(String::as_str), Some("theirs"));
        assert_eq!(system_of(&merged, "/etc/x"), system_of(&theirs, "/etc/x"));
    }

    /// A `[modes]` key both sides set differently is no tie: it takes the stricter mode, without
    /// special bits, a side with no entry counting as its git mode, whatever the answer.
    #[test]
    fn a_modes_key_set_differently_on_both_takes_the_stricter_mode() {
        let base = cfg("[modes]\n\"a\" = \"0600\"\n\"gone\" = \"0600\"\n\"run\" = \"0700\"\n");
        let mine = cfg(
            "[modes]\n\"a\" = \"0640\"\n\"new\" = \"0600\"\n\"s\" = \"4755\"\n\"wide\" = \"0600\"\n",
        );
        let theirs = cfg(
            "[modes]\n\"a\" = \"0660\"\n\"gone\" = \"0640\"\n\"new\" = \"0666\"\n\"run\" = \"0777\"\n\
             \"s\" = \"0777\"\n\"wide\" = \"0666\"\n",
        );
        let git = |source: &str| match source {
            "run" => (0o100_755, 0o100_755),
            _ => (0o100_644, 0o100_644),
        };
        let (merged, ties) = merge(&base, &mine, &theirs, &BTreeMap::new(), &[], &git);
        assert_eq!(ties, []);
        for (key, mode) in [
            ("a", Some("0640")),
            ("gone", Some("0640")),
            ("new", Some("0600")),
            ("s", Some("0755")),
            ("wide", Some("0600")),
            ("run", None),
        ] {
            assert_eq!(mode_of(&merged, key).as_deref(), mode, "{key}");
        }
    }

    /// A `[system]` table both sides set differently takes the stricter mode: with nothing else
    /// different it is no tie, and taking origin's owner and group keeps that mode.
    #[test]
    fn a_system_table_set_differently_on_both_takes_the_stricter_mode() {
        let base = system_cfg(&[("/etc/x", table("root", "root", "0640"))]);
        let mine = system_cfg(&[("/etc/x", table("root", "root", "0600"))]);
        let theirs = system_cfg(&[("/etc/x", table("root", "root", "0666"))]);
        let (merged, ties) = plain(&base, &mine, &theirs);
        assert_eq!(ties, []);
        assert_eq!(system_of(&merged, "/etc/x"), system_of(&mine, "/etc/x"));

        let theirs = system_cfg(&[("/etc/x", table("app", "adm", "4666"))]);
        let (mut merged, ties) = plain(&base, &mine, &theirs);
        assert_eq!(ties.len(), 1);
        assert_eq!(system_of(&merged, "/etc/x"), system_of(&mine, "/etc/x"));
        for tie in &ties {
            tie.take_theirs(&mut merged, &theirs);
        }
        assert_eq!(
            system_of(&merged, "/etc/x"),
            Some(SystemFile {
                owner: "app".to_owned(),
                group: "adm".to_owned(),
                mode: "0600".to_owned(),
                package_owned: false,
            })
        );
    }

    /// A `[system]` tie where one side has no table counts that side as root's at the git mode of
    /// the key's source, so either answer keeps the stricter mode without special bits.
    #[test]
    fn a_system_tie_counts_a_missing_table_as_roots_at_the_git_mode() {
        let base = system_cfg(&[("/etc/x", table("root", "root", "0640"))]);
        let without = RepoConfig::default();
        let git = |source: &str| match source {
            ".dotman/system/etc/x" => (0o100_755, 0o100_755),
            _ => (0o100_644, 0o100_644),
        };
        let want = |owner: &str, mode: &str| {
            Some(SystemFile {
                owner: owner.to_owned(),
                group: owner.to_owned(),
                mode: mode.to_owned(),
                package_owned: false,
            })
        };
        for (wide, mode) in [("4777", "0755"), ("0750", "0750")] {
            let theirs = system_cfg(&[("/etc/x", table("app", "app", wide))]);
            let (mut merged, ties) = merge(&base, &without, &theirs, &BTreeMap::new(), &[], &git);
            assert_eq!(ties.len(), 1, "{wide}");
            assert_eq!(system_of(&merged, "/etc/x"), want("root", mode), "{wide}");
            for tie in &ties {
                tie.take_theirs(&mut merged, &theirs);
            }
            assert_eq!(system_of(&merged, "/etc/x"), want("app", mode), "{wide}");
        }

        let mine = system_cfg(&[("/etc/x", table("app", "app", "0600"))]);
        let (mut merged, ties) = merge(&base, &mine, &without, &BTreeMap::new(), &[], &git);
        for tie in &ties {
            tie.take_theirs(&mut merged, &without);
        }
        assert_eq!(system_of(&merged, "/etc/x"), want("root", "0600"));
    }

    #[test]
    fn merging_with_a_side_equal_to_base_or_to_the_other_gives_that_side() {
        let base = cfg("[dirs]\npaths = [\"x\"]\n[modes]\n\"a\" = \"0600\"\n");
        let x = cfg(
            "[ignore]\npatterns = [\"keep\"]\n[dirs]\npaths = [\"x\", \"z\"]\n[modes]\n\"b\" = \"0700\"\n\
             [hooks]\n\"h\" = \"run\"\n[age]\nrecipients = [\"age1x\"]\n\
             [guard]\nallow = [{ path = \"p\", rule = \"r\" }]\n\
             [system.\"/etc/x\"]\nowner = \"root\"\ngroup = \"root\"\nmode = \"0644\"\n",
        );
        assert_eq!(plain(&base, &x, &base), (x.clone(), vec![]));
        assert_eq!(plain(&base, &x, &x), (x, vec![]));
    }

    #[test]
    fn guard_allow_entries_merge_as_a_list() {
        let allow = |path: &str| Allow {
            path: path.to_owned(),
            rule: "r".to_owned(),
        };
        let mut base = RepoConfig::default();
        base.guard.allow = vec![allow("kept"), allow("dropped")];
        let mut mine = base.clone();
        mine.guard.allow = vec![allow("kept"), allow("mine")];
        let mut theirs = base.clone();
        theirs.guard.allow = vec![allow("kept"), allow("dropped"), allow("theirs")];
        let (merged, _) = plain(&base, &mine, &theirs);
        assert_eq!(
            merged.guard.allow,
            [allow("kept"), allow("mine"), allow("theirs")]
        );
    }

    #[test]
    fn the_result_reparses_equal() {
        let base = cfg("[modes]\n\"a\" = \"0600\"\n");
        let mine = cfg(
            "[modes]\n\"a\" = \"0600\"\n\"b\" = \"0755\"\n[guard]\nallow = [{ path = \"p\", rule = \"r\" }]\n\
             [system.\"/etc/x\"]\nowner = \"root\"\ngroup = \"root\"\nmode = \"0644\"\npackage_owned = true\n",
        );
        let theirs = cfg("[modes]\n\"a\" = \"0600\"\n\"c\" = \"0700\"\n[dirs]\npaths = [\"d\"]\n");
        let (merged, _) = plain(&base, &mine, &theirs);
        let saved = toml::to_string_pretty(&merged).unwrap();
        assert_eq!(RepoConfig::parse(&saved).unwrap(), merged);
    }

    #[test]
    fn a_whole_version_takes_the_winners_modes_entry() {
        let base = cfg("[modes]\n\"a\" = \"0600\"\n\"b\" = \"0600\"\n");
        let mine = cfg("[modes]\n\"a\" = \"0640\"\n");
        let theirs = cfg("[modes]\n\"a\" = \"0660\"\n\"b\" = \"0660\"\n\"c\" = \"0700\"\n");
        let run = |winner| {
            let won = winners(&[
                ("a", settled(winner, false)),
                ("b", settled(winner, false)),
                ("c", settled(winner, false)),
            ]);
            merge(&base, &mine, &theirs, &won, &[], &git)
        };
        let (merged, ties) = run(Winner::Mine);
        assert_eq!(ties, [], "a settled source is no tie");
        assert_eq!(mode_of(&merged, "a").as_deref(), Some("0640"));
        assert_eq!(mode_of(&merged, "b"), None, "mine had no entry");
        assert_eq!(mode_of(&merged, "c"), None);
        let (merged, _) = run(Winner::Theirs);
        assert_eq!(mode_of(&merged, "a").as_deref(), Some("0660"));
        assert_eq!(mode_of(&merged, "b").as_deref(), Some("0660"));
        assert_eq!(mode_of(&merged, "c").as_deref(), Some("0700"));
    }

    #[test]
    fn a_merged_text_takes_the_stricter_mode() {
        let mine = cfg("[modes]\n\"a\" = \"0664\"\n\"s\" = \"4755\"\n");
        let theirs = cfg("[modes]\n\"a\" = \"0640\"\n\"s\" = \"0777\"\n");
        let base = RepoConfig::default();
        for (winner, text_merge) in [
            (Winner::Mine, true),
            (Winner::Theirs, true),
            (Winner::Merged, true),
            (Winner::Merged, false),
        ] {
            let won = winners(&[
                ("a", settled(winner, text_merge)),
                ("s", settled(winner, text_merge)),
            ]);
            let (merged, ties) = merge(&base, &mine, &theirs, &won, &[], &git);
            assert_eq!(ties, []);
            assert_eq!(mode_of(&merged, "a").as_deref(), Some("0640"), "{winner:?}");
            assert_eq!(
                mode_of(&merged, "s").as_deref(),
                Some("0755"),
                "special bits drop"
            );
        }
    }

    #[test]
    fn a_0600_entry_on_one_side_stays_0600_under_both_winners() {
        let base = RepoConfig::default();
        let with = cfg("[modes]\n\"secret\" = \"0600\"\n");
        let without = RepoConfig::default();
        for winner in [Winner::Mine, Winner::Theirs] {
            let won = winners(&[("secret", settled(winner, true))]);
            for (mine, theirs) in [(&with, &without), (&without, &with)] {
                let (merged, _) = merge(&base, mine, theirs, &won, &[], &git);
                assert_eq!(
                    mode_of(&merged, "secret").as_deref(),
                    Some("0600"),
                    "{winner:?}"
                );
            }
        }
    }

    #[test]
    fn a_missing_entry_counts_as_the_git_mode_and_two_missing_stay_missing() {
        let base = RepoConfig::default();
        let theirs = cfg("[modes]\n\"run\" = \"0755\"\n");
        let won = winners(&[
            (
                "run",
                Settled {
                    winner: Winner::Merged,
                    text_merge: true,
                    ours_mode: 0o100_644,
                    theirs_mode: 0o100_755,
                },
            ),
            ("none", settled(Winner::Merged, true)),
        ]);
        let (merged, _) = merge(&base, &RepoConfig::default(), &theirs, &won, &[], &git);
        assert_eq!(mode_of(&merged, "run").as_deref(), Some("0644"));
        assert_eq!(mode_of(&merged, "none"), None);
    }

    /// A `[modes]` tie where one side has no entry keeps the stricter entry when the two sides'
    /// git modes differ, since the merged tree can carry the wider git mode.
    #[test]
    fn a_modes_tie_keeps_the_entry_when_the_git_modes_differ() {
        let base = cfg("[modes]\n\"x\" = \"0600\"\n");
        let without = RepoConfig::default();
        let git = |_: &str| (0o100_644, 0o100_755);
        for wide in ["0777", "4777"] {
            let theirs = cfg(&format!("[modes]\n\"x\" = \"{wide}\"\n"));
            let (merged, ties) = merge(&base, &without, &theirs, &BTreeMap::new(), &[], &git);
            assert_eq!(ties, []);
            assert_eq!(mode_of(&merged, "x").as_deref(), Some("0644"), "{wide}");
        }
        let theirs = cfg("[modes]\n\"x\" = \"0755\"\n");
        let (merged, _) = merge(&base, &theirs, &without, &BTreeMap::new(), &[], &git);
        assert_eq!(mode_of(&merged, "x").as_deref(), Some("0755"));
    }

    #[test]
    fn host_copies_give_the_variant_this_machines_modes_entry() {
        let base = cfg("[modes]\n\"rc\" = \"0600\"\n");
        let mine = cfg("[modes]\n\"rc\" = \"0640\"\n");
        let theirs = cfg("[modes]\n\"rc\" = \"0600\"\n");
        let won = winners(&[("rc", settled(Winner::Theirs, false))]);
        let copies = [("rc".to_owned(), "rc{host=laptop}".to_owned())];
        let (merged, _) = merge(&base, &mine, &theirs, &won, &copies, &git);
        assert_eq!(mode_of(&merged, "rc").as_deref(), Some("0600"));
        assert_eq!(mode_of(&merged, "rc{host=laptop}").as_deref(), Some("0640"));
        let (merged, _) = merge(&base, &theirs, &theirs, &won, &copies[..0], &git);
        assert_eq!(mode_of(&merged, "rc{host=laptop}"), None);
    }

    fn table(owner: &str, group: &str, mode: &str) -> String {
        format!("owner = \"{owner}\"\ngroup = \"{group}\"\nmode = \"{mode}\"\n")
    }

    fn system_cfg(tables: &[(&str, String)]) -> RepoConfig {
        let parts: Vec<String> = tables
            .iter()
            .map(|(path, body)| format!("[system.\"{path}\"]\n{body}"))
            .collect();
        cfg(&parts.concat())
    }

    #[test]
    fn a_system_source_follows_the_winner_through_its_mapped_key() {
        let base = system_cfg(&[("/etc/hosts", table("root", "root", "0644"))]);
        let mine = system_cfg(&[
            ("/etc/hosts", table("root", "wheel", "0640")),
            ("/etc/other", table("root", "root", "0600")),
        ]);
        let theirs = system_cfg(&[("/etc/hosts", table("root", "adm", "0664"))]);
        let source = ".dotman/system/etc/hosts{host=laptop}";
        let run = |winner| {
            let won = winners(&[(source, settled(winner, false))]);
            merge(&base, &mine, &theirs, &won, &[], &git)
        };
        let (merged, ties) = run(Winner::Theirs);
        assert_eq!(ties, [], "a settled system source is no tie");
        assert_eq!(
            system_of(&merged, "/etc/hosts"),
            system_of(&theirs, "/etc/hosts")
        );
        assert_eq!(
            system_of(&merged, "/etc/other"),
            system_of(&mine, "/etc/other")
        );
        let (merged, _) = run(Winner::Mine);
        assert_eq!(
            system_of(&merged, "/etc/hosts"),
            system_of(&mine, "/etc/hosts")
        );

        let without = RepoConfig::default();
        let won = winners(&[(".dotman/system/etc/hosts", settled(Winner::Theirs, false))]);
        let (merged, _) = merge(&base, &mine, &without, &won, &[], &git);
        let kept = system_of(&merged, "/etc/hosts").expect("a table at the stricter mode is kept");
        assert_eq!(
            (kept.owner.as_str(), kept.group.as_str(), kept.mode.as_str()),
            ("root", "root", "0640"),
            "theirs had none, so the file would have been root's at 0644, and wheel is not theirs"
        );
        assert!(merged.system.contains_key("/etc/other"));

        let (merged, _) = merge(
            &base,
            &without,
            &theirs,
            &winners(&[(".dotman/system/etc/hosts", settled(Winner::Mine, true))]),
            &[],
            &git,
        );
        let kept = system_of(&merged, "/etc/hosts").expect("a table at the stricter mode is kept");
        assert_eq!(
            (kept.owner.as_str(), kept.group.as_str(), kept.mode.as_str()),
            ("root", "root", "0644"),
            "this machine had none, so adm, origin's group, is not handed over"
        );
    }

    #[test]
    fn a_merged_system_text_takes_the_stricter_mode_and_the_winners_owner() {
        let base = RepoConfig::default();
        let mine = system_cfg(&[("/etc/x", table("root", "wheel", "0664"))]);
        let theirs = system_cfg(&[("/etc/x", table("app", "adm", "0640"))]);
        let source = ".dotman/system/etc/x";
        let run = |winner, text_merge| {
            let won = winners(&[(source, settled(winner, text_merge))]);
            system_of(&merge(&base, &mine, &theirs, &won, &[], &git).0, "/etc/x")
        };
        let want = |owner: &str, group: &str| SystemFile {
            owner: owner.to_owned(),
            group: group.to_owned(),
            mode: "0640".to_owned(),
            package_owned: false,
        };
        assert_eq!(run(Winner::Mine, true), Some(want("root", "wheel")));
        assert_eq!(run(Winner::Theirs, true), Some(want("app", "adm")));
        assert_eq!(run(Winner::Merged, true), Some(want("root", "wheel")));
        assert_eq!(run(Winner::Merged, false), Some(want("root", "wheel")));
    }

    #[test]
    fn two_variants_of_a_system_path_with_different_winners_take_this_machines_table() {
        let base = RepoConfig::default();
        let mine = system_cfg(&[("/etc/x", table("root", "wheel", "0664"))]);
        let theirs = system_cfg(&[("/etc/x", table("app", "adm", "0640"))]);
        let won = winners(&[
            (".dotman/system/etc/x{host=a}", settled(Winner::Mine, false)),
            (
                ".dotman/system/etc/x{host=b}",
                settled(Winner::Theirs, false),
            ),
        ]);
        let (merged, _) = merge(&base, &mine, &theirs, &won, &[], &git);
        assert_eq!(
            system_of(&merged, "/etc/x"),
            Some(SystemFile {
                owner: "root".to_owned(),
                group: "wheel".to_owned(),
                mode: "0640".to_owned(),
                package_owned: false,
            })
        );
        let same = winners(&[
            (
                ".dotman/system/etc/x{host=a}",
                settled(Winner::Theirs, false),
            ),
            (
                ".dotman/system/etc/x{host=b}",
                settled(Winner::Theirs, false),
            ),
        ]);
        let (merged, _) = merge(&base, &mine, &theirs, &same, &[], &git);
        assert_eq!(system_of(&merged, "/etc/x"), system_of(&theirs, "/etc/x"));
    }
}
