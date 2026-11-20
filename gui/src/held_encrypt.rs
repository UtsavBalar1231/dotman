use serde::{Deserialize, Serialize};
use std::path::Path;

/// A status finding without matched content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub rule: String,
    pub place: String,
    pub line: usize,
}

/// The exact held occurrence selected by the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Occurrence {
    pub scope: String,
    pub source: Option<String>,
    pub reason: String,
    pub finding: Option<Finding>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct NewReview {
    key: String,
    source: String,
    repo_id: String,
    home_hash: String,
    home_mode: u32,
    record_hash: String,
    record_mode: u32,
    creates_recipient_record: bool,
    recipients: Vec<String>,
    policy_hash: String,
    hold: String,
    mode_consequence: String,
    plaintext_history_may_remain: bool,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Source {
    path: String,
    kind: String,
    hash: String,
    mode: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryBinding {
    state_device: u64,
    state_inode: u64,
    state_mode: u32,
    source_device: u64,
    source_inode: u64,
    source_mode: u32,
    existing: Option<RecoveryLocation>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryLocation {
    device: u64,
    inode: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedReview {
    key: String,
    destination: String,
    repo_id: String,
    home_hash: String,
    home_mode: u32,
    selected: String,
    variants: Vec<Source>,
    record_hash: String,
    record_mode: u32,
    creates_recipient_record: bool,
    recipients: Vec<String>,
    policy_hash: String,
    git_hash: String,
    machine_file_hash: String,
    recovery_parent: String,
    recovery_binding: RecoveryBinding,
    creates_backup_directory: bool,
    recovery_consequence: String,
    mode_consequence: String,
    plaintext_history_may_remain: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundReview {
    key: String,
    repo_id: String,
    sources: Vec<Source>,
    home: Option<Source>,
    holds: Vec<(String, String)>,
    context: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedBinding {
    key: String,
    review: BoundReview,
    occurrence: Occurrence,
    source_oids: Vec<String>,
    home_oid: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "review",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum Encryption {
    New(NewReview),
    Managed(ManagedReview),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    key: String,
    occurrence: Occurrence,
    review: Encryption,
    managed_binding: Option<ManagedBinding>,
    context: String,
    content_oid: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Review {
    schema_version: u32,
    record: String,
    operation: String,
    executable: bool,
    key: String,
    occurrence: Occurrence,
    recipients: Vec<String>,
    destination: String,
    plaintext_history_may_remain: bool,
    managed_consequences: Option<ManagedConsequences>,
    snapshot: Option<Snapshot>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManagedConsequences {
    selected_source: String,
    recovery_parent: String,
    recovery_consequence: String,
    mode_consequence: String,
    creates_recipient_record: bool,
    creates_backup_directory: bool,
    state_change: String,
    mode_change: String,
    sources: Vec<Source>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Final {
    record: String,
    outcome: String,
    exit_code: i32,
    error_kind: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Executed {
    schema_version: u32,
    record: String,
    operation: String,
    result: String,
    key: String,
    backup_dir_created: bool,
    private_recovery: Option<Recovery>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    state_backup_run: String,
    file: String,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "The CLI receipt contains five independent effect flags."
)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Effects {
    record_created: bool,
    ciphertext_created: bool,
    state_needs_review: bool,
    backup_dir_created: bool,
    recovery_unavailable: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Partial {
    schema_version: u32,
    record: String,
    code: String,
    message: String,
    effects: Effects,
    private_recovery: Option<Recovery>,
}

fn valid_recovery(value: &Recovery) -> bool {
    let name = &value.state_backup_run;
    value.file == "source"
        && !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Outcome of a verified encryption receipt.
pub enum Execution {
    Complete { recovery: Option<RecoveryHandle> },
    Partial(PartialEffects),
}

/// A validated, native-only recovery run identifier.
pub struct RecoveryHandle(String);

impl RecoveryHandle {
    /// The validated identifier, for joining to the reviewed recovery parent.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Verified effects of a partial encryption operation.
#[expect(
    clippy::struct_excessive_bools,
    reason = "The receipt reports five independent effects."
)]
pub struct PartialEffects {
    pub record_created: bool,
    pub ciphertext_created: bool,
    pub state_needs_review: bool,
    pub backup_dir_created: bool,
    pub recovery_unavailable: bool,
    pub recovery: Option<RecoveryHandle>,
}

fn records(bytes: &[u8], status: Option<i32>) -> Result<&[u8], &'static str> {
    if status != Some(0) || bytes.len() > 65_536 {
        return Err("Encryption receipt could not be verified");
    }
    let mut lines = bytes.split(|byte| *byte == b'\n');
    let first = lines.next().ok_or("Missing receipt")?;
    let last = lines.next().ok_or("Missing final result")?;
    if lines.next() != Some(&[][..]) || lines.next().is_some() {
        return Err("Invalid receipt framing");
    }
    let final_result: Final = serde_json::from_slice(last).map_err(|_| "Invalid final result")?;
    if final_result.record != "result"
        || final_result.outcome != "done"
        || final_result.exit_code != 0
        || final_result.error_kind.is_some()
    {
        return Err("Encryption failed");
    }
    Ok(first)
}

fn hex(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_recipients(values: &[String]) -> bool {
    !values.is_empty()
        && values.iter().all(|value| {
            value.len() == 62
                && value.starts_with("age1")
                && value
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

fn machine_snapshot_valid(value: &str) -> bool {
    value == "missing"
        || value.split_once(':').is_some_and(|(hash, mode)| {
            hex(hash, &[32]) && mode.len() == 4 && mode.bytes().all(|b| (b'0'..=b'7').contains(&b))
        })
}

fn managed_finding_matches(entry: &crate::protocol::StatusEntry, occurrence: &Occurrence) -> bool {
    match (
        entry.finding.as_ref(),
        occurrence.finding.as_ref(),
        occurrence.scope.as_str(),
    ) {
        (Some(actual), Some(expected), "home") => {
            matches!(actual.place, crate::protocol::FindingPlace::Content)
                && actual.rule == expected.rule
                && actual.line == expected.line
                && expected.place == "content"
                && expected.line > 0
        }
        (_, Some(expected), "repo") => {
            expected.place == "content"
                && expected.line > 0
                && !expected.rule.is_empty()
                && expected.rule.len() <= 128
                && expected
                    .rule
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        }
        _ => false,
    }
}

fn managed_sources_match(
    review: &ManagedReview,
    binding: &ManagedBinding,
    effects: &ManagedConsequences,
    entry: &crate::protocol::StatusEntry,
    selected: &str,
) -> bool {
    let mut expected: Vec<_> = entry
        .alternatives
        .iter()
        .map(|value| value.raw.as_str())
        .collect();
    expected.sort_unstable();
    let actual: Vec<_> = review
        .variants
        .iter()
        .map(|source| source.path.as_str())
        .collect();
    let Some(selected_source) = review
        .variants
        .iter()
        .find(|source| source.path == selected)
    else {
        return false;
    };
    let Some(home) = binding.review.home.as_ref() else {
        return false;
    };
    let tuple_matches = |observation: &crate::protocol::StatusObservation, source: &Source| {
        matches!(observation, crate::protocol::StatusObservation::Present { tuple }
            if tuple.kind == source.kind && tuple.hash == source.hash && tuple.mode == format!("{:04o}", source.mode))
    };
    !expected.is_empty()
        && !expected.windows(2).any(|pair| pair.first() == pair.get(1))
        && actual == expected
        && review.variants == binding.review.sources
        && review.variants == effects.sources
        && review.variants.iter().all(|s| {
            s.kind == "file"
                && crate::protocol::literal_relative(Path::new(&s.path))
                && hex(&s.hash, &[32])
                && s.mode <= 0o7777
        })
        && tuple_matches(&entry.raw_h, home)
        && home.path == entry.key.raw
        && tuple_matches(&entry.source_tuple, selected_source)
        && entry
            .physical_plain
            .as_ref()
            .is_none_or(|observation| tuple_matches(observation, selected_source))
        && home.hash == review.home_hash
        && home.mode == review.home_mode
        && binding.source_oids.len() == actual.len()
        && binding.source_oids.iter().all(|oid| hex(oid, &[40, 64]))
        && review.recovery_binding.source_mode == selected_source.mode
}

/// A native-only encryption review.
pub struct HeldEncryption {
    snapshot: Snapshot,
}

/// The managed paths and recovery effect, available only to native confirmation.
pub struct ManagedEffects<'a> {
    /// Exact selected repository source.
    pub selected: &'a str,
    /// Private directory that will contain the recovery run.
    pub recovery_parent: &'a str,
    /// Whether execution will create the private directory.
    pub creates_directory: bool,
    /// All reviewed repository sources and their modes.
    pub sources: Vec<(&'a str, u32)>,
}

impl HeldEncryption {
    /// Parse and bind the CLI review to the retained occurrence.
    ///
    /// # Errors
    /// Rejects malformed or mismatched reviews.
    pub fn parse(
        bytes: &[u8],
        status: Option<i32>,
        key: &str,
        repo_id: &str,
        occurrence: &Occurrence,
    ) -> Result<Self, &'static str> {
        let wire: Review = serde_json::from_slice(records(bytes, status)?)
            .map_err(|_| "Invalid encryption review")?;
        let snapshot = wire.snapshot.ok_or("Encryption is unavailable")?;
        let Encryption::New(review) = &snapshot.review else {
            return Err("Managed review is not a new-file review");
        };
        if wire.schema_version != 1
            || wire.record != "remedy_review"
            || wire.operation != "review_encrypt"
            || !wire.executable
            || wire.key != key
            || &wire.occurrence != occurrence
            || occurrence.scope != "new"
            || occurrence.source.is_some()
            || !wire.plaintext_history_may_remain
            || wire.managed_consequences.is_some()
            || snapshot.key != key
            || &snapshot.occurrence != occurrence
            || snapshot.managed_binding.is_some()
            || review.key != key
            || review.repo_id != repo_id
            || review.source != wire.destination
            || !crate::protocol::literal_relative(Path::new(&review.source))
            || review.recipients != wire.recipients
            || !valid_recipients(&wire.recipients)
            || !matches!(review.home_mode, 0o600 | 0o644)
            || !review.plaintext_history_may_remain
            || review.creates_recipient_record != (review.record_mode == u32::MAX)
            || !hex(&review.home_hash, &[32])
            || !hex(&review.policy_hash, &[32])
            || !hex(&snapshot.context, &[40, 64])
            || !hex(&snapshot.content_oid, &[40, 64])
            || if review.creates_recipient_record {
                review.record_hash != "missing"
            } else {
                !hex(&review.record_hash, &[32]) || review.record_mode > 0o7777
            }
            || review.hold
                != if occurrence.reason == "secret_binary" {
                    "secret_name"
                } else {
                    &occurrence.reason
                }
        {
            return Err("Encryption review differs from selected occurrence");
        }
        Ok(Self { snapshot })
    }

    /// Parse a managed review against the selected Home status observation.
    ///
    /// # Errors
    /// Rejects incomplete or mismatched source, occurrence, context and effect data.
    pub fn parse_managed(
        bytes: &[u8],
        status: Option<i32>,
        context: &crate::protocol::StatusContext,
        entry: &crate::protocol::StatusEntry,
        occurrence: &Occurrence,
    ) -> Result<Self, &'static str> {
        let invalid = "Managed encryption review differs from status";
        let wire: Review = serde_json::from_slice(records(bytes, status)?).map_err(|_| invalid)?;
        let snapshot = wire.snapshot.ok_or(invalid)?;
        let Encryption::Managed(review) = &snapshot.review else {
            return Err(invalid);
        };
        let binding = snapshot.managed_binding.as_ref().ok_or(invalid)?;
        let effects = wire.managed_consequences.as_ref().ok_or(invalid)?;
        let selected = entry.source.as_ref().ok_or(invalid)?.raw.as_str();
        let repo_id = &context.repo_id.as_ref().ok_or(invalid)?.raw;
        let recovery = context.state.path.parent().ok_or(invalid)?.join("backups");
        if entry.scope != "home"
            || (occurrence.scope == "home" && entry.status != "held-in-home")
            || (occurrence.scope == "repo"
                && !matches!(
                    entry.status.as_str(),
                    "clean" | "changed-locally" | "held-in-home"
                ))
            || entry.encrypted
            || entry.template
            || entry.system
            || entry.identity_changed
            || entry.key.raw != wire.key
            || !matches!(occurrence.scope.as_str(), "home" | "repo")
            || occurrence.source.as_deref() != Some(selected)
            || occurrence.reason != "content"
            || !managed_finding_matches(entry, occurrence)
            || wire.schema_version != 1
            || wire.record != "remedy_review"
            || wire.operation != "review_encrypt"
            || !wire.executable
            || &wire.occurrence != occurrence
            || snapshot.occurrence != *occurrence
            || snapshot.key != entry.key.raw
            || binding.key != entry.key.raw
            || binding.review.key != entry.key.raw
            || binding.occurrence != *occurrence
            || binding.review.repo_id != *repo_id
            || review.repo_id != *repo_id
            || review.key != entry.key.raw
            || review.selected != selected
            || review.destination != wire.destination
            || review.selected != effects.selected_source
            || !crate::protocol::literal_relative(Path::new(&review.destination))
            || !managed_sources_match(review, binding, effects, entry, selected)
            || binding
                .home_oid
                .as_ref()
                .is_none_or(|oid| !hex(oid, &[40, 64]))
            || !hex(&binding.review.context, &[32])
            || !hex(&snapshot.context, &[40, 64])
            || !hex(&snapshot.content_oid, &[40, 64])
            || !hex(&review.policy_hash, &[32])
            || !hex(&review.git_hash, &[32])
            || !machine_snapshot_valid(&review.machine_file_hash)
            || review.recipients != wire.recipients
            || !valid_recipients(&wire.recipients)
            || !matches!(review.home_mode, 0o600 | 0o644)
            || !review.plaintext_history_may_remain
            || !wire.plaintext_history_may_remain
            || review.creates_recipient_record != effects.creates_recipient_record
            || review.creates_backup_directory != effects.creates_backup_directory
            || review.creates_recipient_record != (review.record_mode == u32::MAX)
            || (review.creates_recipient_record && review.record_hash != "missing")
            || (!review.creates_recipient_record
                && (!hex(&review.record_hash, &[32]) || review.record_mode > 0o7777))
            || Path::new(&review.recovery_parent) != recovery
            || review.recovery_parent != effects.recovery_parent
            || review.recovery_consequence != effects.recovery_consequence
            || review.mode_consequence != effects.mode_consequence
            || effects.state_change != "replace_recorded_entry"
            || effects.mode_change != "none"
            || review.recovery_binding.state_mode != 0o700
            || review.recovery_binding.state_device != review.recovery_binding.source_device
            || review.creates_backup_directory != review.recovery_binding.existing.is_none()
            || review
                .recovery_binding
                .existing
                .as_ref()
                .is_some_and(|location| {
                    location.device != review.recovery_binding.state_device || location.inode == 0
                })
        {
            return Err(invalid);
        }
        Ok(Self { snapshot })
    }

    /// Managed conversion details for native confirmation only.
    #[must_use]
    pub fn managed_effects(&self) -> Option<ManagedEffects<'_>> {
        let Encryption::Managed(review) = &self.snapshot.review else {
            return None;
        };
        Some(ManagedEffects {
            selected: &review.selected,
            recovery_parent: &review.recovery_parent,
            creates_directory: review.creates_backup_directory,
            sources: review
                .variants
                .iter()
                .map(|source| (source.path.as_str(), source.mode))
                .collect(),
        })
    }

    /// Reviewed repository destination.
    #[must_use]
    pub fn destination(&self) -> &str {
        match &self.snapshot.review {
            Encryption::New(review) => &review.source,
            Encryption::Managed(review) => &review.destination,
        }
    }
    /// Reviewed recipient set.
    #[must_use]
    pub fn recipients(&self) -> &[String] {
        match &self.snapshot.review {
            Encryption::New(review) => &review.recipients,
            Encryption::Managed(review) => &review.recipients,
        }
    }
    /// Home file mode observed by the preflight.
    #[must_use]
    pub const fn mode(&self) -> u32 {
        match &self.snapshot.review {
            Encryption::New(review) => review.home_mode,
            Encryption::Managed(review) => review.home_mode,
        }
    }
    /// Whether this creates the recipient record.
    #[must_use]
    pub const fn creates_record(&self) -> bool {
        match &self.snapshot.review {
            Encryption::New(review) => review.creates_recipient_record,
            Encryption::Managed(review) => review.creates_recipient_record,
        }
    }
    /// Compare the preflight with any observed Home tuple.
    #[must_use]
    pub fn matches_home(&self, observation: &crate::protocol::StatusObservation) -> bool {
        let (hash, mode) = match &self.snapshot.review {
            Encryption::New(review) => (&review.home_hash, review.home_mode),
            Encryption::Managed(review) => (&review.home_hash, review.home_mode),
        };
        match observation {
            crate::protocol::StatusObservation::Unavailable { .. } => true,
            crate::protocol::StatusObservation::Present { tuple } => {
                tuple.kind == "file" && &tuple.hash == hash && tuple.mode == format!("{mode:04o}")
            }
            _ => false,
        }
    }
    /// Encode the fixed execution request.
    ///
    /// # Errors
    /// Fails if the request cannot be serialized.
    pub fn execute_request(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(
            &serde_json::json!({"schema_version":1,"operation":"execute_encrypt","key":self.snapshot.key,"occurrence":self.snapshot.occurrence,"snapshot":self.snapshot}),
        )
    }

    /// Validate a success or partial receipt without disclosing recovery handles.
    ///
    /// # Errors
    /// Rejects malformed or uncertain receipts.
    pub fn execution(&self, bytes: &[u8], status: Option<i32>) -> Result<Execution, &'static str> {
        let code = status.ok_or("Invalid encryption result")?;
        if code == 0 {
            let result: Executed = serde_json::from_slice(records(bytes, status)?)
                .map_err(|_| "Invalid encryption result")?;
            let recovery_ok = match (&self.snapshot.review, result.private_recovery.as_ref()) {
                (Encryption::New(_), None) => !result.backup_dir_created,
                (Encryption::Managed(review), Some(value)) => {
                    valid_recovery(value)
                        && result.backup_dir_created == review.creates_backup_directory
                }
                _ => false,
            };
            if result.schema_version != 1
                || result.record != "remedy_result"
                || result.operation != "execute_encrypt"
                || result.result != "encrypted"
                || result.key != self.snapshot.key
                || !recovery_ok
            {
                return Err("Invalid encryption result");
            }
            return Ok(Execution::Complete {
                recovery: result
                    .private_recovery
                    .map(|value| RecoveryHandle(value.state_backup_run)),
            });
        }
        if bytes.len() > 65_536 {
            return Err("Invalid encryption result");
        }
        let mut lines = bytes.split(|byte| *byte == b'\n');
        let first = lines.next().ok_or("Invalid encryption result")?;
        let final_line = lines.next().ok_or("Invalid encryption result")?;
        if lines.next() != Some(&[][..]) || lines.next().is_some() {
            return Err("Invalid encryption result");
        }
        let final_result: Final =
            serde_json::from_slice(final_line).map_err(|_| "Invalid encryption result")?;
        let result: Partial =
            serde_json::from_slice(first).map_err(|_| "Invalid encryption result")?;
        if final_result.record != "result"
            || final_result.outcome != "error"
            || final_result.exit_code != code
            || result.schema_version != 1
            || result.record != "remedy_error"
            || result.code != "partial"
            || result.message != "encryption partially completed; refresh status"
            || result
                .private_recovery
                .as_ref()
                .is_some_and(|value| !valid_recovery(value))
            || (result.effects.recovery_unavailable && result.private_recovery.is_some())
            || (result.effects.backup_dir_created
                && !matches!(&self.snapshot.review, Encryption::Managed(review) if review.creates_backup_directory))
            || (result.effects.record_created && !self.creates_record())
            || (matches!(&self.snapshot.review, Encryption::New(_))
                && result.private_recovery.is_some())
        {
            return Err("Invalid encryption result");
        }
        Ok(Execution::Partial(PartialEffects {
            record_created: result.effects.record_created,
            ciphertext_created: result.effects.ciphertext_created,
            state_needs_review: result.effects.state_needs_review,
            backup_dir_created: result.effects.backup_dir_created,
            recovery_unavailable: result.effects.recovery_unavailable,
            recovery: result
                .private_recovery
                .map(|value| RecoveryHandle(value.state_backup_run)),
        }))
    }
}

/// Validate an encryption execution receipt.
///
/// # Errors
/// Rejects nonzero, malformed, or mismatched results.
pub fn executed(bytes: &[u8], status: Option<i32>, key: &str) -> Result<(), &'static str> {
    let value: Executed =
        serde_json::from_slice(records(bytes, status)?).map_err(|_| "Invalid encryption result")?;
    if value.schema_version != 1
        || value.record != "remedy_result"
        || value.operation != "execute_encrypt"
        || value.result != "encrypted"
        || value.key != key
        || value.backup_dir_created
        || value.private_recovery.is_some()
    {
        return Err("Encryption result differs");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_rejects_changed_occurrence_recipients_and_exit() -> Result<(), &'static str> {
        let occurrence = Occurrence {
            scope: "new".into(),
            source: None,
            reason: "content".into(),
            finding: Some(Finding {
                rule: "github-token".into(),
                place: "content".into(),
                line: 1,
            }),
        };
        let recipient = format!("age1{}", "a".repeat(58));
        let mut wire = serde_json::json!({
            "schema_version":1,"record":"remedy_review","operation":"review_encrypt","executable":true,
            "key":"file","occurrence":occurrence,"recipients":[recipient],"destination":"file{age}",
            "plaintext_history_may_remain":true,"managed_consequences":null,
            "snapshot":{"key":"file","occurrence":occurrence,"managed_binding":null,
                "context":"a".repeat(40),"content_oid":"b".repeat(40),
                "review":{"kind":"new","review":{"key":"file","source":"file{age}","repo_id":"id",
                    "home_hash":"c".repeat(32),"home_mode":384,"record_hash":"missing","record_mode":u32::MAX,
                    "creates_recipient_record":true,"recipients":[recipient],"policy_hash":"d".repeat(32),
                    "hold":"content","mode_consequence":"0600","plaintext_history_may_remain":true}}}
        });
        let finish = r#"{"record":"result","outcome":"done","exit_code":0,"error_kind":null}"#;
        let output = |wire: &serde_json::Value| format!("{wire}\n{finish}\n");
        assert!(
            HeldEncryption::parse(output(&wire).as_bytes(), Some(0), "file", "id", &occurrence)
                .is_ok()
        );
        assert!(
            HeldEncryption::parse(output(&wire).as_bytes(), Some(1), "file", "id", &occurrence)
                .is_err()
        );
        *wire
            .pointer_mut("/occurrence/finding/line")
            .ok_or("missing finding line")? = 2.into();
        assert!(
            HeldEncryption::parse(output(&wire).as_bytes(), Some(0), "file", "id", &occurrence)
                .is_err()
        );
        *wire
            .pointer_mut("/occurrence/finding/line")
            .ok_or("missing finding line")? = 1.into();
        *wire.get_mut("recipients").ok_or("missing recipients")? =
            serde_json::json!(["not-a-recipient"]);
        assert!(
            HeldEncryption::parse(output(&wire).as_bytes(), Some(0), "file", "id", &occurrence)
                .is_err()
        );
        Ok(())
    }
}
