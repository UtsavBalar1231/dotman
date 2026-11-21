//! Encodes a complete outgoing review and explicit, bounded approvals.
//!
//! Blocked, unavailable and hypothetical ciphertext observations are unsupported.

use crate::protocol::{
    self, StatusContext, StatusEntry, StatusIdentity, StatusObservation, StatusReport, StatusTuple,
    SyncRecord, SyncReview,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const MAX_DOCUMENT: usize = 32 * 1024 * 1024;

/// A validated review. The caller must separately ensure it is still retained and current.
pub struct CompleteReview<'a> {
    review: &'a SyncReview,
}

impl<'a> CompleteReview<'a> {
    pub(crate) fn home(&self) -> &'a Path {
        &self.review.context.home.path
    }

    /// Returns the CLI's opaque selected-operation target binding.
    ///
    /// # Errors
    /// Rejects absent or malformed bindings.
    pub fn selected_target(&self) -> Result<&'a str, &'static str> {
        let binding = self
            .review
            .records
            .iter()
            .find_map(|record| match record {
                SyncRecord::Outgoing {
                    selected_target, ..
                } => selected_target.as_deref(),
                _ => None,
            })
            .ok_or("Selected sync target is unavailable")?;
        if binding == "local"
            || binding.strip_prefix("git-").is_some_and(|hash| {
                matches!(hash.len(), 40 | 64)
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        {
            Ok(binding)
        } else {
            Err("Invalid selected sync target")
        }
    }

    /// Checks the report and its binding to the last accepted status context.
    ///
    /// # Errors
    /// Rejects partial, stale-context, unsupported or malformed review records.
    pub fn new(review: &'a SyncReview, status: &StatusContext) -> Result<Self, &'static str> {
        status
            .validate("status")
            .map_err(|_| "invalid status context")?;
        review
            .context
            .validate("sync")
            .map_err(|_| "invalid review context")?;
        let mass = review.exit_code == 1 && review.error_kind.as_deref() == Some("mass_delete");
        if !review.successful_receipt()
            || !(review.exit_code == 0 && review.error_kind.is_none() || mass)
            || review.recovery.is_some()
            || !protocol::same_context(&review.context, status)
        {
            return Err("review is not a successful bound report");
        }
        validate_identity(&review.context)?;
        validate_records(&review.records)?;
        let sets: Vec<_> = review
            .records
            .iter()
            .filter_map(|record| match record {
                SyncRecord::MassDeleteReview { paths } => Some(paths),
                _ => None,
            })
            .collect();
        if mass {
            let paths = sets.first().ok_or("missing mass deletion set")?;
            let listed: BTreeSet<_> = paths.iter().map(|path| &path.path).collect();
            let edits: BTreeSet<_> = review
                .records
                .iter()
                .filter_map(|record| match record {
                    SyncRecord::RepoEdit(edit) | SyncRecord::Own(edit)
                        if matches!(edit.stored, StatusObservation::Missing) =>
                    {
                        Some(&edit.path.path)
                    }
                    _ => None,
                })
                .collect();
            if sets.len() != 1 || listed.len() != paths.len() || listed != edits {
                return Err("invalid mass deletion set");
            }
        } else if !sets.is_empty() {
            return Err("unexpected mass deletion set");
        }
        Ok(Self { review })
    }

    /// Returns the validated, retained mass-deletion set, if present.
    #[must_use]
    pub fn mass_delete_paths(&self) -> Option<&'a [protocol::StatusPath]> {
        self.review.records.iter().find_map(|record| match record {
            SyncRecord::MassDeleteReview { paths } => Some(paths.as_slice()),
            _ => None,
        })
    }

    /// Produces the bounded v1 stdin document.
    ///
    /// # Errors
    /// Returns an error if serialization fails or the document exceeds the size bound.
    pub fn encode(self) -> Result<Vec<u8>, &'static str> {
        self.encode_selected(None)
    }

    /// Encodes only selected captures, retaining the CLI's raw selectors.
    ///
    /// # Errors
    /// Rejects empty, duplicate, unknown or invalid selectors.
    pub fn encode_selected(
        self,
        selected: Option<&BTreeMap<String, Vec<u32>>>,
    ) -> Result<Vec<u8>, &'static str> {
        self.encode_mode(selected, false)
    }

    pub(crate) fn encode_mode(
        self,
        selected: Option<&BTreeMap<String, Vec<u32>>>,
        mass_delete: bool,
    ) -> Result<Vec<u8>, &'static str> {
        if mass_delete && (selected.is_some() || self.mass_delete_paths().is_none()) {
            return Err("mass deletion approval unavailable");
        }
        let selected_target = selected.map(|_| self.selected_target()).transpose()?;
        if selected.is_some_and(|values| {
            values.is_empty()
                || values.values().any(|ids| {
                    ids.is_empty()
                        || ids.contains(&0)
                        || ids.iter().collect::<BTreeSet<_>>().len() != ids.len()
                })
        }) {
            return Err("invalid selected lines");
        }
        let context = &self.review.context;
        let mut captures = Vec::new();
        let mut outgoing = Vec::new();
        let mut head = None;
        let mut tracking_ref = None;
        for record in &self.review.records {
            match record {
                SyncRecord::Outgoing {
                    head: h,
                    tracking_ref: t,
                    ..
                } => {
                    head = h.as_ref().map(|value| value.raw.as_str());
                    tracking_ref = t.as_ref().map(|value| value.raw.as_str());
                }
                SyncRecord::Capture(capture)
                    if selected.is_none_or(|lines| lines.contains_key(&capture.key.raw)) =>
                {
                    captures.push(Capture {
                        key: &capture.key.raw,
                        source: capture
                            .source
                            .path
                            .to_str()
                            .ok_or("non-UTF-8 capture source")?,
                        logical_r: observed(&capture.logical_r)?,
                        raw_h: observed(&capture.raw_h)?,
                        stored_before: observed(&capture.stored_before)?,
                    });
                }
                SyncRecord::RepoEdit(edit) | SyncRecord::Own(edit) if selected.is_none() => {
                    outgoing.push(Outgoing {
                        path: input_path(&edit.path.path),
                        stored: observed(&edit.stored)?,
                    });
                }
                _ => {}
            }
        }
        if selected.is_some_and(|lines| lines.len() != captures.len()) {
            return Err("unknown selected capture");
        }
        let machine = &context.machine;
        let document = Document {
            schema_version: 1,
            repo_id: &context
                .repo_id
                .as_ref()
                .ok_or("missing repository identity")?
                .raw,
            home: input_path(&context.home.path),
            repo: input_path(&context.repo.path),
            machine: Machine {
                host: &machine.host.raw,
                user: &machine.user.raw,
                os: &machine.os.raw,
                distro: &machine.distro.raw,
                like: &machine.like.raw,
                arch: &machine.arch.raw,
                profiles: machine
                    .profiles
                    .iter()
                    .map(|value| value.raw.as_str())
                    .collect(),
            },
            files: [],
            sync: SyncInput {
                head,
                tracking_ref,
                captures,
                outgoing,
                selected_lines: selected,
                selected_target,
            },
            approvals: self.approvals(mass_delete)?,
        };
        let mut bytes = Bounded(Vec::new());
        serde_json::to_writer(&mut bytes, &document)
            .map_err(|_| "cannot encode bounded decisions")?;
        Ok(bytes.0)
    }

    fn approvals(&self, mass_delete: bool) -> Result<SyncApprovals<'a>, &'static str> {
        let paths = if mass_delete {
            Some(
                self.mass_delete_paths()
                    .ok_or("missing mass deletion set")?
                    .iter()
                    .map(|path| input_path(&path.path))
                    .collect(),
            )
        } else {
            None
        };
        Ok(SyncApprovals { mass_delete: paths })
    }
}

pub(crate) fn validate_identity(context: &StatusContext) -> Result<(), &'static str> {
    let id = context
        .repo_id
        .as_ref()
        .ok_or("missing repository identity")?;
    if id.raw.is_empty() || id.raw.contains('\0') {
        return Err("invalid repository identity");
    }
    validate_machine(&context.machine)
}

fn validate_machine(machine: &StatusIdentity) -> Result<(), &'static str> {
    let mut profiles = BTreeSet::new();
    if machine
        .profiles
        .iter()
        .any(|value| !profiles.insert(&value.raw))
        || [
            &machine.host.raw,
            &machine.user.raw,
            &machine.os.raw,
            &machine.distro.raw,
            &machine.like.raw,
            &machine.arch.raw,
        ]
        .into_iter()
        .any(|value| value.contains('\0'))
        || machine
            .profiles
            .iter()
            .any(|value| value.raw.contains('\0'))
    {
        return Err("invalid machine identity");
    }
    Ok(())
}

/// A complete status-bound identity approval.
pub struct ReviewedIdentity<'a> {
    report: &'a StatusReport,
    current: &'a StatusIdentity,
    recorded: &'a StatusIdentity,
}

impl<'a> ReviewedIdentity<'a> {
    /// Checks the identity and classified home entries in a successful status report.
    ///
    /// # Errors
    /// Rejects missing, unchanged, invalid or unbound identity reviews.
    pub fn new(report: &'a StatusReport) -> Result<Self, &'static str> {
        report
            .context
            .validate("status")
            .map_err(|_| "invalid status context")?;
        validate_identity(&report.context)?;
        let current = report
            .current_identity
            .as_deref()
            .ok_or("missing current identity")?;
        let recorded = report
            .recorded_identity
            .as_deref()
            .ok_or("missing recorded identity")?;
        validate_machine(current)?;
        validate_machine(recorded)?;
        if !protocol::same_identity_raw(current, &report.context.machine)
            || protocol::same_identity_raw(current, recorded)
            || !report.entries.iter().any(|entry| {
                entry.scope == "home"
                    && !entry.system
                    && entry.path.is_none()
                    && entry.identity_changed
                    && entry.status == "identity-changed"
                    && entry.row.as_ref().is_some_and(|row| !row.is_empty())
                    && entry.row_number.is_some()
                    && entry.hold.is_none()
            })
        {
            return Err("identity review is not eligible");
        }
        Ok(Self {
            report,
            current,
            recorded,
        })
    }

    /// The bound status context.
    #[must_use]
    pub const fn context(&self) -> &StatusContext {
        &self.report.context
    }

    /// The current reviewed identity.
    #[must_use]
    pub const fn current(&self) -> &StatusIdentity {
        self.current
    }

    /// The previously recorded reviewed identity.
    #[must_use]
    pub const fn recorded(&self) -> &StatusIdentity {
        self.recorded
    }

    /// Checks that a fresh report records this approval in the same context.
    #[must_use]
    pub fn is_recorded_in(&self, after: &StatusReport) -> bool {
        protocol::same_context(self.context(), &after.context)
            && after
                .recorded_identity
                .as_ref()
                .is_some_and(|recorded| protocol::same_identity_raw(self.current, recorded))
    }

    /// Encodes the identity approval without file decisions.
    ///
    /// # Errors
    /// Returns an error if serialization fails or exceeds the size bound.
    pub fn encode(&self) -> Result<Vec<u8>, &'static str> {
        let context = self.context();
        let document = IdentityDocument {
            schema_version: 1,
            repo_id: &context
                .repo_id
                .as_ref()
                .ok_or("missing repository identity")?
                .raw,
            home: input_path(&context.home.path),
            repo: input_path(&context.repo.path),
            machine: Machine::from(self.current),
            files: [],
            approvals: IdentityApprovals {
                identity: IdentityApproval {
                    recorded: Machine::from(self.recorded),
                    current: Machine::from(self.current),
                },
            },
        };
        let mut bytes = Bounded(Vec::new());
        serde_json::to_writer(&mut bytes, &document)
            .map_err(|_| "cannot encode bounded decisions")?;
        Ok(bytes.0)
    }
}

/// An action supported for an ordinary reviewed file.
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyAction {
    /// Keep the home version.
    KeepLocal,
    /// Write the repository version.
    TakeRepo,
    /// Restore a locally deleted file.
    Restore,
}

/// Validated file decisions. The caller must retain current status and diff receipts and reject
/// keys with supplemental holds from the complete status report.
pub struct ReviewedApply<'a> {
    context: &'a StatusContext,
    choices: Vec<(&'a StatusEntry, ApplyAction)>,
    operands: Vec<PathBuf>,
}

impl<'a> ReviewedApply<'a> {
    pub(crate) fn home(&self) -> &'a Path {
        &self.context.home.path
    }

    /// Checks a nonempty batch of classified status entries.
    ///
    /// # Errors
    /// Rejects invalid context, unsupported entries, actions and duplicate keys.
    pub fn new(
        context: &'a StatusContext,
        choices: Vec<(&'a StatusEntry, ApplyAction)>,
    ) -> Result<Self, &'static str> {
        context
            .validate("status")
            .map_err(|_| "invalid status context")?;
        validate_identity(context)?;
        if [&context.home.path, &context.repo.path]
            .into_iter()
            .any(|path| path.as_os_str().as_bytes().contains(&0))
            || choices.is_empty()
            || choices.len() > 256
        {
            return Err("invalid context path or batch size");
        }
        let mut keys = BTreeSet::new();
        let mut operands = Vec::with_capacity(choices.len());
        for (entry, action) in &choices {
            let key = &entry.key.raw;
            let source = entry.source.as_ref().ok_or("missing source")?;
            if entry.scope != "home"
                || entry.path.is_some()
                || entry.encrypted
                || entry.template
                || entry.system
                || entry.identity_changed
                || !valid_key(key, true)
                || !valid_key(&source.raw, true)
                || !keys.insert(key)
            {
                return Err("unsupported or duplicate entry");
            }
            let row = entry.row.as_deref().ok_or("missing classified row")?;
            if entry.status != row {
                return Err("unsupported status");
            }
            let present_home = match (row, action) {
                ("both-changed" | "existing", ApplyAction::KeepLocal | ApplyAction::TakeRepo)
                | ("changed-in-repo", ApplyAction::TakeRepo) => true,
                ("deleted-locally" | "deleted-changed-upstream", ApplyAction::Restore) => false,
                _ => return Err("unsupported row or action"),
            };
            for observation in [&entry.r, &entry.source_tuple] {
                require_file(observation)?;
            }
            for observation in [&entry.h, &entry.raw_h] {
                if present_home {
                    require_file(observation)?;
                } else if !matches!(observation, StatusObservation::Missing) {
                    return Err("expected missing home");
                }
            }
            operands.push(context.home.path.join(key));
        }
        Ok(Self {
            context,
            choices,
            operands,
        })
    }

    /// Literal absolute operands for the trusted runner.
    #[must_use]
    pub fn operands(&self) -> &[PathBuf] {
        &self.operands
    }

    /// Encodes only the reviewed file decisions and an empty approvals object.
    ///
    /// # Errors
    /// Returns an error if serialization fails or exceeds the size bound.
    pub fn encode(&self) -> Result<Vec<u8>, &'static str> {
        let context = self.context;
        let machine = &context.machine;
        let files = self
            .choices
            .iter()
            .map(|(entry, action)| {
                Ok(FileDecision {
                    key: &entry.key.raw,
                    source: entry.source.as_ref().map(|source| source.raw.as_str()),
                    r: observed(&entry.r)?,
                    source_tuple: observed(&entry.source_tuple)?,
                    h: observed(&entry.h)?,
                    raw_h: observed(&entry.raw_h)?,
                    action: *action,
                })
            })
            .collect::<Result<Vec<_>, &'static str>>()?;
        let document = ApplyDocument {
            schema_version: 1,
            repo_id: &context
                .repo_id
                .as_ref()
                .ok_or("missing repository identity")?
                .raw,
            home: input_path(&context.home.path),
            repo: input_path(&context.repo.path),
            machine: Machine {
                host: &machine.host.raw,
                user: &machine.user.raw,
                os: &machine.os.raw,
                distro: &machine.distro.raw,
                like: &machine.like.raw,
                arch: &machine.arch.raw,
                profiles: machine
                    .profiles
                    .iter()
                    .map(|value| value.raw.as_str())
                    .collect(),
            },
            files,
            approvals: Approvals {},
        };
        let mut bytes = Bounded(Vec::new());
        serde_json::to_writer(&mut bytes, &document)
            .map_err(|_| "cannot encode bounded decisions")?;
        Ok(bytes.0)
    }
}

fn require_file(value: &StatusObservation) -> Result<(), &'static str> {
    observed(value)?;
    if matches!(value, StatusObservation::Present { tuple } if tuple.kind == "file") {
        Ok(())
    } else {
        Err("expected regular file")
    }
}

#[derive(Serialize)]
struct FileDecision<'a> {
    key: &'a str,
    source: Option<&'a str>,
    r: Observed<'a>,
    source_tuple: Observed<'a>,
    h: Observed<'a>,
    raw_h: Observed<'a>,
    action: ApplyAction,
}

#[derive(Serialize)]
struct ApplyDocument<'a> {
    schema_version: u8,
    repo_id: &'a str,
    home: InputPath<'a>,
    repo: InputPath<'a>,
    machine: Machine<'a>,
    files: Vec<FileDecision<'a>>,
    approvals: Approvals,
}

fn validate_records(records: &[SyncRecord]) -> Result<(), &'static str> {
    let mut keys = BTreeSet::new();
    let mut sources = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut present_paths = Vec::new();
    let mut stages = Vec::new();
    let mut outgoing = 0_usize;
    for record in records {
        protocol::validate_sync_record(record).map_err(|_| "invalid review record")?;
        match record {
            SyncRecord::Outgoing {
                head,
                tracking_ref,
                tracking_commit,
                knowledge,
                ..
            } => {
                outgoing = outgoing.saturating_add(1);
                if knowledge != "last_fetched"
                    || head.as_ref().is_some_and(|value| !valid_oid(&value.raw))
                    || tracking_commit
                        .as_ref()
                        .is_some_and(|value| !valid_oid(&value.raw))
                    || tracking_ref
                        .as_ref()
                        .is_some_and(|value| !valid_tracking_ref(&value.raw))
                    || (tracking_commit.is_some() && tracking_ref.is_none())
                {
                    return Err("invalid reviewed head or tracking");
                }
            }
            SyncRecord::Stage { stage, dry_run } => {
                if !dry_run || stage == "blocked_by_repo_hold" {
                    return Err("blocked review");
                }
                if !matches!(
                    stage.as_str(),
                    "capture"
                        | "commit"
                        | "hooks"
                        | "apply"
                        | "reencrypt"
                        | "expected_hooks_current_repo"
                ) {
                    return Err("unsupported review stage");
                }
                if matches!(stage.as_str(), "capture" | "commit" | "hooks") {
                    stages.push(stage.as_str());
                }
            }
            SyncRecord::Capture(capture) => {
                validate_capture(capture, &mut keys, &mut sources)?;
            }
            SyncRecord::RepoEdit(edit) | SyncRecord::Own(edit) => {
                if !valid_outgoing(&edit.path.path)
                    || edit.path.path.starts_with(".dotman/system")
                    || !paths.insert(&edit.path.path)
                    || (edit.basis == "worktree") != edit.observed.is_some()
                {
                    return Err("invalid or duplicate outgoing path");
                }
                observed(&edit.stored)?;
                if let Some(actual) = &edit.observed {
                    observed(actual)?;
                }
                if matches!(edit.stored, StatusObservation::Present { .. }) {
                    present_paths.push(edit.path.path.as_path());
                }
            }
            SyncRecord::Undecided {
                attention: true, ..
            }
            | SyncRecord::Refusal { .. } => return Err("review needs attention"),
            SyncRecord::Hook { outcome, .. }
                if !matches!(
                    outcome.as_str(),
                    "disabled" | "would_run" | "would_hold" | "unshown" | "unapproved"
                ) =>
            {
                return Err("invalid dry-run hook");
            }
            _ => {}
        }
    }
    if outgoing != 1
        || !matches!(records.first(), Some(SyncRecord::Outgoing { .. }))
        || stages != ["capture", "commit", "hooks"]
    {
        return Err("incomplete review");
    }
    present_paths.sort_unstable();
    if present_paths.windows(2).any(|pair| match pair {
        [first, second] => second.starts_with(first),
        _ => false,
    }) {
        return Err("conflicting outgoing paths");
    }
    Ok(())
}

fn validate_capture<'a>(
    capture: &'a protocol::SyncCapture,
    keys: &mut BTreeSet<&'a str>,
    sources: &mut BTreeSet<&'a str>,
) -> Result<(), &'static str> {
    if capture.scope != "home" || !capture.hypothetical {
        return Err("unsupported capture scope");
    }
    let source = capture
        .source
        .path
        .to_str()
        .ok_or("non-UTF-8 capture source")?;
    if !valid_key(&capture.key.raw, true)
        || !valid_key(source, true)
        || !keys.insert(&capture.key.raw)
        || !sources.insert(source)
    {
        return Err("invalid or duplicate capture");
    }
    for value in [
        &capture.logical_r,
        &capture.raw_h,
        &capture.portable_h,
        &capture.stored_before,
        &capture.stored_after,
    ] {
        observed(value)?;
    }
    if !matches!(capture.raw_h, StatusObservation::Present { .. }) {
        return Err("capture has no observed home content");
    }
    Ok(())
}

fn valid_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_tracking_ref(value: &str) -> bool {
    let Some(branch) = value.strip_prefix("refs/remotes/origin/") else {
        return false;
    };
    !branch.is_empty()
        && !branch.ends_with('/')
        && !branch.ends_with('.')
        && !branch.contains("..")
        && !branch.contains("@{")
        && branch.split('/').all(|part| {
            !part.is_empty() && !part.starts_with('.') && !part.as_bytes().ends_with(b".lock")
        })
        && !branch
            .chars()
            .any(|c| c.is_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}

const fn hidden(c: char) -> bool {
    matches!(c, '\u{ad}' | '\u{61c}' | '\u{180e}' | '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{e0001}' | '\u{e0020}'..='\u{e007f}')
}

fn valid_key(value: &str, root: bool) -> bool {
    if value.is_empty()
        || value.starts_with('/')
        || value.chars().any(|c| c.is_control() || hidden(c))
        || gdk::glib::normalize(value, gdk::glib::NormalizeMode::DefaultCompose).as_str() != value
    {
        return false;
    }
    for part in value.split('/') {
        if matches!(part, "" | "." | "..")
            || part.split('\\').any(|segment| {
                let bare = segment
                    .split(':')
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches([' ', '.']);
                bare.eq_ignore_ascii_case(".git") || bare.eq_ignore_ascii_case("git~1")
            })
        {
            return false;
        }
    }
    let first = value.split('/').next().unwrap_or_default().to_lowercase();
    !root
        || !(matches!(
            first.as_str(),
            ".git" | ".dotman" | ".dotman.toml" | "install.sh"
        ) || first.starts_with("readme")
            || first.starts_with("license"))
}

fn valid_outgoing(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    !bytes.is_empty()
        && !bytes.contains(&0)
        && bytes.split(|byte| *byte == b'/').all(|part| {
            !matches!(part, b"" | b"." | b"..")
                && std::str::from_utf8(part).map_or(true, |part| valid_key(part, false))
        })
}

fn observed(value: &StatusObservation) -> Result<Observed<'_>, &'static str> {
    value.validate().map_err(|_| "invalid observation")?;
    match value {
        StatusObservation::Present { tuple } => Ok(Observed::Present {
            tuple: Tuple::from(tuple),
        }),
        StatusObservation::Missing => Ok(Observed::Missing),
        StatusObservation::Unavailable { .. } | StatusObservation::Blocked { .. } => {
            Err("unsupported observation")
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum InputPath<'a> {
    Raw { raw: &'a str },
    Bytes { bytes: &'a [u8] },
}

fn input_path(path: &Path) -> InputPath<'_> {
    path.to_str().map_or_else(
        || InputPath::Bytes {
            bytes: path.as_os_str().as_bytes(),
        },
        |raw| InputPath::Raw { raw },
    )
}

#[derive(Serialize)]
struct Tuple<'a> {
    kind: &'a str,
    hash: &'a str,
    mode: &'a str,
}
impl<'a> From<&'a StatusTuple> for Tuple<'a> {
    fn from(value: &'a StatusTuple) -> Self {
        Self {
            kind: &value.kind,
            hash: &value.hash,
            mode: &value.mode,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum Observed<'a> {
    Present { tuple: Tuple<'a> },
    Missing,
}

#[derive(Serialize)]
struct Capture<'a> {
    key: &'a str,
    source: &'a str,
    logical_r: Observed<'a>,
    raw_h: Observed<'a>,
    stored_before: Observed<'a>,
}

#[derive(Serialize)]
struct Outgoing<'a> {
    path: InputPath<'a>,
    stored: Observed<'a>,
}

#[derive(Serialize)]
struct SyncInput<'a> {
    head: Option<&'a str>,
    tracking_ref: Option<&'a str>,
    captures: Vec<Capture<'a>>,
    outgoing: Vec<Outgoing<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_lines: Option<&'a BTreeMap<String, Vec<u32>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_target: Option<&'a str>,
}

#[derive(Serialize)]
struct Machine<'a> {
    host: &'a str,
    user: &'a str,
    os: &'a str,
    distro: &'a str,
    like: &'a str,
    arch: &'a str,
    profiles: Vec<&'a str>,
}

impl<'a> From<&'a StatusIdentity> for Machine<'a> {
    fn from(machine: &'a StatusIdentity) -> Self {
        Self {
            host: &machine.host.raw,
            user: &machine.user.raw,
            os: &machine.os.raw,
            distro: &machine.distro.raw,
            like: &machine.like.raw,
            arch: &machine.arch.raw,
            profiles: machine
                .profiles
                .iter()
                .map(|value| value.raw.as_str())
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct IdentityApproval<'a> {
    recorded: Machine<'a>,
    current: Machine<'a>,
}

#[derive(Serialize)]
struct IdentityApprovals<'a> {
    identity: IdentityApproval<'a>,
}

#[derive(Serialize)]
struct IdentityDocument<'a> {
    schema_version: u8,
    repo_id: &'a str,
    home: InputPath<'a>,
    repo: InputPath<'a>,
    machine: Machine<'a>,
    files: [(); 0],
    approvals: IdentityApprovals<'a>,
}

#[derive(Serialize)]
struct Approvals {}

#[derive(Serialize)]
struct SyncApprovals<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    mass_delete: Option<Vec<InputPath<'a>>>,
}

#[derive(Serialize)]
struct Document<'a> {
    schema_version: u8,
    repo_id: &'a str,
    home: InputPath<'a>,
    repo: InputPath<'a>,
    machine: Machine<'a>,
    files: [(); 0],
    sync: SyncInput<'a>,
    approvals: SyncApprovals<'a>,
}

struct Bounded(Vec<u8>);
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_DOCUMENT.saturating_sub(self.0.len()) {
            return Err(io::Error::other("decisions document too large"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{StatusReply, SyncReply, parse_status, parse_sync};
    use std::os::unix::ffi::OsStringExt;

    const CONTEXT: &str = r#"{"record":"context","schema_version":1,"experimental":true,"command":"status","home":{"bytes":[47,104,255],"shown":"shown home"},"repo":{"raw":"/repo","shown":"shown repo"},"config":{"raw":"/config","shown":"config"},"repo_config":{"raw":"/repo/config","shown":"config"},"state":{"raw":"/state","shown":"state"},"identity":{"raw":"/identity","shown":"identity"},"age_key_file":null,"repo_id":{"raw":"raw-id","shown":"shown-id"},"machine":{"host":{"raw":"raw-host","shown":"shown-host"},"user":{"raw":"u","shown":"u"},"os":{"raw":"linux","shown":"linux"},"distro":{"raw":"","shown":""},"like":{"raw":"","shown":""},"arch":{"raw":"x","shown":"x"},"profiles":[{"raw":"profile","shown":"shown-profile"}]}}"#;
    const REVIEW: &str = r#"{"record":"review_context","hooks":{"standing":"empty"},"remote":{"basis":"unknown","ahead":null,"behind":null}}"#;
    const SUMMARY: &str = r#"{"record":"summary","rows":0,"clean":0,"new":0,"held":0,"held_in_home":0,"held_in_repo":0,"unrecorded":0}"#;
    const RESULT: &str = r#"{"record":"result","outcome":"done","exit_code":0,"error_kind":null}"#;
    const HEADER: &str = r#"{"record":"outgoing","head":null,"tracking_ref":null,"tracking_commit":null,"selected_target":"local","knowledge":"last_fetched","warnings":[]}"#;
    const CAPTURE: &str = r#"{"record":"capture","key":{"raw":"é","shown":"shown-key"},"source":{"raw":"é","shown":"shown-source"},"scope":"home","logical_r":{"state":"missing"},"raw_h":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}},"portable_h":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}},"stored_before":{"state":"missing"},"stored_after":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}},"kept_mode":null,"repo_mode":null,"hypothetical":true,"encrypted":false}"#;
    const EDIT: &str = r#"{"record":"repo_edit","path":{"bytes":[120,255],"shown":"shown-path"},"stored":{"state":"missing"},"observed":null,"remove_tracked_temporary":false,"basis":"index"}"#;
    const FILE: &str = r#"{"status":"both-changed","key":{"raw":"--option","shown":"alias"},"scope":"home","path":null,"source":{"raw":"--option","shown":"alias"},"recorded_source":null,"encrypted":false,"template":false,"system":false,"row":"both-changed","row_number":11,"category":"conflicts","r":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}},"source_tuple":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0644"}},"h":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}},"raw_h":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}},"s":{"state":"missing"},"identity_changed":false,"update_state":false}"#;

    fn identity_report() -> Result<StatusReport, Box<dyn std::error::Error>> {
        let context = CONTEXT.replace("\"raw-host\"", "\"beta\"");
        let context_value: serde_json::Value = serde_json::from_str(&context)?;
        let current = context_value.get("machine").ok_or("machine")?.clone();
        let mut recorded = current.clone();
        *recorded.pointer_mut("/host/raw").ok_or("host")? = "alpha".into();
        let marker = serde_json::json!({"record":"review_context","hooks":{"standing":"empty"},"remote":{"basis":"unknown","ahead":null,"behind":null},"current_identity":current,"recorded_identity":recorded});
        let entry = FILE
            .replace(
                "\"status\":\"both-changed\"",
                "\"record\":\"entry\",\"status\":\"identity-changed\"",
            )
            .replace("\"row\":\"both-changed\"", "\"row\":\"changed-in-repo\"")
            .replace("\"row_number\":11", "\"row_number\":10")
            .replace("\"identity_changed\":false", "\"identity_changed\":true")
            .replace(
                "\"recorded_source\":null",
                "\"recorded_source\":{\"raw\":\"profile{host=alpha}\",\"shown\":\"old\"}",
            );
        let input = format!("{context}\n{entry}\n{marker}\n{SUMMARY}\n{RESULT}\n");
        let StatusReply::Report(report) = parse_status(input.as_bytes(), Some(0))? else {
            return Err("expected report".into());
        };
        Ok(report)
    }

    #[test]
    fn reviewed_identity_exact_envelope_and_post_status() -> Result<(), Box<dyn std::error::Error>>
    {
        let report = identity_report()?;
        let review = ReviewedIdentity::new(&report)?;
        let machine = serde_json::json!({"host":"beta","user":"u","os":"linux","distro":"","like":"","arch":"x","profiles":["profile"]});
        let mut old = machine.clone();
        *old.get_mut("host").ok_or("host")? = "alpha".into();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&review.encode()?)?,
            serde_json::json!({"schema_version":1,"repo_id":"raw-id","home":{"bytes":[47,104,255]},"repo":{"raw":"/repo"},"machine":machine,"files":[],"approvals":{"identity":{"recorded":old,"current":machine}}})
        );
        assert_eq!(review.current().host.raw, "beta");
        assert_eq!(review.recorded().host.raw, "alpha");
        let mut after = identity_report()?;
        assert!(!review.is_recorded_in(&after));
        after.recorded_identity = after.current_identity.clone();
        assert!(review.is_recorded_in(&after));
        after.recorded_identity = None;
        assert!(!review.is_recorded_in(&after));
        after.recorded_identity = after.current_identity.clone();
        after.context.repo_id.as_mut().ok_or("id")?.raw = "other".into();
        assert!(!review.is_recorded_in(&after));
        Ok(())
    }

    #[test]
    fn reviewed_identity_rejects_ineligible_reports() -> Result<(), Box<dyn std::error::Error>> {
        let mut report = identity_report()?;
        report.entries.clear();
        assert!(ReviewedIdentity::new(&report).is_err());
        let mut report = identity_report()?;
        report.recorded_identity = None;
        assert!(ReviewedIdentity::new(&report).is_err());
        report = identity_report()?;
        report.current_identity = None;
        assert!(ReviewedIdentity::new(&report).is_err());
        report = identity_report()?;
        report.recorded_identity = report.current_identity.clone();
        assert!(ReviewedIdentity::new(&report).is_err());
        report = identity_report()?;
        report.current_identity.as_mut().ok_or("current")?.host.raw = "stale".into();
        assert!(ReviewedIdentity::new(&report).is_err());
        report = identity_report()?;
        report.context.repo_id = None;
        assert!(ReviewedIdentity::new(&report).is_err());
        report = identity_report()?;
        report
            .recorded_identity
            .as_mut()
            .ok_or("recorded")?
            .host
            .raw = "\0".into();
        assert!(ReviewedIdentity::new(&report).is_err());
        report = identity_report()?;
        let entry = report.entries.first_mut().ok_or("entry")?;
        entry.row = None;
        assert!(ReviewedIdentity::new(&report).is_err());
        report = identity_report()?;
        let entry = report.entries.first_mut().ok_or("entry")?;
        entry.scope = "system".into();
        entry.system = true;
        assert!(ReviewedIdentity::new(&report).is_err());
        report = identity_report()?;
        let entry = report.entries.first_mut().ok_or("entry")?;
        entry.status = "held".into();
        assert!(ReviewedIdentity::new(&report).is_err());
        Ok(())
    }

    #[test]
    fn status_rejects_mismatched_review_identity_but_accepts_legacy()
    -> Result<(), Box<dyn std::error::Error>> {
        let legacy = format!("{CONTEXT}\n{REVIEW}\n{SUMMARY}\n{RESULT}\n");
        let StatusReply::Report(report) = parse_status(legacy.as_bytes(), Some(0))? else {
            return Err("expected legacy report".into());
        };
        assert!(ReviewedIdentity::new(&report).is_err());
        let context: serde_json::Value = serde_json::from_str(CONTEXT)?;
        let mut current = context.get("machine").ok_or("machine")?.clone();
        *current.pointer_mut("/host/raw").ok_or("host")? = "stale".into();
        let marker = serde_json::json!({"record":"review_context","hooks":{"standing":"empty"},"remote":{"basis":"unknown","ahead":null,"behind":null},"current_identity":current});
        let input = format!("{CONTEXT}\n{marker}\n{SUMMARY}\n{RESULT}\n");
        assert!(parse_status(input.as_bytes(), Some(0)).is_err());
        Ok(())
    }

    #[test]
    fn reviewed_apply_encodes_literal_mixed_batch() -> Result<(), Box<dyn std::error::Error>> {
        let (mut context, _) = fixtures(CAPTURE, EDIT)?;
        context.repo.path = std::ffi::OsString::from_vec(vec![47, 114, 255]).into();
        let first: StatusEntry = serde_json::from_str(FILE)?;
        let mut second = first.clone();
        second.key.raw = "--option-extra".into();
        second.source.as_mut().ok_or("source")?.raw = "variant".into();
        second.row = Some("deleted-locally".into());
        second.status = "deleted-locally".into();
        second.h = StatusObservation::Missing;
        second.raw_h = StatusObservation::Missing;
        let mut third = first.clone();
        third.key.raw = "other".into();
        third.status = "existing".into();
        third.row = Some("existing".into());
        let review = ReviewedApply::new(
            &context,
            vec![
                (&first, ApplyAction::KeepLocal),
                (&second, ApplyAction::Restore),
                (&third, ApplyAction::TakeRepo),
            ],
        )?;
        assert_eq!(
            review
                .operands()
                .first()
                .ok_or("operand")?
                .as_os_str()
                .as_bytes(),
            b"/h\xff/--option"
        );
        let json: serde_json::Value = serde_json::from_slice(&review.encode()?)?;
        assert_eq!(
            json.pointer("/repo/bytes"),
            Some(&serde_json::json!([47, 114, 255]))
        );
        assert_eq!(
            json.pointer("/home/bytes"),
            Some(&serde_json::json!([47, 104, 255]))
        );
        assert_eq!(
            json.pointer("/files/0/key"),
            Some(&serde_json::json!("--option"))
        );
        assert_eq!(
            json.pointer("/files/0/action"),
            Some(&serde_json::json!("keep_local"))
        );
        assert_eq!(
            json.pointer("/files/1/source"),
            Some(&serde_json::json!("variant"))
        );
        assert_eq!(
            json.pointer("/files/1/h/state"),
            Some(&serde_json::json!("missing"))
        );
        assert_eq!(
            json.pointer("/files/1/raw_h/state"),
            Some(&serde_json::json!("missing"))
        );
        assert_eq!(
            json.pointer("/files/1/action"),
            Some(&serde_json::json!("restore"))
        );
        assert_eq!(
            json.pointer("/files/2/action"),
            Some(&serde_json::json!("take_repo"))
        );
        assert_eq!(json.pointer("/approvals"), Some(&serde_json::json!({})));
        assert!(json.get("sync").is_none());
        assert!(!json.to_string().contains("alias"));
        Ok(())
    }

    #[test]
    fn reviewed_apply_rejects_unsupported_inputs() -> Result<(), Box<dyn std::error::Error>> {
        let (context, _) = fixtures(CAPTURE, EDIT)?;
        let entry: StatusEntry = serde_json::from_str(FILE)?;
        assert!(ReviewedApply::new(&context, vec![]).is_err());
        assert!(ReviewedApply::new(&context, vec![(&entry, ApplyAction::Restore)]).is_err());
        assert!(
            ReviewedApply::new(
                &context,
                vec![
                    (&entry, ApplyAction::TakeRepo),
                    (&entry, ApplyAction::KeepLocal)
                ]
            )
            .is_err()
        );
        assert!(ReviewedApply::new(&context, vec![(&entry, ApplyAction::TakeRepo); 257]).is_err());
        for change in 0..12 {
            let mut bad = entry.clone();
            match change {
                0 => bad.encrypted = true,
                1 => bad.template = true,
                2 => bad.system = true,
                3 => bad.identity_changed = true,
                4 => bad.status = "held".into(),
                5 => bad.key.raw = "../escape".into(),
                6 => bad.source = None,
                7 => bad.r = StatusObservation::Missing,
                8 => {
                    bad.raw_h = StatusObservation::Unavailable {
                        reason: "unreadable".into(),
                    }
                }
                9 => bad.h = StatusObservation::Missing,
                10 => {
                    if let StatusObservation::Present { tuple } = &mut bad.source_tuple {
                        tuple.kind = "symlink".into();
                        tuple.mode = "0777".into();
                    }
                }
                _ => {
                    if let StatusObservation::Present { tuple } = &mut bad.r {
                        tuple.hash = "bad".into();
                    }
                }
            }
            assert!(
                ReviewedApply::new(&context, vec![(&bad, ApplyAction::TakeRepo)]).is_err(),
                "case {change}"
            );
        }
        Ok(())
    }

    #[test]
    fn changed_in_repo_requires_take_repo_and_regular_exact_observations()
    -> Result<(), Box<dyn std::error::Error>> {
        let (context, _) = fixtures(CAPTURE, EDIT)?;
        let mut entry: StatusEntry = serde_json::from_str(FILE)?;
        entry.status = "changed-in-repo".into();
        entry.row = Some("changed-in-repo".into());
        assert!(ReviewedApply::new(&context, vec![(&entry, ApplyAction::TakeRepo)]).is_ok());
        for action in [ApplyAction::KeepLocal, ApplyAction::Restore] {
            assert!(ReviewedApply::new(&context, vec![(&entry, action)]).is_err());
        }
        for observation in 0..4 {
            let mut bad = entry.clone();
            let value = match observation {
                0 => &mut bad.r,
                1 => &mut bad.source_tuple,
                2 => &mut bad.h,
                _ => &mut bad.raw_h,
            };
            *value = StatusObservation::Missing;
            assert!(ReviewedApply::new(&context, vec![(&bad, ApplyAction::TakeRepo)]).is_err());
        }
        Ok(())
    }

    fn fixtures(capture: &str, edit: &str) -> Result<(StatusContext, Box<SyncReview>), String> {
        let status = format!("{CONTEXT}\n{REVIEW}\n{SUMMARY}\n{RESULT}\n");
        let StatusReply::Report(status) = parse_status(status.as_bytes(), Some(0))? else {
            return Err("status error".into());
        };
        let context = CONTEXT.replace(r#""command":"status""#, r#""command":"sync""#);
        let output = format!(
            "{context}\n{HEADER}\n{{\"record\":\"stage\",\"stage\":\"capture\",\"dry_run\":true}}\n{capture}\n{{\"record\":\"stage\",\"stage\":\"commit\",\"dry_run\":true}}\n{edit}\n{{\"record\":\"stage\",\"stage\":\"hooks\",\"dry_run\":true}}\n{RESULT}\n"
        );
        let SyncReply::Review(review) = parse_sync(output.as_bytes(), Some(0), &status.context)?
        else {
            return Err("review error".into());
        };
        Ok((status.context, review))
    }

    #[test]
    fn mass_delete_receipt_requires_exact_set_and_explicit_approval()
    -> Result<(), Box<dyn std::error::Error>> {
        let status = format!("{CONTEXT}\n{REVIEW}\n{SUMMARY}\n{RESULT}\n");
        let StatusReply::Report(status) = parse_status(status.as_bytes(), Some(0))? else {
            return Err("status error".into());
        };
        let context = CONTEXT.replace(r#""command":"status""#, r#""command":"sync""#);
        let edit = r#"{"record":"repo_edit","path":{"raw":".gone","shown":"not-authoritative"},"stored":{"state":"missing"},"observed":{"state":"missing"},"remove_tracked_temporary":false,"basis":"worktree"}"#;
        let marker = r#"{"record":"mass_delete_review","paths":[{"raw":".gone","shown":"not-authoritative"}]}"#;
        let result =
            r#"{"record":"result","outcome":"error","exit_code":1,"error_kind":"mass_delete"}"#;
        let output = format!(
            "{context}\n{HEADER}\n{{\"record\":\"stage\",\"stage\":\"capture\",\"dry_run\":true}}\n{{\"record\":\"stage\",\"stage\":\"commit\",\"dry_run\":true}}\n{edit}\n{marker}\n{{\"record\":\"stage\",\"stage\":\"hooks\",\"dry_run\":true}}\n{result}\n"
        );
        let SyncReply::Review(review) = parse_sync(output.as_bytes(), Some(1), &status.context)?
        else {
            return Err("mass review missing".into());
        };
        let review = CompleteReview::new(&review, &status.context)?;
        let denied: serde_json::Value = serde_json::from_slice(&review.encode_mode(None, false)?)?;
        assert!(denied.pointer("/approvals/mass_delete").is_none());
        let SyncReply::Review(review) = parse_sync(output.as_bytes(), Some(1), &status.context)?
        else {
            return Err("mass review missing".into());
        };
        let review = CompleteReview::new(&review, &status.context)?;
        let accepted: serde_json::Value = serde_json::from_slice(&review.encode_mode(None, true)?)?;
        assert_eq!(
            accepted.pointer("/approvals/mass_delete"),
            Some(&serde_json::json!([{"raw":".gone"}]))
        );
        let SyncReply::Review(review) = parse_sync(output.as_bytes(), Some(1), &status.context)?
        else {
            return Err("mass review missing".into());
        };
        assert!(
            CompleteReview::new(&review, &status.context)?
                .encode_mode(Some(&BTreeMap::new()), true)
                .is_err()
        );
        for bad in [
            output.replace(marker, ""),
            output.replace(marker, &format!("{marker}\n{marker}")),
            output.replace(
                r#""raw":".gone","shown":"not-authoritative"}]}"#,
                r#""raw":".other","shown":"not-authoritative"}]}"#,
            ),
            output.replace(r#""stage":"hooks""#, r#""stage":"apply""#),
            output.replace("mass_delete", "other_error"),
        ] {
            match parse_sync(bad.as_bytes(), Some(1), &status.context) {
                Ok(SyncReply::Review(review)) => {
                    assert!(CompleteReview::new(&review, &status.context).is_err());
                }
                Ok(SyncReply::Error { .. }) | Err(_) => {}
            }
        }
        Ok(())
    }

    #[test]
    fn selected_document_excludes_unselected_outgoing() -> Result<(), Box<dyn std::error::Error>> {
        let (status, review) = fixtures(CAPTURE, EDIT)?;
        let selected = BTreeMap::from([("é".to_owned(), vec![1, 3])]);
        let bytes = CompleteReview::new(&review, &status)?.encode_selected(Some(&selected))?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(
            json.pointer("/sync/selected_lines/é"),
            Some(&serde_json::json!([1, 3]))
        );
        assert_eq!(
            json.pointer("/sync/selected_target"),
            Some(&serde_json::json!("local"))
        );
        assert_eq!(json.pointer("/sync/outgoing"), Some(&serde_json::json!([])));
        assert_eq!(
            json.pointer("/sync/captures")
                .and_then(|v| v.as_array())
                .map(Vec::len),
            Some(1)
        );
        for invalid in [
            BTreeMap::new(),
            BTreeMap::from([("é".into(), vec![])]),
            BTreeMap::from([("é".into(), vec![1, 1])]),
            BTreeMap::from([("unknown".into(), vec![1])]),
        ] {
            assert!(
                CompleteReview::new(&review, &status)?
                    .encode_selected(Some(&invalid))
                    .is_err()
            );
        }
        let (status, mut review) = fixtures(CAPTURE, EDIT)?;
        let Some(SyncRecord::Outgoing {
            selected_target, ..
        }) = review.records.first_mut()
        else {
            return Err("missing outgoing".into());
        };
        *selected_target = None;
        assert!(
            CompleteReview::new(&review, &status)?
                .encode_selected(Some(&selected))
                .is_err()
        );
        let Some(SyncRecord::Outgoing {
            selected_target, ..
        }) = review.records.first_mut()
        else {
            return Err("missing outgoing".into());
        };
        *selected_target = Some(format!("git-{}", "a".repeat(40)));
        let value = CompleteReview::new(&review, &status)?.encode_selected(Some(&selected))?;
        let json: serde_json::Value = serde_json::from_slice(&value)?;
        assert_eq!(
            json.pointer("/sync/selected_target")
                .and_then(|v| v.as_str()),
            Some(format!("git-{}", "a".repeat(40)).as_str())
        );
        Ok(())
    }

    #[test]
    fn emits_raw_values_byte_paths_and_nulls_without_approvals()
    -> Result<(), Box<dyn std::error::Error>> {
        let (status, review) = fixtures(CAPTURE, EDIT)?;
        let bytes = CompleteReview::new(&review, &status)?.encode()?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(json.pointer("/repo_id"), Some(&serde_json::json!("raw-id")));
        assert_eq!(
            json.pointer("/home/bytes"),
            Some(&serde_json::json!([47, 104, 255]))
        );
        assert_eq!(json.pointer("/sync/head"), Some(&serde_json::Value::Null));
        assert_eq!(
            json.pointer("/sync/tracking_ref"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(
            json.pointer("/sync/captures/0/source"),
            Some(&serde_json::json!("é"))
        );
        assert_eq!(
            json.pointer("/sync/captures/0/logical_r/state"),
            Some(&serde_json::json!("missing"))
        );
        assert_eq!(
            json.pointer("/sync/outgoing/0/path/bytes"),
            Some(&serde_json::json!([120, 255]))
        );
        assert_eq!(json.pointer("/files"), Some(&serde_json::json!([])));
        assert_eq!(json.pointer("/approvals"), Some(&serde_json::json!({})));
        assert!(!String::from_utf8(bytes)?.contains("shown-"));
        let (status, mut review) = fixtures(CAPTURE, EDIT)?;
        if let Some(SyncRecord::Outgoing {
            head, tracking_ref, ..
        }) = review.records.first_mut()
        {
            *head = Some(protocol::StatusText {
                raw: "a".repeat(40),
                shown: "hidden".into(),
            });
            *tracking_ref = Some(protocol::StatusText {
                raw: "refs/remotes/origin/main".into(),
                shown: "hidden".into(),
            });
        }
        let bytes = CompleteReview::new(&review, &status)?.encode()?;
        let json: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(
            json.pointer("/sync/head"),
            Some(&serde_json::json!("a".repeat(40)))
        );
        assert_eq!(
            json.pointer("/sync/tracking_ref"),
            Some(&serde_json::json!("refs/remotes/origin/main"))
        );
        Ok(())
    }

    #[test]
    fn rejects_stale_partial_duplicate_system_and_unavailable()
    -> Result<(), Box<dyn std::error::Error>> {
        let (status, mut review) = fixtures(CAPTURE, EDIT)?;
        review.exit_code = 3;
        assert!(CompleteReview::new(&review, &status).is_err());
        review.exit_code = 0;
        review.records.push(serde_json::from_str(CAPTURE)?);
        assert!(CompleteReview::new(&review, &status).is_err());
        let context = CONTEXT.replace(r#""command":"status""#, r#""command":"sync""#);
        let partial = format!(
            "{context}\n{HEADER}\n{{\"record\":\"result\",\"outcome\":\"error\",\"exit_code\":3,\"error_kind\":null}}\n"
        );
        let SyncReply::Review(mut failed) = parse_sync(partial.as_bytes(), Some(3), &status)?
        else {
            return Err("missing partial review".into());
        };
        failed.exit_code = 0;
        let (_, mut complete) = fixtures(CAPTURE, EDIT)?;
        failed.records = std::mem::take(&mut complete.records);
        assert!(CompleteReview::new(&failed, &status).is_err());
        let (mut status, review) = fixtures(CAPTURE, EDIT)?;
        status.repo.path = "/different".into();
        assert!(CompleteReview::new(&review, &status).is_err());
        let system = CAPTURE
            .replace(
                r#""raw":"é","shown":"shown-key""#,
                r#""raw":"/etc/test","shown":"shown-key""#,
            )
            .replace(r#""scope":"home""#, r#""scope":"system""#);
        let (status, review) = fixtures(&system, EDIT)?;
        assert!(CompleteReview::new(&review, &status).is_err());
        let encrypted = CAPTURE.replace(r#""stored_after":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}}"#, r#""stored_after":{"state":"unavailable","reason":"hypothetical_ciphertext"}"#);
        let (status, review) = fixtures(&encrypted, EDIT)?;
        assert!(CompleteReview::new(&review, &status).is_err());
        Ok(())
    }

    #[test]
    fn rejects_malformed_keys_conflicts_and_invalid_tuples()
    -> Result<(), Box<dyn std::error::Error>> {
        for value in [
            "../x",
            ".git/config",
            "git~1/config",
            "e\u{301}",
            "x\u{202e}",
        ] {
            assert!(!valid_key(value, true));
        }
        assert!(valid_key("é", true));
        let (status, mut review) = fixtures(CAPTURE, EDIT)?;
        if let Some(SyncRecord::Capture(capture)) = review
            .records
            .iter_mut()
            .find(|record| matches!(record, SyncRecord::Capture(_)))
            && let StatusObservation::Present { tuple } = &mut capture.raw_h
        {
            tuple.hash = "invalid".into();
        }
        assert!(CompleteReview::new(&review, &status).is_err());
        assert!(!valid_outgoing(Path::new("../bad")));
        Ok(())
    }

    #[test]
    fn rejects_repeated_paths_non_utf8_source_and_blocked_stage()
    -> Result<(), Box<dyn std::error::Error>> {
        let (status, mut review) = fixtures(CAPTURE, EDIT)?;
        review.records.push(serde_json::from_str(EDIT)?);
        assert!(CompleteReview::new(&review, &status).is_err());
        let source = CAPTURE.replace(
            r#""source":{"raw":"é","shown":"shown-source"}"#,
            r#""source":{"bytes":[120,255],"shown":"shown-source"}"#,
        );
        let (status, review) = fixtures(&source, EDIT)?;
        assert!(CompleteReview::new(&review, &status).is_err());
        let (status, mut review) = fixtures(CAPTURE, EDIT)?;
        review.records.push(serde_json::from_str(
            r#"{"record":"stage","stage":"blocked_by_repo_hold","dry_run":true}"#,
        )?);
        assert!(CompleteReview::new(&review, &status).is_err());
        let (status, mut review) = fixtures(CAPTURE, EDIT)?;
        if let Some(SyncRecord::Outgoing { tracking_ref, .. }) = review.records.first_mut() {
            *tracking_ref = Some(protocol::StatusText {
                raw: "foreign/ref".into(),
                shown: String::new(),
            });
        }
        assert!(CompleteReview::new(&review, &status).is_err());
        Ok(())
    }

    #[test]
    fn rejects_conflicting_present_paths_and_enforces_size_bound()
    -> Result<(), Box<dyn std::error::Error>> {
        let (status, mut review) = fixtures(CAPTURE, EDIT)?;
        let edit = r#"{"record":"repo_edit","path":{"raw":"parent","shown":"parent"},"stored":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0644"}},"observed":null,"remove_tracked_temporary":false,"basis":"index"}"#;
        review.records.push(serde_json::from_str(edit)?);
        review.records.push(serde_json::from_str(
            &edit.replace(r#""raw":"parent""#, r#""raw":"parent/child""#),
        )?);
        assert!(CompleteReview::new(&review, &status).is_err());
        let system = EDIT.replace(
            r#""bytes":[120,255]"#,
            r#""raw":".dotman/system/etc/example""#,
        );
        let (status, review) = fixtures(CAPTURE, &system)?;
        assert!(CompleteReview::new(&review, &status).is_err());
        let unknown = EDIT
            .replace(
                r#""observed":null"#,
                r#""observed":{"state":"unavailable","reason":"unreadable"}"#,
            )
            .replace(r#""basis":"index""#, r#""basis":"worktree""#);
        let (status, review) = fixtures(CAPTURE, &unknown)?;
        assert!(CompleteReview::new(&review, &status).is_err());
        let mut output = Bounded(Vec::new());
        assert!(
            output
                .write_all(&vec![0; MAX_DOCUMENT.saturating_add(1)])
                .is_err()
        );
        assert_eq!(
            output.0,
            Vec::<u8>::new(),
            "oversized write must leave the destination empty"
        );
        Ok(())
    }
}
