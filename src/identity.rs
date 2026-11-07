//! The machine identity that variant conditions are matched against.

use crate::config::Config;
use crate::state::IdentitySnapshot;
use crate::sys;
use std::ffi::OsString;
use std::fs;
use std::process::{Command, Stdio};

/// Detected values, replaced by the machine config's `[identity]` and `profiles`, then by
/// `DOTMAN_HOST` and `DOTMAN_PROFILES` (comma-separated). Every value is lowercased.
#[must_use]
pub fn current(config: &Config) -> IdentitySnapshot {
    resolve(&Probe::system(), config, |name| std::env::var_os(name))
}

/// The fields that differ, as `host a -> b`, for telling the user what changed.
#[must_use]
pub fn describe_change(old: &IdentitySnapshot, new: &IdentitySnapshot) -> String {
    let (old_profiles, new_profiles) = (old.profiles.join(","), new.profiles.join(","));
    let fields = [
        ("host", old.host.as_str(), new.host.as_str()),
        ("user", &old.user, &new.user),
        ("os", &old.os, &new.os),
        ("distro", &old.distro, &new.distro),
        ("like", &old.like, &new.like),
        ("arch", &old.arch, &new.arch),
        ("profiles", &old_profiles, &new_profiles),
    ];
    let shown = |value: &str| if value.is_empty() { "(none)" } else { value }.to_owned();
    fields
        .iter()
        .filter(|(_, a, b)| a != b)
        .map(|(name, a, b)| format!("{name} {} -> {}", shown(a), shown(b)))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Default)]
struct Probe {
    os: &'static str,
    host: Option<String>,
    user: Option<String>,
    kernel_release: Option<String>,
    os_release: Option<String>,
    arch: Option<String>,
}

impl Probe {
    fn system() -> Self {
        let os = if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else {
            std::env::consts::OS
        };
        let host = if os == "macos" {
            local_host_name().or_else(sys::hostname)
        } else {
            sys::hostname()
        };
        Self {
            os,
            host,
            user: sys::passwd_by_uid(sys::geteuid()).map(|pw| pw.name),
            kernel_release: fs::read_to_string("/proc/sys/kernel/osrelease").ok(),
            os_release: ["/etc/os-release", "/usr/lib/os-release"]
                .iter()
                .find_map(|path| fs::read_to_string(path).ok()),
            arch: sys::machine(),
        }
    }
}

// `uname -n` on macOS follows DHCP and network names; LocalHostName is the one the user set.
fn local_host_name() -> Option<String> {
    let out = Command::new("/usr/sbin/scutil")
        .args(["--get", "LocalHostName"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !name.is_empty()).then_some(name)
}

fn resolve(
    probe: &Probe,
    config: &Config,
    env: impl Fn(&str) -> Option<OsString>,
) -> IdentitySnapshot {
    let wsl = probe.os == "linux"
        && probe
            .kernel_release
            .as_deref()
            .is_some_and(|release| release.to_lowercase().contains("microsoft"));
    let (distro, like) = if probe.os == "macos" {
        ("macos".to_owned(), String::new())
    } else {
        probe
            .os_release
            .as_deref()
            .map(parse_os_release)
            .unwrap_or_default()
    };
    let host = probe
        .host
        .as_deref()
        .and_then(|name| name.split('.').next())
        .unwrap_or_default();
    let over = &config.identity;
    let value =
        |over: Option<&String>, detected: &str| clean(over.map_or(detected, String::as_str));
    let mut identity = IdentitySnapshot {
        host: value(over.host.as_ref(), host),
        user: value(
            over.user.as_ref(),
            probe.user.as_deref().unwrap_or_default(),
        ),
        os: value(over.os.as_ref(), if wsl { "wsl" } else { probe.os }),
        distro: value(over.distro.as_ref(), &distro),
        like: value(over.like.as_ref(), &like)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        arch: value(
            over.arch.as_ref(),
            probe.arch.as_deref().unwrap_or_default(),
        ),
        profiles: list(config.profiles.iter().map(String::as_str)),
    };
    // An empty variable is unset, like `DOTMAN_REPO_PATH`.
    let set = |name: &str| {
        env(name)
            .and_then(|v| v.into_string().ok())
            .filter(|v| !v.trim().is_empty())
    };
    if let Some(host) = set("DOTMAN_HOST") {
        identity.host = clean(&host);
    }
    if let Some(profiles) = set("DOTMAN_PROFILES") {
        identity.profiles = list(profiles.split(','));
    }
    identity
}

fn clean(value: &str) -> String {
    value.trim().to_lowercase()
}

fn list<'a>(values: impl Iterator<Item = &'a str>) -> Vec<String> {
    values
        .map(clean)
        .filter(|value| !value.is_empty())
        .collect()
}

// os-release(5) looks like shell but is parsed, never sourced: only `KEY=VALUE` lines count, with
// one pair of quotes removed.
fn parse_os_release(text: &str) -> (String, String) {
    let (mut id, mut like) = (String::new(), String::new());
    for line in text.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        let value = value.trim();
        let value = ['"', '\'']
            .iter()
            .find_map(|q| value.strip_prefix(*q).and_then(|v| v.strip_suffix(*q)))
            .unwrap_or(value);
        match key.trim() {
            "ID" => value.clone_into(&mut id),
            "ID_LIKE" => value.clone_into(&mut like),
            _ => {}
        }
    }
    (id, like)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::IdentityOverrides;

    const ARCH: &str = "NAME=\"Arch Linux\"\nID=arch\n# ID=commented\nBUILD_ID=rolling\n";
    const UBUNTU: &str = "ID=ubuntu\nID_LIKE='Debian  extra'\nVERSION_ID=\"24.04\"\n";

    fn linux(host: &str, os_release: &str) -> Probe {
        Probe {
            os: "linux",
            host: Some(host.to_owned()),
            user: Some("Alice".to_owned()),
            kernel_release: Some("6.10.2-arch1-1\n".to_owned()),
            os_release: Some(os_release.to_owned()),
            arch: Some("x86_64".to_owned()),
        }
    }

    fn no_env(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn detected_values_are_parsed_and_lowercased() {
        let id = resolve(&linux("Box.Example.ORG", ARCH), &Config::default(), no_env);
        assert_eq!(id.host, "box");
        assert_eq!(id.user, "alice");
        assert_eq!(id.os, "linux");
        assert_eq!(id.distro, "arch");
        assert_eq!(id.like, "");
        assert_eq!(id.arch, "x86_64");
        assert_eq!(id.profiles, Vec::<String>::new());

        let id = resolve(&linux("box", UBUNTU), &Config::default(), no_env);
        assert_eq!(id.distro, "ubuntu");
        assert_eq!(id.like, "debian extra");
    }

    #[test]
    fn a_microsoft_kernel_is_wsl() {
        for release in [
            "5.15.153.1-microsoft-standard-WSL2",
            "4.4.0-19041-Microsoft",
        ] {
            let probe = Probe {
                kernel_release: Some(release.to_owned()),
                ..linux("box", UBUNTU)
            };
            assert_eq!(
                resolve(&probe, &Config::default(), no_env).os,
                "wsl",
                "{release}"
            );
        }
        let mac = Probe {
            os: "macos",
            kernel_release: Some("microsoft".to_owned()),
            os_release: Some(ARCH.to_owned()),
            ..Probe::default()
        };
        let id = resolve(&mac, &Config::default(), no_env);
        assert_eq!((id.os.as_str(), id.distro.as_str()), ("macos", "macos"));
        assert_eq!(id.like, "");
    }

    #[test]
    fn config_overrides_then_environment() {
        let config = Config {
            profiles: vec!["Work".to_owned(), " ".to_owned()],
            identity: IdentityOverrides {
                host: Some("Desk".to_owned()),
                os: Some("WSL".to_owned()),
                like: Some("Arch".to_owned()),
                ..IdentityOverrides::default()
            },
            ..Config::default()
        };
        let id = resolve(&linux("box", UBUNTU), &config, no_env);
        assert_eq!(id.host, "desk");
        assert_eq!(id.os, "wsl");
        assert_eq!(id.like, "arch");
        assert_eq!(id.distro, "ubuntu");
        assert_eq!(id.profiles, ["work"]);

        let env = |name: &str| match name {
            "DOTMAN_HOST" => Some(OsString::from(" Container ")),
            "DOTMAN_PROFILES" => Some(OsString::from("home, Laptop,,")),
            _ => None,
        };
        let id = resolve(&linux("box", UBUNTU), &config, env);
        assert_eq!(id.host, "container");
        assert_eq!(id.profiles, ["home", "laptop"]);

        let empty = |_: &str| Some(OsString::new());
        let id = resolve(&linux("box", UBUNTU), &config, empty);
        assert_eq!(id.host, "desk");
        assert_eq!(id.profiles, ["work"]);
    }

    #[test]
    fn missing_inputs_give_empty_values() {
        let probe = Probe {
            os: "linux",
            ..Probe::default()
        };
        let id = resolve(&probe, &Config::default(), no_env);
        assert_eq!(id.os, "linux");
        assert!(id.host.is_empty() && id.user.is_empty() && id.distro.is_empty());
        assert_eq!(id.arch, "");
    }

    #[test]
    fn changes_name_each_differing_field() {
        let old = resolve(&linux("a", ARCH), &Config::default(), no_env);
        let new = IdentitySnapshot {
            host: "b".to_owned(),
            profiles: vec!["work".to_owned()],
            ..old.clone()
        };
        assert_eq!(
            describe_change(&old, &new),
            "host a -> b, profiles (none) -> work"
        );
        assert_eq!(describe_change(&old, &old), "");
    }
}
