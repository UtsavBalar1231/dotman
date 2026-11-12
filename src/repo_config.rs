//! `.dotman.toml`: the tracked config shared by every machine that uses the repo.
//!
//! Maps are `BTreeMap`s so saving is stable and a load/save round trip of a saved file is
//! byte-identical.

use crate::config::Allow;
use crate::engine::parse_mode;
use crate::name;
use crate::output;
use crate::paths::write_atomic;
use anyhow::{Context, Result, anyhow};
use glob::Pattern;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
}
