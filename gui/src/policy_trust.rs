//! Bounded standalone directory and ignore trust reviews.

use crate::protocol::{self, StatusContext, StatusReport};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const RENDER_LIMIT: usize = 4096;
const DOCUMENT_LIMIT: usize = 1024 * 1024;
const MAX_ITEMS: usize = 16;

/// A fixed local trust action.
#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Approve the unapproved directory set.
    Directories,
    /// Release retained ignore rules.
    Ignores,
}

impl Action {
    /// Fixed CLI selector.
    #[must_use]
    pub const fn argument(self) -> &'static str {
        match self {
            Self::Directories => "directories",
            Self::Ignores => "ignores",
        }
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Tuple {
    kind: String,
    hash: String,
    mode: u32,
}

impl Tuple {
    fn valid(&self) -> bool {
        match self.kind.as_str() {
            "file" | "symlink" => {
                self.mode <= 0o7777
                    && (self.kind != "symlink" || self.mode == 0o777)
                    && self.hash.len() == 32
                    && self
                        .hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }
            "directory" => self.mode <= 0o7777 && self.hash.is_empty(),
            "missing" => self.mode == 0 && self.hash.is_empty(),
            _ => false,
        }
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Candidate {
    key: String,
    disposition: String,
    tuple: Tuple,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Control {
    kind: String,
    hash: String,
    mode: u32,
    dev: u64,
    ino: u64,
    uid: u32,
    gid: u32,
    nlink: u64,
}

impl Control {
    fn valid(&self) -> bool {
        match self.kind.as_str() {
            "file" => {
                self.mode <= 0o7777
                    && self.nlink > 0
                    && self.hash.len() == 32
                    && self
                        .hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }
            "missing" => {
                self.hash.is_empty()
                    && self.mode == 0
                    && self.dev == 0
                    && self.ino == 0
                    && self.uid == 0
                    && self.gid == 0
                    && self.nlink == 0
            }
            _ => false,
        }
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    action: String,
    paths: TrustPaths,
    lock_dev: u64,
    lock_ino: u64,
    inventory_hash: String,
    directories: Vec<String>,
    retained: Vec<String>,
    current: Vec<String>,
    state: Control,
    config: Control,
    machine_config: Control,
    candidates: Vec<Candidate>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewRecord {
    record: String,
    review: Snapshot,
    display_policy: String,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged, deny_unknown_fields)]
enum InputPath {
    Raw { raw: String },
    Bytes { bytes: Vec<u8> },
}

impl InputPath {
    fn matches(&self, path: &Path) -> bool {
        let bytes = match self {
            Self::Raw { raw } => raw.as_bytes(),
            Self::Bytes { bytes } => bytes.as_slice(),
        };
        !bytes.is_empty() && !bytes.contains(&0) && path.as_os_str().as_bytes() == bytes
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TrustPaths {
    state: InputPath,
    machine_config: InputPath,
    repo_config: InputPath,
}

/// A complete preflight retained only by the native core.
pub struct PolicyReview {
    context: StatusContext,
    snapshot: Snapshot,
}

fn path_value(path: &Path) -> Value {
    path.to_str().map_or_else(
        || json!({"bytes": path.as_os_str().as_bytes()}),
        |raw| json!({"raw": raw}),
    )
}

fn safe_value(value: &str) -> bool {
    value.len() <= 96 && !value.chars().any(crate::runner::unsafe_display_char)
}

fn validate_snapshot(snapshot: &Snapshot, action: Action) -> Result<(), &'static str> {
    if snapshot.action != action.argument()
        || snapshot.inventory_hash.len() != 32
        || !snapshot
            .inventory_hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || snapshot
            .directories
            .len()
            .saturating_add(snapshot.retained.len())
            .saturating_add(snapshot.current.len())
            .saturating_add(snapshot.candidates.len())
            > MAX_ITEMS
        || ![&snapshot.state, &snapshot.config, &snapshot.machine_config]
            .iter()
            .all(|t| t.valid())
        || snapshot.state.kind != "file"
        || snapshot.state.mode != 0o600
        || snapshot.state.nlink != 1
    {
        return Err("Unsupported policy review");
    }
    for values in [&snapshot.directories, &snapshot.retained, &snapshot.current] {
        if values.iter().any(|s| !safe_value(s) || s.is_empty()) {
            return Err("Unsupported policy text");
        }
    }
    if snapshot
        .directories
        .iter()
        .any(|key| !protocol::literal_relative(Path::new(key)))
        || snapshot.directories.iter().collect::<BTreeSet<_>>().len() != snapshot.directories.len()
        || snapshot.retained.iter().collect::<BTreeSet<_>>().len() != snapshot.retained.len()
    {
        return Err("Invalid policy set");
    }
    let mut keys = BTreeSet::new();
    for candidate in &snapshot.candidates {
        if !safe_value(&candidate.key)
            || !protocol::literal_relative(Path::new(&candidate.key))
            || !keys.insert(&candidate.key)
            || !candidate.tuple.valid()
            || !matches!(
                (
                    candidate.disposition.as_str(),
                    candidate.tuple.kind.as_str()
                ),
                ("new" | "waiting" | "held", "file" | "symlink")
                    | ("directory", "directory" | "missing")
            )
        {
            return Err("A policy candidate cannot be shown safely");
        }
    }
    Ok(())
}

impl PolicyReview {
    /// Parses an exact successful preflight bound to current status.
    ///
    /// # Errors
    /// Rejects incomplete, stale, sensitive, unsupported or oversized data.
    pub fn parse(
        stdout: &[u8],
        exit: Option<i32>,
        status: &StatusReport,
        action: Action,
        redacted: &BTreeSet<String>,
    ) -> Result<Self, &'static str> {
        if stdout.len() > DOCUMENT_LIMIT || exit != Some(0) || !stdout.ends_with(b"\n") {
            return Err("Invalid policy preflight");
        }
        status
            .context
            .validate("status")
            .map_err(|_| "Invalid status context")?;
        crate::decisions::validate_identity(&status.context)?;
        if !redacted.is_empty() {
            return Err("A sensitive path prevents policy review");
        }
        let lines: Vec<_> = stdout
            .strip_suffix(b"\n")
            .ok_or("Invalid policy preflight")?
            .split(|b| *b == b'\n')
            .collect();
        let [context, review, result] = lines.as_slice() else {
            return Err("Incomplete policy preflight");
        };
        for (line, expected) in [(*context, "context"), (*result, "result")] {
            let header: protocol::Header =
                serde_json::from_slice(line).map_err(|_| "Invalid policy record")?;
            if header.record != expected {
                return Err("Invalid policy record");
            }
        }
        let context: StatusContext =
            serde_json::from_slice(context).map_err(|_| "Invalid policy context")?;
        context
            .validate("apply")
            .map_err(|_| "Invalid policy context")?;
        if !protocol::same_context(&context, &status.context) {
            return Err("Policy context changed");
        }
        let record: ReviewRecord =
            serde_json::from_slice(review).map_err(|_| "Invalid policy review")?;
        let receipt: protocol::ResultRecord =
            serde_json::from_slice(result).map_err(|_| "Invalid policy result")?;
        if record.record != "trust_review"
            || record.display_policy != "all_values_scanned_v1"
            || receipt.outcome != "done"
            || receipt.exit_code != 0
            || receipt.error_kind.is_some()
            || receipt.recovery.is_some()
        {
            return Err("Invalid policy result");
        }
        let snapshot = record.review;
        if !snapshot.paths.state.matches(&context.state.path)
            || !snapshot
                .paths
                .repo_config
                .matches(&context.repo_config.path)
            || !snapshot.paths.machine_config.matches(&context.config.path)
        {
            return Err("Policy paths changed");
        }
        validate_snapshot(&snapshot, action)?;
        let review = Self { context, snapshot };
        review.modal()?;
        review.encode()?;
        Ok(review)
    }

    /// The bound context.
    #[must_use]
    pub const fn context(&self) -> &StatusContext {
        &self.context
    }

    /// Whether this review has anything to grant.
    #[must_use]
    pub fn eligible(&self) -> bool {
        if self.snapshot.action == "directories" {
            !self.snapshot.directories.is_empty()
        } else {
            !self.snapshot.retained.is_empty()
        }
    }

    /// Builds the complete native confirmation.
    ///
    /// # Errors
    /// Rejects oversized confirmation text.
    pub fn modal(&self) -> Result<String, &'static str> {
        fn add(text: &mut String, value: &str) -> Result<(), &'static str> {
            text.push('"');
            for c in value.chars() {
                if crate::runner::unsafe_display_char(c) || matches!(c, '"' | '\\') {
                    text.extend(c.escape_default());
                } else {
                    text.push(c);
                }
                if text.len() > RENDER_LIMIT {
                    return Err("Policy review too large");
                }
            }
            text.push_str("\"\n");
            Ok(())
        }
        let mut text = format!(
            "Grant: {}\n\n",
            if self.snapshot.action == "directories" {
                "Approve the unapproved directories"
            } else {
                "Release the retained ignore rules"
            }
        );
        for (label, path) in [
            ("Repository", &self.context.repo.path),
            ("Home", &self.context.home.path),
        ] {
            if path.as_os_str().as_bytes().len() > 256 {
                return Err("Policy path too large");
            }
            text.push_str(label);
            text.push_str(": ");
            if let Some(value) = path.to_str() {
                add(&mut text, value)?;
            } else {
                text.push_str("Unix path bytes: ");
                for byte in path.as_os_str().as_bytes() {
                    write!(&mut text, "{byte:02x}").map_err(|_| "Cannot render path")?;
                    if text.len() > RENDER_LIMIT {
                        return Err("Policy review too large");
                    }
                }
                text.push('\n');
            }
        }
        for (heading, values) in [
            ("Unapproved directories", &self.snapshot.directories),
            ("Retained ignore rules", &self.snapshot.retained),
            ("Current ignore rules (in order)", &self.snapshot.current),
        ] {
            write!(&mut text, "\n{heading} ({}):\n", values.len())
                .map_err(|_| "Cannot render policy")?;
            for value in values {
                add(&mut text, value)?;
            }
        }
        write!(
            &mut text,
            "\nProjected candidates and directories ({}):\n",
            self.snapshot.candidates.len()
        )
        .map_err(|_| "Cannot render policy")?;
        for candidate in &self.snapshot.candidates {
            write!(
                &mut text,
                "{} · {} · {:04o} · {}: ",
                candidate.disposition,
                candidate.tuple.kind,
                candidate.tuple.mode,
                candidate.tuple.hash
            )
            .map_err(|_| "Cannot render policy")?;
            add(&mut text, &candidate.key)?;
        }
        text.push_str("\nThis grants only the selected local policy. Future reviewed sync may capture eligible files. No home or repository files are changed now. Continue?");
        if text.len() > RENDER_LIMIT {
            return Err("Policy review too large");
        }
        Ok(text)
    }

    /// Encodes the retained preflight as a standalone decision.
    ///
    /// # Errors
    /// Rejects oversized output.
    pub fn encode(&self) -> Result<Vec<u8>, &'static str> {
        let c = &self.context;
        let m = &c.machine;
        let bytes = serde_json::to_vec(&json!({
            "schema_version":1,"repo_id":c.repo_id.as_ref().ok_or("Missing repository identity")?.raw,
            "home":path_value(&c.home.path),"repo":path_value(&c.repo.path),
            "machine":{"host":m.host.raw,"user":m.user.raw,"os":m.os.raw,"distro":m.distro.raw,"like":m.like.raw,"arch":m.arch.raw,"profiles":m.profiles.iter().map(|v| &v.raw).collect::<Vec<_>>()},
            "files":[],"approvals":{},"trust":self.snapshot
        })).map_err(|_| "Cannot encode policy review")?;
        if bytes.len() > DOCUMENT_LIMIT {
            return Err("Policy review too large");
        }
        Ok(bytes)
    }

    /// Checks the exact writer receipt.
    #[must_use]
    pub fn receipt_matches(
        &self,
        stdout: &[u8],
        exit: Option<i32>,
        status: &StatusReport,
        redacted: &BTreeSet<String>,
    ) -> bool {
        Self::parse(
            stdout,
            exit,
            status,
            if self.snapshot.action == "directories" {
                Action::Directories
            } else {
                Action::Ignores
            },
            redacted,
        )
        .is_ok_and(|other| other.snapshot == self.snapshot)
    }

    /// Checks that a new preflight no longer reports the granted set.
    #[must_use]
    pub fn verified(&self, after: &StatusReport, next: &Self) -> bool {
        protocol::same_context(&self.context, &after.context)
            && protocol::same_context(&self.context, &next.context)
            && self.snapshot.action == next.snapshot.action
            && self.snapshot.paths == next.snapshot.paths
            && self.snapshot.lock_dev == next.snapshot.lock_dev
            && self.snapshot.lock_ino == next.snapshot.lock_ino
            && self.snapshot.current == next.snapshot.current
            && self.snapshot.config == next.snapshot.config
            && self.snapshot.machine_config == next.snapshot.machine_config
            && self.snapshot.candidates == next.snapshot.candidates
            && if self.snapshot.action == "directories" {
                next.snapshot.directories.is_empty()
                    && next.snapshot.retained == self.snapshot.retained
            } else {
                next.snapshot.retained.is_empty()
                    && next.snapshot.directories == self.snapshot.directories
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::process::{Command, Output, Stdio};

    fn fixture() -> Result<(StatusReport, String), Box<dyn std::error::Error>> {
        let context = json!({"record":"context","schema_version":1,"experimental":true,"command":"status",
            "home":{"raw":"/real-home","shown":"/different-home"},"repo":{"raw":"/real-repo","shown":"/different-repo"},
            "config":{"raw":"/config","shown":"config"},"repo_config":{"raw":"/repo/config","shown":"config"},
            "state":{"raw":"/state","shown":"state"},"identity":{"raw":"/identity","shown":"identity"},
            "age_key_file":null,"repo_id":{"raw":"id","shown":"id"},
            "machine":{"host":{"raw":"h","shown":"h"},"user":{"raw":"u","shown":"u"},"os":{"raw":"linux","shown":"linux"},
            "distro":{"raw":"","shown":""},"like":{"raw":"","shown":""},"arch":{"raw":"x","shown":"x"},"profiles":[]}});
        let marker = json!({"record":"review_context","hooks":{"standing":"empty"},"remote":{"basis":"unknown","ahead":null,"behind":null}});
        let summary = json!({"record":"summary","rows":0,"clean":0,"new":0,"held":0,"held_in_home":0,"held_in_repo":0,"unrecorded":0});
        let result = json!({"record":"result","outcome":"done","exit_code":0,"error_kind":null});
        let status = format!("{context}\n{marker}\n{summary}\n{result}\n");
        let protocol::StatusReply::Report(status) =
            protocol::parse_status(status.as_bytes(), Some(0))?
        else {
            return Err("status".into());
        };
        let mut apply = context;
        *apply.get_mut("command").ok_or("command")? = json!("apply");
        let tuple = json!({"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":384});
        let control = json!({"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":384,"dev":1,"ino":2,"uid":1000,"gid":1000,"nlink":1});
        let review = json!({"record":"trust_review","review":{
            "action":"ignores","directories":[],"retained":[".old"],"current":[".new"],
            "paths":{"state":{"raw":"/state"},"machine_config":{"raw":"/config"},"repo_config":{"raw":"/repo/config"}},
            "lock_dev":1,"lock_ino":2,
            "inventory_hash":"0123456789abcdef0123456789abcdef",
            "state":control,"config":control,"machine_config":{"kind":"missing","hash":"","mode":0,"dev":0,"ino":0,"uid":0,"gid":0,"nlink":0},
            "candidates":[{"key":".config/example","disposition":"new","tuple":tuple}]},
            "display_policy":"all_values_scanned_v1"});
        Ok((status, format!("{apply}\n{review}\n{result}\n")))
    }

    #[test]
    fn safety_projection_binds_every_value_and_actual_context_path()
    -> Result<(), Box<dyn std::error::Error>> {
        let (status, text) = fixture()?;
        let review = PolicyReview::parse(
            text.as_bytes(),
            Some(0),
            &status,
            Action::Ignores,
            &BTreeSet::new(),
        )?;
        assert!(review.eligible());
        let modal = review.modal()?;
        assert!(modal.contains("/real-home") && modal.contains("/real-repo"));
        assert!(!modal.contains("/different-home") && !modal.contains("/different-repo"));
        let encoded: Value = serde_json::from_slice(&review.encode()?)?;
        assert_eq!(encoded.pointer("/trust/retained"), Some(&json!([".old"])));
        assert!(review.receipt_matches(text.as_bytes(), Some(0), &status, &BTreeSet::new()));
        for invalid in [
            text.replace("\"all_values_scanned_v1\"", "\"unknown\""),
            text.replace("\"0123456789abcdef0123456789abcdef\"", "\"BAD\""),
            text.replace("\"lock_ino\":2", "\"lock_ino\":3"),
            text.replace(
                "\"repo_config\":{\"raw\":\"/repo/config\"}",
                "\"repo_config\":{\"raw\":\"/wrong\"}",
            ),
            text.replace("\"record\":\"result\"", "\"record\":\"other\""),
            text.replace("\"action\":\"ignores\"", "\"action\":\"directories\""),
        ] {
            let parsed = PolicyReview::parse(
                invalid.as_bytes(),
                Some(0),
                &status,
                Action::Ignores,
                &BTreeSet::new(),
            );
            assert!(
                parsed.is_err()
                    || !review.receipt_matches(
                        invalid.as_bytes(),
                        Some(0),
                        &status,
                        &BTreeSet::new()
                    )
            );
            assert!(!review.receipt_matches(
                invalid.as_bytes(),
                Some(0),
                &status,
                &BTreeSet::new()
            ));
        }
        let mut lines = text.lines();
        let context = lines.next().ok_or("context")?;
        let mut record: Value = serde_json::from_str(lines.next().ok_or("review")?)?;
        record
            .as_object_mut()
            .ok_or("record")?
            .remove("display_policy");
        let result = lines.next().ok_or("result")?;
        let unclassified = format!("{context}\n{record}\n{result}\n");
        assert!(
            PolicyReview::parse(
                unclassified.as_bytes(),
                Some(0),
                &status,
                Action::Ignores,
                &BTreeSet::new()
            )
            .is_err()
        );
        assert!(
            PolicyReview::parse(
                text.as_bytes(),
                Some(3),
                &status,
                Action::Ignores,
                &BTreeSet::new()
            )
            .is_err()
        );
        assert!(
            PolicyReview::parse(
                text.as_bytes(),
                Some(0),
                &status,
                Action::Ignores,
                &BTreeSet::from(["sensitive".to_owned()])
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn requires_inventory_and_complete_control_metadata() -> Result<(), Box<dyn std::error::Error>>
    {
        let (status, text) = fixture()?;
        let mut lines = text.lines();
        let context = lines.next().ok_or("context")?;
        let original: Value = serde_json::from_str(lines.next().ok_or("review")?)?;
        let result = lines.next().ok_or("result")?;
        for pointer in ["/review/state", "/review/config", "/review/machine_config"] {
            for field in ["dev", "ino", "uid", "gid", "nlink"] {
                let mut record = original.clone();
                record
                    .pointer_mut(pointer)
                    .and_then(Value::as_object_mut)
                    .ok_or("control")?
                    .remove(field);
                let response = format!("{context}\n{record}\n{result}\n");
                assert!(
                    PolicyReview::parse(
                        response.as_bytes(),
                        Some(0),
                        &status,
                        Action::Ignores,
                        &BTreeSet::new()
                    )
                    .is_err(),
                    "{pointer}/{field}"
                );
            }
        }
        for value in [
            json!(null),
            json!("ABCDEF0123456789abcdef0123456789"),
            json!("short"),
        ] {
            let mut record = original.clone();
            *record
                .pointer_mut("/review/inventory_hash")
                .ok_or("inventory")? = value;
            let response = format!("{context}\n{record}\n{result}\n");
            assert!(
                PolicyReview::parse(
                    response.as_bytes(),
                    Some(0),
                    &status,
                    Action::Ignores,
                    &BTreeSet::new()
                )
                .is_err()
            );
        }
        Ok(())
    }

    struct Fixture(std::path::PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl Fixture {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let root =
                std::env::temp_dir().join(format!("dotman-policy-review-{}", std::process::id()));
            std::fs::create_dir(&root)?;
            let fixture = Self(root);
            for dir in ["home", "config", "data", "state", "cache", "bin"] {
                std::fs::create_dir(fixture.0.join(dir))?;
            }
            std::fs::write(
                fixture.0.join("home/.gitconfig"),
                "[user]\nname = Test\nemail = test@example.invalid\n",
            )?;
            let cli = std::env::var_os("DOTMAN_TEST_CLI").ok_or("missing DOTMAN_TEST_CLI")?;
            std::fs::copy(cli, fixture.0.join("bin/dotman"))?;
            Ok(fixture)
        }

        fn run(
            &self,
            args: &[&str],
            input: Option<&[u8]>,
        ) -> Result<Output, Box<dyn std::error::Error>> {
            let home = self.0.join("home");
            let mut cmd = Command::new(self.0.join("bin/dotman"));
            cmd.env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", self.0.join("config"))
                .env("XDG_DATA_HOME", self.0.join("data"))
                .env("XDG_STATE_HOME", self.0.join("state"))
                .env("XDG_CACHE_HOME", self.0.join("cache"))
                .env("DOTMAN_NON_INTERACTIVE", "1")
                .current_dir(home)
                .args(args)
                .stdin(if input.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = cmd.spawn()?;
            if let Some(input) = input {
                child
                    .stdin
                    .take()
                    .ok_or("missing stdin")?
                    .write_all(input)?;
            }
            Ok(child.wait_with_output()?)
        }

        fn status(&self) -> Result<StatusReport, Box<dyn std::error::Error>> {
            let output = self.run(&["status", "--json"], None)?;
            let protocol::StatusReply::Report(report) =
                protocol::parse_status(&output.stdout, output.status.code())?
            else {
                return Err("status failed".into());
            };
            Ok(report)
        }

        fn preview(
            &self,
            status: &StatusReport,
            action: Action,
        ) -> Result<PolicyReview, Box<dyn std::error::Error>> {
            let output = self.run(
                &[
                    "apply",
                    "--review-trust",
                    action.argument(),
                    "--dry-run",
                    "--json",
                ],
                None,
            )?;
            PolicyReview::parse(
                &output.stdout,
                output.status.code(),
                status,
                action,
                &BTreeSet::new(),
            )
            .map_err(|error| {
                format!(
                    "{error}: exit {:?}: {}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr)
                )
                .into()
            })
        }

        fn prepare_policy(&self, action: Action) -> Result<(), Box<dyn std::error::Error>> {
            let repo = self.0.join("data/dotman");
            let dir = self.0.join("home/.config/app");
            std::fs::create_dir_all(&dir)?;
            if action == Action::Ignores {
                std::fs::write(dir.join("anchor"), "anchor")?;
                assert!(self.run(&["add", ".config/app"], None)?.status.success());
                std::fs::write(
                    repo.join(".dotman.toml"),
                    "[dirs]\npaths = [\".config/app\"]\n[ignore]\npatterns = [\".config/app/ignored*\"]\n",
                )?;
                assert!(self.run(&["sync"], None)?.status.success());
                std::fs::write(
                    repo.join(".dotman.toml"),
                    "[dirs]\npaths = [\".config/app\"]\n[ignore]\npatterns = []\n",
                )?;
                std::fs::write(dir.join("ignored-one"), "one")?;
            } else {
                std::fs::write(
                    repo.join(".dotman.toml"),
                    "[dirs]\npaths = [\".config/app\"]\n",
                )?;
                std::fs::write(dir.join("one"), "one")?;
            }
            Ok(())
        }
    }

    #[test]
    #[ignore = "requires DOTMAN_TEST_CLI pointing to a built CLI"]
    fn real_cli_policy_reviews_deny_stale_and_grant_only_local_state()
    -> Result<(), Box<dyn std::error::Error>> {
        for action in [Action::Directories, Action::Ignores] {
            let fixture = Fixture::new()?;
            let init = fixture.run(&["init"], None)?;
            assert!(
                init.status.success(),
                "{}",
                String::from_utf8_lossy(&init.stderr)
            );
            let baseline = fixture.run(&["sync"], None)?;
            assert!(
                baseline.status.success(),
                "{}",
                String::from_utf8_lossy(&baseline.stderr)
            );
            let home = fixture.0.join("home");
            let repo = fixture.0.join("data/dotman");
            let dir = home.join(".config/app");
            fixture.prepare_policy(action)?;
            let status = fixture.status()?;
            let review = fixture.preview(&status, action)?;
            assert!(review.eligible());
            let _modal = review.modal()?;
            let state_file = fixture.0.join("state/dotman/state.toml");
            let state_before = std::fs::read(&state_file).ok();
            let config_before = std::fs::read(repo.join(".dotman.toml"))?;
            let index_before = std::fs::read(repo.join(".git/index")).ok();
            // A declined confirmation never invokes the writer.
            assert_eq!(std::fs::read(&state_file).ok(), state_before);
            let added = if action == Action::Ignores {
                "ignored-two"
            } else {
                "two"
            };
            std::fs::write(dir.join(added), "two")?;
            let stale = fixture.run(
                &[
                    "apply",
                    "--review-trust",
                    action.argument(),
                    "--decide-from",
                    "-",
                    "--json",
                ],
                Some(&review.encode()?),
            )?;
            assert_eq!(
                stale.status.code(),
                Some(3),
                "{}",
                String::from_utf8_lossy(&stale.stderr)
            );
            assert!(!review.receipt_matches(
                &stale.stdout,
                stale.status.code(),
                &status,
                &BTreeSet::new()
            ));
            assert_eq!(std::fs::read(&state_file).ok(), state_before);
            std::fs::remove_file(dir.join(added))?;
            let status = fixture.status()?;
            let review = fixture.preview(&status, action)?;
            let receipt = fixture.run(
                &[
                    "apply",
                    "--review-trust",
                    action.argument(),
                    "--decide-from",
                    "-",
                    "--json",
                ],
                Some(&review.encode()?),
            )?;
            assert!(
                review.receipt_matches(
                    &receipt.stdout,
                    receipt.status.code(),
                    &status,
                    &BTreeSet::new()
                ),
                "{}",
                String::from_utf8_lossy(&receipt.stderr)
            );
            let after = fixture.status()?;
            let next = fixture.preview(&after, action)?;
            assert!(review.verified(&after, &next));
            assert_eq!(std::fs::read(repo.join(".dotman.toml"))?, config_before);
            assert_eq!(std::fs::read(repo.join(".git/index")).ok(), index_before);
            assert_ne!(std::fs::read(&state_file).ok(), state_before);
            let key = if action == Action::Ignores {
                "ignored-one"
            } else {
                "one"
            };
            assert_eq!(std::fs::read(dir.join(key))?, b"one");
            assert!(!repo.join(".config/app").join(key).exists());
        }
        Ok(())
    }
}
