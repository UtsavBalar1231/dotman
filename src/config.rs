//! The per-machine config file. It is never synced, and every field is optional.

use crate::output;
use crate::paths::{create_private_dir, write_atomic};
use crate::proc::AGE_KEY_VAR;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Machine config, read from `config.toml`.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Repo location. `DOTMAN_REPO_PATH` still wins over it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
    /// Profiles this machine opts into.
    pub profiles: Vec<String>,
    /// Replacements for detected machine identity values.
    pub identity: IdentityOverrides,
    /// Template variables.
    pub vars: BTreeMap<String, String>,
    /// What templates may read from the environment.
    pub template: TemplateConfig,
    /// Where system files may be written, and how to escalate.
    pub system: SystemConfig,
    /// Which age executable and identity file to use.
    pub age: AgeConfig,
    /// Credentials this machine stores in plain text on purpose.
    #[serde(skip_serializing_if = "MachineGuard::is_empty")]
    pub guard: MachineGuard,
}

/// The `[guard]` table. It lives here, not in `.dotman.toml`: an entry another machine pushed
/// must never let a credential out of this one.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MachineGuard {
    /// What `add --allow-plain` stores in plain text, by key and rule.
    pub allow: Vec<Allow>,
}

impl MachineGuard {
    /// No entries, so a saved config needs no `[guard]` table.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.allow.is_empty()
    }
}

/// One `[guard] allow` entry. It matches by key and rule, never by line, so an edit elsewhere in
/// the file keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allow {
    /// The key, as `$HOME`-relative as every other key.
    pub path: String,
    /// A content guard rule name, such as `github-token`.
    pub rule: String,
}

/// Each field replaces the detected identity value of the same name; values are lowercased when used.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
// reason: the fields are the config keys, named by the struct doc.
#[allow(missing_docs)]
pub struct IdentityOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distro: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub like: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
}

/// The `[template]` table.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TemplateConfig {
    /// Environment variable names templates may read. Empty means none.
    pub env: Vec<String>,
}

/// The `[system]` table.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemConfig {
    /// Absolute directories outside `$HOME` that system files may be written to. Empty means none.
    pub roots: Vec<PathBuf>,
    /// Privilege-escalation command for the system write step.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalate: Option<String>,
}

/// The `[age]` table.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgeConfig {
    /// The `age` or `rage` executable; auto-detected when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Identity file; replaces `$XDG_CONFIG_HOME/dotman/age-identity.txt`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<PathBuf>,
}

const KNOWN_KEYS: &[(&str, &[&str])] = &[
    (
        "",
        &[
            "repo", "profiles", "identity", "vars", "template", "system", "age", "guard",
        ],
    ),
    ("guard", &["allow"]),
    (
        "identity",
        &["host", "user", "os", "distro", "like", "arch"],
    ),
    ("template", &["env"]),
    ("system", &["roots", "escalate"]),
    ("age", &["command", "identity"]),
];

impl Config {
    /// Parses config text, returning the config and the dotted names of keys it does not know.
    ///
    /// # Errors
    ///
    /// Fails when the text is not valid TOML or does not fit the schema, or `[template] env` lists
    /// the age key variable.
    pub fn parse(text: &str) -> Result<(Self, Vec<String>)> {
        let table: toml::Table = toml::from_str(text)?;
        let unknown = unknown_keys(&table);
        let config: Self = table.try_into()?;
        if config.template.env.iter().any(|name| name == AGE_KEY_VAR) {
            bail!("`[template] env` lists {AGE_KEY_VAR}; the age key never reaches a template");
        }
        Ok((config, unknown))
    }

    /// Loads the config; a missing file gives the defaults. Each unknown key is warned about once.
    ///
    /// # Errors
    ///
    /// Fails when the file exists but cannot be read or parsed. A missing file is not an error.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        let (config, unknown) =
            Self::parse(&text).with_context(|| format!("cannot parse {}", path.display()))?;
        for key in unknown {
            output::warning(&format!("{}: unknown key `{key}` ignored", path.display()));
        }
        Ok(config)
    }

    /// Writes the config atomically with mode 0600, creating its directory at 0700. A dry run writes nothing.
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

fn unknown_keys(table: &toml::Table) -> Vec<String> {
    let mut unknown = Vec::new();
    for (section, known) in KNOWN_KEYS {
        let scope = if section.is_empty() {
            Some(table)
        } else {
            table.get(*section).and_then(toml::Value::as_table)
        };
        for key in scope.into_iter().flat_map(toml::Table::keys) {
            if !known.contains(&key.as_str()) {
                unknown.push(if section.is_empty() {
                    key.clone()
                } else {
                    format!("{section}.{key}")
                });
            }
        }
    }
    unknown
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    const FULL: &str = r#"
repo = "/data/dots"
profiles = ["work", "laptop"]

[identity]
host = "box"
arch = "aarch64"

[vars]
email = "a@example.org"

[template]
env = ["EDITOR"]

[system]
roots = ["/etc/foo"]
escalate = "doas"

[age]
command = "rage"
identity = "/keys/id.txt"
"#;

    #[test]
    fn missing_file_is_defaults() {
        let tmp = TempDir::new().unwrap();
        let config = Config::load(&tmp.path().join("none.toml")).unwrap();
        assert_eq!(config, Config::default());
        assert!(config.template.env.is_empty() && config.system.roots.is_empty());
    }

    #[test]
    fn full_file_round_trips() {
        let (config, unknown) = Config::parse(FULL).unwrap();
        assert_eq!(unknown, Vec::<String>::new());
        assert_eq!(config.repo.as_deref(), Some(Path::new("/data/dots")));
        assert_eq!(config.identity.host.as_deref(), Some("box"));
        assert_eq!(
            config.vars.get("email").map(String::as_str),
            Some("a@example.org")
        );
        assert_eq!(config.system.escalate.as_deref(), Some("doas"));

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("dotman/config.toml");
        config.save(&path, false).unwrap();
        assert_eq!(Config::load(&path).unwrap(), config);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn unknown_keys_are_reported_not_fatal() {
        let (config, unknown) = Config::parse(
            "colour = 1\nrepo = \"/r\"\n[age]\ncommand = \"age\"\nbogus = true\n[vars]\nanything = \"goes\"\n",
        )
        .unwrap();
        assert_eq!(unknown, ["colour", "age.bogus"]);
        assert_eq!(config.age.command.as_deref(), Some("age"));
    }

    #[test]
    fn guard_allow_entries_parse_and_round_trip() {
        let (config, unknown) = Config::parse(
            "[guard]\nallow = [{ path = \".config/zsh/local.zsh\", rule = \"github-token\" }]\n",
        )
        .unwrap();
        assert_eq!(unknown, Vec::<String>::new());
        let entry = Allow {
            path: ".config/zsh/local.zsh".to_owned(),
            rule: "github-token".to_owned(),
        };
        assert_eq!(config.guard.allow, [entry]);
        let saved = toml::to_string_pretty(&config).unwrap();
        assert_eq!(Config::parse(&saved).unwrap().0, config);
        assert!(
            !toml::to_string_pretty(&Config::default())
                .unwrap()
                .contains("guard"),
            "an empty allow list is not written"
        );
    }

    #[test]
    fn templates_can_never_read_the_age_key() {
        let err = Config::parse("[template]\nenv = [\"EDITOR\", \"DOTMAN_AGE_KEY\"]\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("DOTMAN_AGE_KEY"), "{err}");
        assert!(Config::parse("[template]\nenv = [\"DOTMAN_AGE_KEY_FILE\"]\n").is_ok());
    }

    #[test]
    fn wrong_types_and_bad_toml_are_errors() {
        assert!(Config::parse("profiles = 3").is_err());
        assert!(Config::parse("repo = [").is_err());
    }

    #[test]
    fn dry_run_save_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("dotman/config.toml");
        Config::default().save(&path, true).unwrap();
        assert!(!tmp.path().join("dotman").exists());
    }
}
