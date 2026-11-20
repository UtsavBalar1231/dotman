use serde::Deserialize;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

pub(crate) const MAX_REPORT: usize = 32 * 1024 * 1024;
pub(crate) const MAX_RECORD: usize = 1024 * 1024;
pub(crate) const MAX_RECORDS: usize = 100_000;

#[derive(Debug, Deserialize)]
pub(crate) struct Header {
    pub(crate) record: String,
}

/// Exact UTF-8 metadata and the CLI's display form.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StatusText {
    /// Original metadata; not presentation.
    pub raw: String,
    /// Escaped presentation; never a path or argument.
    pub shown: String,
}

fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
struct WirePath {
    #[serde(default, deserialize_with = "present")]
    raw: Option<String>,
    #[serde(default, deserialize_with = "present")]
    bytes: Option<Vec<u8>>,
    shown: String,
}

/// A byte-preserving Rust path and its display-only form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusPath {
    /// Original path; never reconstructed from `shown`.
    pub path: PathBuf,
    /// CLI-supplied escaped presentation.
    pub shown: String,
}

impl<'de> Deserialize<'de> for StatusPath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let wire = WirePath::deserialize(deserializer)?;
        let bytes = match (wire.raw, wire.bytes) {
            (Some(raw), None) => raw.into_bytes(),
            (None, Some(bytes)) => bytes,
            _ => return Err(D::Error::custom("path needs exactly one of raw or bytes")),
        };
        if bytes.is_empty() || bytes.contains(&0) {
            return Err(D::Error::custom("invalid path bytes"));
        }
        Ok(Self {
            path: PathBuf::from(OsString::from_vec(bytes)),
            shown: wire.shown,
        })
    }
}

/// Machine identity captured with the status context.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StatusIdentity {
    /// Host name.
    pub host: StatusText,
    /// User name.
    pub user: StatusText,
    /// Operating system.
    pub os: StatusText,
    /// Distribution.
    pub distro: StatusText,
    /// Distribution family.
    pub like: StatusText,
    /// Architecture.
    pub arch: StatusText,
    /// Ordered profiles.
    pub profiles: Vec<StatusText>,
}

/// Resolved context from the CLI, including display paths.
#[derive(Debug, Clone, Deserialize)]
pub struct StatusContext {
    schema_version: u32,
    experimental: bool,
    command: String,
    /// Resolved home.
    pub home: StatusPath,
    /// Resolved repository.
    pub repo: StatusPath,
    /// Machine configuration.
    pub config: StatusPath,
    /// Repository configuration.
    pub repo_config: StatusPath,
    /// State file.
    pub state: StatusPath,
    /// Age identity path, not its contents.
    pub identity: StatusPath,
    /// Optional additional age identity path.
    #[serde(deserialize_with = "nullable")]
    pub age_key_file: Option<StatusPath>,
    /// Repository identity, unavailable on guard or identity failure.
    #[serde(deserialize_with = "nullable")]
    pub repo_id: Option<StatusText>,
    /// Machine identity.
    pub machine: StatusIdentity,
}

impl StatusContext {
    pub(crate) fn validate(&self, command: &str) -> Result<(), String> {
        if self.schema_version != 1 || !self.experimental || self.command != command {
            return Err("unsupported status context".into());
        }
        let paths = [
            &self.home,
            &self.repo,
            &self.config,
            &self.repo_config,
            &self.state,
            &self.identity,
        ];
        if paths.into_iter().any(|path| !path.path.is_absolute())
            || self
                .age_key_file
                .as_ref()
                .is_some_and(|path| !path.path.is_absolute())
        {
            return Err("relative resolved path".into());
        }
        Ok(())
    }
}

/// A status snapshot tuple.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StatusTuple {
    /// File or symlink.
    pub kind: String,
    /// Lowercase xxh3-128 hex digest.
    pub hash: String,
    /// Four-digit octal mode.
    pub mode: String,
}

/// An observed value; unavailable is not missing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StatusObservation {
    /// Observed tuple.
    Present { tuple: StatusTuple },
    /// No value exists.
    Missing,
    /// A filesystem obstacle.
    Blocked { reason: StatusText },
    /// The value was not available.
    Unavailable { reason: String },
}

impl StatusObservation {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if let Self::Present { tuple } = self
            && (!matches!(tuple.kind.as_str(), "file" | "symlink")
                || tuple.hash.len() != 32
                || !tuple
                    .hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || tuple.mode.len() != 4
                || !tuple.mode.bytes().all(|b| (b'0'..=b'7').contains(&b))
                || (tuple.kind == "symlink" && tuple.mode != "0777"))
        {
            return Err("invalid status tuple".into());
        }
        Ok(())
    }
}

/// Classification of a new-file hold.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hold {
    /// File is too large.
    Large,
    /// File is binary.
    Binary,
    /// The name is sensitive.
    SecretName,
    /// A binary file is sensitive.
    SecretBinary,
    /// A content finding.
    Content,
    /// A special file.
    Special,
    /// An unreadable file.
    Unreadable,
    /// A collision.
    Collision,
    /// An unapproved directory.
    Unapproved,
    /// An unknown classification.
    #[serde(other)]
    Unknown,
}

/// Location of a guard finding.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingPlace {
    /// File content.
    Content,
    /// File name.
    Name,
    /// Link target.
    Target,
    /// Unknown location.
    #[serde(other)]
    Unknown,
}

/// Guard metadata without matching bytes.
#[derive(Debug, Clone, Deserialize)]
pub struct Finding {
    /// Rule identifier.
    pub rule: String,
    /// One-based line.
    pub line: usize,
    /// Finding location.
    pub place: FindingPlace,
}

/// An invalid source and its diagnostic.
#[derive(Debug, Clone, Deserialize)]
pub struct InvalidSource {
    /// Repository source path.
    pub source: StatusText,
    /// Diagnostic.
    pub reason: StatusText,
}

/// One occurrence in the CLI's status output.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Deserialize)]
pub struct StatusEntry {
    /// Status token.
    pub status: String,
    /// Key and display form; repo-source keys can be lossy display metadata.
    pub key: StatusText,
    /// `home`, `system`, or `repo_source`.
    pub scope: String,
    /// Repo-source byte path when supplied.
    #[serde(deserialize_with = "nullable")]
    pub path: Option<StatusPath>,
    /// Selected source.
    #[serde(deserialize_with = "nullable")]
    pub source: Option<StatusText>,
    /// Previously recorded source.
    #[serde(deserialize_with = "nullable")]
    pub recorded_source: Option<StatusText>,
    /// Sources considered by the CLI.
    #[serde(default)]
    pub alternatives: Vec<StatusText>,
    /// Invalid sources and their diagnostics.
    #[serde(default)]
    pub invalid_sources: Vec<InvalidSource>,
    /// Source encryption flag.
    pub encrypted: bool,
    /// Source template flag.
    pub template: bool,
    /// System scope flag.
    pub system: bool,
    /// Underlying row token.
    #[serde(deserialize_with = "nullable")]
    pub row: Option<String>,
    /// Underlying row number.
    #[serde(deserialize_with = "nullable")]
    pub row_number: Option<u8>,
    /// Prompt category.
    #[serde(deserialize_with = "nullable")]
    pub category: Option<String>,
    /// Effective repo observation.
    pub r: StatusObservation,
    /// Repo tuple before kept-mode adjustment.
    pub source_tuple: StatusObservation,
    /// Physical plain source observation, when available.
    #[serde(default)]
    pub physical_plain: Option<StatusObservation>,
    /// Portable home observation.
    pub h: StatusObservation,
    /// Raw home observation.
    pub raw_h: StatusObservation,
    /// Recorded state observation.
    pub s: StatusObservation,
    /// Machine identity changed.
    pub identity_changed: bool,
    /// State needs recording.
    pub update_state: bool,
    /// Optional new-file hold classification.
    #[serde(default)]
    pub hold: Option<Hold>,
    /// Optional guard finding.
    #[serde(default)]
    pub finding: Option<Finding>,
}

pub(crate) fn literal_relative(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    !bytes.is_empty()
        && !bytes.contains(&0)
        && bytes
            .split(|b| *b == b'/')
            .all(|part| !part.is_empty() && part != b"." && part != b"..")
}

impl StatusEntry {
    fn validate(&self) -> Result<(), String> {
        if self.status.is_empty() || self.system != (self.scope == "system") {
            return Err("invalid status metadata".into());
        }
        let key = Path::new(&self.key.raw);
        match self.scope.as_str() {
            "home" | "repo_source" if literal_relative(key) => {}
            "system"
                if key.is_absolute()
                    && literal_relative(
                        key.strip_prefix("/").map_err(|_| "invalid system key")?,
                    ) => {}
            _ => return Err("invalid status key scope or path".into()),
        }
        match (&self.path, self.scope.as_str()) {
            (Some(path), "repo_source") if literal_relative(&path.path) => {}
            (None, "home" | "system") => {}
            _ => return Err("invalid repo source path".into()),
        }
        for observation in [&self.r, &self.source_tuple, &self.h, &self.raw_h, &self.s] {
            observation.validate()?;
        }
        if let Some(observation) = &self.physical_plain {
            observation.validate()?;
        }
        Ok(())
    }
}

/// Counts in the status summary.
#[derive(Debug, Deserialize)]
pub struct StatusSummary {
    /// Classified rows.
    pub rows: usize,
    /// Clean rows.
    pub clean: usize,
    /// New keys.
    pub new: usize,
    /// Held keys.
    pub held: usize,
    /// Supplemental held-home occurrences.
    pub held_in_home: usize,
    /// Held repository occurrences.
    pub held_in_repo: usize,
    /// Clean rows requiring state recording.
    pub unrecorded: usize,
}

#[derive(Debug, Deserialize)]
struct ReviewMarker {
    hooks: ReviewHooks,
    remote: ReviewRemote,
    #[serde(default)]
    current_identity: Option<StatusIdentity>,
    #[serde(default)]
    recorded_identity: Option<StatusIdentity>,
}

#[derive(Debug, Deserialize)]
struct ReviewHooks {
    standing: String,
    #[serde(flatten)]
    projection: serde_json::Map<String, serde_json::Value>,
}

/// Hook table and pending execution data from one status report.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookProjection {
    /// Canonical ordered-table JSON.
    pub canonical: StatusText,
    /// Ordered pattern and command entries.
    pub entries: Vec<HookEntry>,
    /// Managed keys awaiting hooks.
    pub pending_keys: Vec<StatusText>,
    /// Ordered, deduplicated commands due for the pending keys.
    pub pending_commands: Vec<StatusText>,
}

/// One ordered hook table entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookEntry {
    /// Hook pattern.
    pub pattern: StatusText,
    /// Hook command.
    pub command: StatusText,
}

#[derive(Debug, Deserialize)]
pub struct ReviewRemote {
    /// Basis of the remote position.
    pub basis: String,
    #[serde(deserialize_with = "nullable")]
    pub ahead: Option<usize>,
    #[serde(deserialize_with = "nullable")]
    pub behind: Option<usize>,
}

impl ReviewMarker {
    fn validate(&self) -> Result<(), String> {
        if !matches!(
            self.hooks.standing.as_str(),
            "empty" | "approved" | "unapproved" | "changed" | "old_format" | "invalid"
        ) || !matches!(
            self.remote.basis.as_str(),
            "last_fetched" | "not_fetched" | "unknown"
        ) {
            return Err("invalid review context".into());
        }
        let _ = (self.remote.ahead, self.remote.behind);
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResultRecord {
    pub(crate) outcome: String,
    pub(crate) exit_code: i32,
    #[serde(deserialize_with = "nullable")]
    pub(crate) error_kind: Option<String>,
    pub(crate) recovery: Option<Vec<RecoveryStep>>,
}

/// Unquoted terminal argument; it is data, not an execution request.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum RecoveryArgument {
    /// UTF-8 argument.
    Text(String),
    /// Exact Unix bytes.
    Bytes {
        /// Original argument bytes.
        bytes: Vec<u8>,
    },
}

/// One terminal recovery instruction or command.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryStep {
    /// A command and its display-only rendering.
    Command {
        /// Unquoted argv, including the program.
        argv: Vec<RecoveryArgument>,
        /// Escaped presentation, never used as argv.
        shown: Vec<String>,
        /// Condition for the command, not an argument.
        #[serde(deserialize_with = "nullable")]
        when: Option<String>,
    },
    /// Manual instruction.
    Instruction {
        /// Raw and display text.
        text: RecoveryText,
    },
}

/// Raw text and its CLI-supplied display form.
#[derive(Debug, Deserialize)]
pub struct RecoveryText {
    /// Original text.
    pub raw: String,
    /// Escaped presentation.
    pub shown: String,
}

pub(crate) fn validate_recovery(steps: &[RecoveryStep]) -> Result<(), String> {
    for step in steps {
        if let RecoveryStep::Command { argv, shown, .. } = step {
            if argv.is_empty() || argv.len() != shown.len() {
                return Err("invalid recovery argv".into());
            }
            for arg in argv {
                let bytes = match arg {
                    RecoveryArgument::Text(text) => text.as_bytes(),
                    RecoveryArgument::Bytes { bytes } => bytes,
                };
                if bytes.contains(&0) {
                    return Err("invalid recovery argument".into());
                }
            }
        }
    }
    Ok(())
}

/// Parses a bounded recovery array without executing its commands.
///
/// # Errors
/// Rejects malformed or oversized recovery data and invalid argv.
pub fn parse_recovery(input: &[u8]) -> Result<Vec<RecoveryStep>, String> {
    if input.len() > MAX_RECORD {
        return Err("oversized recovery data".into());
    }
    let steps: Vec<RecoveryStep> =
        serde_json::from_slice(input).map_err(|_| "invalid recovery data")?;
    validate_recovery(&steps)?;
    Ok(steps)
}

/// Typed records retained in the core, not an IPC payload.
#[derive(Debug)]
pub struct StatusReport {
    /// Versioned resolved context.
    pub context: StatusContext,
    /// Entries in wire order, including duplicate keys.
    pub entries: Vec<StatusEntry>,
    /// Counts supplied by the CLI.
    pub summary: StatusSummary,
    /// Last observed remote position.
    pub remote: ReviewRemote,
    /// Hook standing and projection, if the complete projection is supported.
    pub hooks: Option<Box<(String, HookProjection)>>,
    /// Current identity from the review context, when supplied.
    pub current_identity: Option<Box<StatusIdentity>>,
    /// Previously recorded identity, when supplied.
    pub recorded_identity: Option<Box<StatusIdentity>>,
}

/// A non-success result is not a status report or authorization to proceed.
#[derive(Debug)]
pub enum StatusReply {
    /// A complete successful report.
    Report(StatusReport),
    /// An unsuccessful process result.
    Error {
        /// Authoritative process exit.
        exit_code: i32,
        /// Context if resolution reached it.
        context: Option<StatusContext>,
        /// CLI error classification.
        error_kind: Option<String>,
        /// Terminal-only instructions, if emitted.
        recovery: Option<Vec<RecoveryStep>>,
    },
}

/// Parses the complete bounded stdout after the child has been reaped.
///
/// # Errors
/// Rejects malformed, incomplete, oversized or contradictory output.
pub fn parse_status(stdout: &[u8], exit_code: Option<i32>) -> Result<StatusReply, String> {
    if stdout.len() > MAX_REPORT || !stdout.ends_with(b"\n") {
        return Err("invalid or oversized status output".into());
    }
    let exit_code = exit_code.ok_or("status process terminated abnormally")?;
    let mut context = None;
    let mut entries = Vec::new();
    let mut review = None;
    let mut summary = None;
    let mut result = None;
    let body = stdout.strip_suffix(b"\n").ok_or("missing final newline")?;
    for (index, line) in body.split(|byte| *byte == b'\n').enumerate() {
        if index >= MAX_RECORDS || line.len() > MAX_RECORD || line.is_empty() || result.is_some() {
            return Err("invalid status framing".into());
        }
        let header: Header = serde_json::from_slice(line).map_err(|_| "invalid record type")?;
        match header.record.as_str() {
            "context" if index == 0 => {
                let parsed: StatusContext =
                    serde_json::from_slice(line).map_err(|_| "invalid context")?;
                parsed.validate("status")?;
                context = Some(parsed);
            }
            "entry" if context.is_some() && review.is_none() && summary.is_none() => {
                let entry: StatusEntry =
                    serde_json::from_slice(line).map_err(|_| "invalid entry")?;
                entry.validate()?;
                entries.push(entry);
            }
            "review_context" if context.is_some() && review.is_none() && summary.is_none() => {
                let marker: ReviewMarker =
                    serde_json::from_slice(line).map_err(|_| "invalid review context")?;
                marker.validate()?;
                review = Some(marker);
            }
            "summary" if review.is_some() && summary.is_none() => {
                summary = Some(serde_json::from_slice(line).map_err(|_| "invalid summary")?);
            }
            "result" => {
                let parsed: ResultRecord =
                    serde_json::from_slice(line).map_err(|_| "invalid result")?;
                if parsed.exit_code != exit_code
                    || parsed.outcome != if exit_code == 0 { "done" } else { "error" }
                    || (exit_code == 0 && parsed.error_kind.is_some())
                    || !matches!(exit_code, 0 | 1 | 3 | 4)
                {
                    return Err("result does not match process exit".into());
                }
                if let Some(steps) = &parsed.recovery {
                    if exit_code == 0 {
                        return Err("recovery on successful result".into());
                    }
                    validate_recovery(steps)?;
                }
                result = Some(parsed);
            }
            _ => return Err("unexpected status record".into()),
        }
    }
    let result = result.ok_or("missing final result")?;
    if exit_code != 0 {
        return Ok(StatusReply::Error {
            exit_code,
            context,
            error_kind: result.error_kind,
            recovery: result.recovery,
        });
    }
    let context = context.ok_or("missing context")?;
    if context.repo_id.is_none() {
        return Err("successful status has no repository identity".into());
    }
    let review = review.ok_or("missing review context")?;
    if review
        .current_identity
        .as_ref()
        .is_some_and(|identity| !same_identity_raw(identity, &context.machine))
    {
        return Err("review identity does not match context".into());
    }
    Ok(StatusReply::Report(StatusReport {
        context,
        entries,
        summary: summary.ok_or("missing summary")?,
        remote: review.remote,
        hooks: serde_json::from_value(serde_json::Value::Object(review.hooks.projection))
            .ok()
            .map(|projection| Box::new((review.hooks.standing, projection))),
        current_identity: review.current_identity.map(Box::new),
        recorded_identity: review.recorded_identity.map(Box::new),
    }))
}

pub(crate) fn same_identity_raw(a: &StatusIdentity, b: &StatusIdentity) -> bool {
    a.host.raw == b.host.raw
        && a.user.raw == b.user.raw
        && a.os.raw == b.os.raw
        && a.distro.raw == b.distro.raw
        && a.like.raw == b.like.raw
        && a.arch.raw == b.arch.raw
        && a.profiles
            .iter()
            .map(|v| &v.raw)
            .eq(b.profiles.iter().map(|v| &v.raw))
}

/// A selected report occurrence, retained only by the core.
#[derive(Clone)]
pub struct DiffSelection {
    /// Resolved status context.
    pub context: StatusContext,
    /// Selected occurrence.
    pub entry: StatusEntry,
}

#[derive(Deserialize)]
struct DiffRecord {
    key: StatusText,
    scope: String,
    path: StatusPath,
    status: String,
    system_off: bool,
    #[serde(deserialize_with = "nullable")]
    source: Option<StatusText>,
    r: StatusObservation,
    source_tuple: StatusObservation,
    #[serde(default)]
    physical_plain: Option<StatusObservation>,
    h: StatusObservation,
    raw_h: StatusObservation,
    #[serde(deserialize_with = "nullable")]
    payload: Option<WirePath>,
    line_edits: Option<Vec<LineEdit>>,
}

/// An exact CLI line selector retained on the native side.
#[derive(Clone, Deserialize, PartialEq, Eq)]
pub struct LineEdit {
    /// One-based selector.
    pub id: u32,
    /// Original line number.
    #[serde(deserialize_with = "nullable")]
    pub old_line: Option<u32>,
    /// New line number.
    #[serde(deserialize_with = "nullable")]
    pub new_line: Option<u32>,
    /// Original fragment including its terminator.
    #[serde(deserialize_with = "nullable")]
    pub before: Option<String>,
    /// New fragment including its terminator.
    #[serde(deserialize_with = "nullable")]
    pub after: Option<String>,
}

#[derive(Deserialize)]
struct DiffCount {
    count: usize,
}

/// Display-only diff result.
pub struct DiffView {
    /// Fixed CLI status.
    pub status: String,
    /// Whether system scope is disabled.
    pub system_off: bool,
    /// Escaped comparison text.
    pub text: Option<String>,
    /// Validated CLI selectors; never sent to the webview.
    pub line_edits: Option<Vec<LineEdit>>,
}

/// Parsed diff or terminal-only failure.
pub enum DiffReply {
    /// A bound comparison.
    Report(DiffView),
    /// A CLI error, including terminal-only recovery.
    Error {
        /// Process exit.
        exit_code: i32,
        /// Context emitted before the failure, if any.
        context: Option<Box<StatusContext>>,
        /// CLI error classification, if any.
        error_kind: Option<String>,
        /// Recovery data never sent to the webview.
        recovery: Option<Vec<RecoveryStep>>,
    },
}

pub(crate) fn same_context(a: &StatusContext, b: &StatusContext) -> bool {
    same_context_paths_and_machine(a, b)
        && a.repo_id.as_ref().map(|id| &id.raw) == b.repo_id.as_ref().map(|id| &id.raw)
}

fn same_context_paths_and_machine(a: &StatusContext, b: &StatusContext) -> bool {
    a.home.path == b.home.path
        && a.repo.path == b.repo.path
        && a.config.path == b.config.path
        && a.repo_config.path == b.repo_config.path
        && a.state.path == b.state.path
        && a.identity.path == b.identity.path
        && a.age_key_file.as_ref().map(|p| &p.path) == b.age_key_file.as_ref().map(|p| &p.path)
        && a.machine == b.machine
}

/// Binds a terminal error to the reviewed context, including early guard failures.
#[must_use]
pub fn terminal_context_matches(
    expected: &StatusContext,
    actual: &StatusContext,
    error_kind: &str,
) -> bool {
    same_context(expected, actual)
        || (error_kind == "in_progress"
            && actual.repo_id.is_none()
            && expected.repo_id.is_some()
            && same_context_paths_and_machine(expected, actual))
}

/// Outgoing dry-run records retained on the trusted side.
#[derive(Debug, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum SyncRecord {
    /// Last-fetched repository position.
    Outgoing {
        /// Local HEAD.
        #[serde(deserialize_with = "nullable")]
        head: Option<StatusText>,
        /// Expected tracking ref.
        #[serde(deserialize_with = "nullable")]
        tracking_ref: Option<StatusText>,
        /// Last-fetched tracking commit.
        #[serde(deserialize_with = "nullable")]
        tracking_commit: Option<StatusText>,
        /// Opaque selected-operation target binding, if supported by the CLI.
        selected_target: Option<String>,
        /// Remote knowledge basis.
        knowledge: String,
        /// Symbolic warnings.
        warnings: Vec<String>,
    },
    /// An already-ahead commit.
    AheadCommit {
        /// Commit identifier.
        id: StatusText,
        /// Commit subject.
        subject: StatusText,
        /// Paths in this commit.
        paths: Vec<StatusPath>,
    },
    /// A reached stage.
    Stage {
        /// Stage name.
        stage: String,
        /// Whether this is a dry run.
        dry_run: bool,
    },
    /// An unresolved occurrence.
    Undecided {
        /// Reason.
        reason: String,
        /// Key when applicable.
        #[serde(deserialize_with = "nullable")]
        key: Option<StatusText>,
        /// Scope.
        scope: String,
        /// Pass.
        pass: String,
        /// Needs attention.
        attention: bool,
        /// Excluded from a later pass.
        skip_next_pass: bool,
    },
    /// A hypothetical resolution.
    Resolved {
        /// Key.
        key: StatusText,
        /// Scope.
        scope: String,
        /// Effect.
        effect: String,
        /// Whether the resolution is hypothetical.
        hypothetical: bool,
    },
    /// A capture snapshot.
    Capture(Box<SyncCapture>),
    /// A projected repository edit.
    RepoEdit(Box<SyncEdit>),
    /// A projected dotman-owned edit.
    Own(Box<SyncEdit>),
    /// Complete projected deletion set for a mass-delete refusal.
    MassDeleteReview {
        /// Literal repository-relative paths.
        paths: Vec<StatusPath>,
    },
    /// Suggested commit message.
    Message {
        /// Message text.
        message: StatusText,
    },
    /// Current hook plan.
    Hooks {
        /// Canonical hook table.
        canonical: StatusText,
        /// Due commands.
        commands: Vec<StatusText>,
        /// Pending commands.
        pending_commands: Vec<StatusText>,
        /// Pending keys.
        pending_keys: Vec<StatusText>,
        /// Standing approval.
        approved: bool,
        /// Table not shown.
        unshown: bool,
        /// Whether this is a dry run.
        dry_run: bool,
    },
    /// Individual hook result.
    Hook {
        /// Command when available.
        #[serde(deserialize_with = "nullable")]
        command: Option<StatusText>,
        /// Outcome.
        outcome: String,
        /// Child exit when available.
        #[serde(deserialize_with = "nullable")]
        exit_code: Option<i32>,
    },
    /// Push or render refusal.
    Refusal {
        /// Refusal kind.
        kind: String,
        /// Source path when available.
        #[serde(deserialize_with = "nullable")]
        source: Option<StatusPath>,
        /// Key when available.
        #[serde(deserialize_with = "nullable")]
        key: Option<StatusText>,
    },
}

/// A capture snapshot.
#[derive(Debug, Deserialize)]
pub struct SyncCapture {
    /// Key.
    pub key: StatusText,
    /// Literal source path.
    pub source: StatusPath,
    /// Scope.
    pub scope: String,
    /// Logical repo observation.
    pub logical_r: StatusObservation,
    /// Raw home observation.
    pub raw_h: StatusObservation,
    /// Portable home observation.
    pub portable_h: StatusObservation,
    /// Stored source before capture.
    pub stored_before: StatusObservation,
    /// Stored source after capture.
    pub stored_after: StatusObservation,
    /// Kept mode, if any.
    #[serde(deserialize_with = "nullable")]
    pub kept_mode: Option<String>,
    /// Repo mode, if any.
    #[serde(deserialize_with = "nullable")]
    pub repo_mode: Option<String>,
    /// Whether this is hypothetical.
    pub hypothetical: bool,
    /// Whether the source is encrypted.
    pub encrypted: bool,
}

/// Match a hypothetical plain-file capture to the exact status snapshot for a diff.
#[must_use]
pub fn capture_matches_status(capture: &SyncCapture, entry: &StatusEntry) -> bool {
    let concrete_file_or_missing = |observation: &StatusObservation| match observation {
        StatusObservation::Present { tuple } => tuple.kind == "file",
        StatusObservation::Missing => true,
        StatusObservation::Blocked { .. } | StatusObservation::Unavailable { .. } => false,
    };
    let present_file = |observation: &StatusObservation| matches!(observation, StatusObservation::Present { tuple } if tuple.kind == "file");
    if capture.scope != "home"
        || entry.scope != "home"
        || entry.system
        || entry.encrypted
        || entry.template
        || capture.encrypted
        || !capture.hypothetical
        || capture.key.raw != entry.key.raw
        || !literal_relative(Path::new(&entry.key.raw))
        || !literal_relative(&capture.source.path)
        || !present_file(&capture.raw_h)
        || !present_file(&capture.portable_h)
        || !concrete_file_or_missing(&capture.logical_r)
        || !concrete_file_or_missing(&capture.stored_before)
        || [
            (&capture.logical_r, &entry.r),
            (&capture.raw_h, &entry.raw_h),
            (&capture.portable_h, &entry.h),
        ]
        .into_iter()
        .any(|(capture_value, status_value)| capture_value != status_value)
    {
        return false;
    }
    entry.source.as_ref().map_or_else(
        || {
            entry.recorded_source.is_none()
                && matches!(entry.r, StatusObservation::Missing)
                && matches!(entry.source_tuple, StatusObservation::Missing)
                && matches!(capture.stored_before, StatusObservation::Missing)
                && entry.physical_plain.is_none()
                && capture.source.path.as_os_str().as_bytes() == entry.key.raw.as_bytes()
        },
        |source| {
            present_file(&capture.stored_before)
                && entry.physical_plain.as_ref() == Some(&capture.stored_before)
                && capture.source.path.as_os_str().as_bytes() == source.raw.as_bytes()
        },
    )
}

/// One projected repository edit.
#[derive(Debug, Deserialize)]
pub struct SyncEdit {
    /// Literal repository path.
    pub path: StatusPath,
    /// Projected stored observation.
    pub stored: StatusObservation,
    /// Physical observation where applicable.
    #[serde(deserialize_with = "nullable")]
    pub observed: Option<StatusObservation>,
    /// Temporary tracked path removal.
    pub remove_tracked_temporary: bool,
    /// Projection basis.
    pub basis: String,
}

/// A bounded sync dry-run report, never serialized to the webview.
pub struct SyncReview {
    /// Versioned context observed by the dry run.
    pub context: StatusContext,
    /// Ordered protocol records.
    pub records: Vec<SyncRecord>,
    /// Authoritative exit, including attention.
    pub exit_code: i32,
    /// Error classification when supplied.
    pub error_kind: Option<String>,
    /// Terminal-only recovery when supplied.
    pub recovery: Option<Vec<RecoveryStep>>,
    successful_receipt: bool,
}

impl SyncReview {
    pub(crate) const fn successful_receipt(&self) -> bool {
        self.successful_receipt
    }
}

/// Parsed dry-run review or terminal failure.
pub enum SyncReply {
    /// Review data; exit 3 is an attention result.
    Review(Box<SyncReview>),
    /// A terminal error without a trusted review.
    Error {
        /// Authoritative process exit.
        exit_code: i32,
        /// Context when available.
        context: Option<Box<StatusContext>>,
        /// Error classification.
        error_kind: Option<String>,
        /// Terminal-only recovery.
        recovery: Option<Vec<RecoveryStep>>,
    },
}

pub(crate) fn validate_sync_record(record: &SyncRecord) -> Result<(), String> {
    match record {
        SyncRecord::Outgoing { knowledge, .. } if knowledge != "last_fetched" => {
            Err("invalid remote basis".into())
        }
        SyncRecord::Stage { dry_run, .. } | SyncRecord::Hooks { dry_run, .. } if !dry_run => {
            Err("not a dry run".into())
        }
        SyncRecord::Resolved { hypothetical, .. } if !hypothetical => {
            Err("non-hypothetical result".into())
        }
        SyncRecord::Capture(capture) => {
            let key = Path::new(&capture.key.raw);
            let valid_key = match capture.scope.as_str() {
                "home" => literal_relative(key),
                "system" => key.strip_prefix("/").is_ok_and(literal_relative),
                _ => false,
            };
            if !capture.hypothetical || !literal_relative(&capture.source.path) || !valid_key {
                return Err("invalid capture".into());
            }
            for observation in [
                &capture.logical_r,
                &capture.raw_h,
                &capture.portable_h,
                &capture.stored_before,
                &capture.stored_after,
            ] {
                observation.validate()?;
            }
            Ok(())
        }
        SyncRecord::RepoEdit(edit) | SyncRecord::Own(edit) => {
            if !literal_relative(&edit.path.path)
                || !matches!(
                    edit.basis.as_str(),
                    "worktree" | "projected" | "index" | "held_reset" | "temporary_removal"
                )
            {
                return Err("invalid outgoing path".into());
            }
            edit.stored.validate()?;
            if let Some(observed) = &edit.observed {
                observed.validate()?;
            }
            Ok(())
        }
        SyncRecord::AheadCommit { paths, .. }
            if paths.iter().any(|path| !literal_relative(&path.path)) =>
        {
            Err("invalid commit path".into())
        }
        SyncRecord::MassDeleteReview { paths }
            if paths.is_empty()
                || paths.len() > 256
                || paths.iter().any(|path| !literal_relative(&path.path)) =>
        {
            Err("invalid mass deletion paths".into())
        }
        _ => Ok(()),
    }
}

/// Parse and context-bind a sync dry-run after the child has exited.
///
/// # Errors
/// Rejects malformed, incomplete, oversized or contradictory responses.
pub fn parse_sync(
    stdout: &[u8],
    exit: Option<i32>,
    expected: &StatusContext,
) -> Result<SyncReply, String> {
    if stdout.len() > MAX_REPORT || !stdout.ends_with(b"\n") {
        return Err("invalid sync framing".into());
    }
    let exit = exit.ok_or("abnormal sync exit")?;
    let mut context = None;
    let mut records = Vec::new();
    let mut result = None;
    for (index, line) in stdout
        .strip_suffix(b"\n")
        .ok_or("missing newline")?
        .split(|b| *b == b'\n')
        .enumerate()
    {
        if index >= MAX_RECORDS || line.is_empty() || line.len() > MAX_RECORD || result.is_some() {
            return Err("invalid sync framing".into());
        }
        let header: Header = serde_json::from_slice(line).map_err(|_| "invalid sync record")?;
        match header.record.as_str() {
            "context" if index == 0 => {
                let value: StatusContext =
                    serde_json::from_slice(line).map_err(|_| "invalid sync context")?;
                value.validate("sync")?;
                context = Some(value);
            }
            "result" => {
                let value: ResultRecord =
                    serde_json::from_slice(line).map_err(|_| "invalid result")?;
                if value.exit_code != exit
                    || value.outcome != if exit == 0 { "done" } else { "error" }
                    || !matches!(exit, 0 | 1 | 3 | 4)
                    || (exit == 0 && (value.error_kind.is_some() || value.recovery.is_some()))
                {
                    return Err("sync exit mismatch".into());
                }
                if let Some(steps) = &value.recovery {
                    validate_recovery(steps)?;
                }
                result = Some(value);
            }
            _ if context.is_some() => {
                let record: SyncRecord =
                    serde_json::from_slice(line).map_err(|_| "invalid sync event")?;
                validate_sync_record(&record)?;
                records.push(record);
            }
            _ => return Err("unexpected sync record".into()),
        }
    }
    let result = result.ok_or("missing final result")?;
    let mass_delete = exit == 1
        && result.error_kind.as_deref() == Some("mass_delete")
        && result.recovery.is_none()
        && records
            .iter()
            .filter(|r| matches!(r, SyncRecord::MassDeleteReview { .. }))
            .count()
            == 1;
    if matches!(exit, 1 | 4) && !mass_delete {
        return Ok(SyncReply::Error {
            exit_code: exit,
            context: context.map(Box::new),
            error_kind: result.error_kind,
            recovery: result.recovery,
        });
    }
    let context = context.ok_or("missing sync context")?;
    if !same_context(expected, &context) {
        return Err("sync context changed".into());
    }
    let outgoing = records
        .iter()
        .filter(|r| matches!(r, SyncRecord::Outgoing { .. }))
        .count();
    if outgoing > 1 {
        return Err("duplicate outgoing report".into());
    }
    let position = |name: &str| {
        records
            .iter()
            .position(|record| matches!(record, SyncRecord::Stage { stage, .. } if stage == name))
    };
    if (exit == 0 || mass_delete)
        && (outgoing != 1
            || !matches!(records.first(), Some(SyncRecord::Outgoing { .. }))
            || position("blocked_by_repo_hold").is_some()
            || !matches!((position("capture"), position("commit"), position("hooks")),
            (Some(capture), Some(commit), Some(hooks)) if capture < commit && commit < hooks))
    {
        return Err("incomplete sync report".into());
    }
    Ok(SyncReply::Review(Box::new(SyncReview {
        context,
        records,
        exit_code: exit,
        error_kind: result.error_kind,
        recovery: result.recovery,
        successful_receipt: exit == 0 || mass_delete,
    })))
}

fn same_observation(expected: &StatusObservation, actual: &StatusObservation) -> bool {
    match expected {
        StatusObservation::Unavailable { .. } => true,
        _ => expected == actual,
    }
}

/// Escapes untrusted diff text for display.
#[must_use]
pub fn escape_diff(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    for c in text.chars() {
        if c.is_control() && c != '\n' && c != '\t' {
            out.extend(c.escape_debug());
        } else if matches!(c, '\u{ad}' | '\u{61c}' | '\u{180e}' | '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{e0001}' | '\u{e0020}'..='\u{e007f}')
        {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse and bind an exact diff to the selected status snapshot.
///
/// # Errors
/// Rejects malformed, incomplete, stale or contradictory reports.
pub fn parse_diff(
    stdout: &[u8],
    exit: Option<i32>,
    selected: &DiffSelection,
) -> Result<DiffReply, String> {
    if stdout.len() > MAX_REPORT || !stdout.ends_with(b"\n") {
        return Err("invalid diff framing".into());
    }
    let exit = exit.ok_or("abnormal diff exit")?;
    let mut context = None;
    let mut diff = None;
    let mut count = None;
    let mut result = None;
    for (index, line) in stdout
        .strip_suffix(b"\n")
        .ok_or("missing newline")?
        .split(|b| *b == b'\n')
        .enumerate()
    {
        if index >= MAX_RECORDS || line.is_empty() || line.len() > MAX_RECORD || result.is_some() {
            return Err("invalid diff framing".into());
        }
        let header: Header = serde_json::from_slice(line).map_err(|_| "invalid diff record")?;
        match header.record.as_str() {
            "context" if index == 0 => {
                let value: StatusContext =
                    serde_json::from_slice(line).map_err(|_| "invalid diff context")?;
                value.validate("diff")?;
                context = Some(value);
            }
            "diff" if context.is_some() && diff.is_none() && count.is_none() => {
                diff =
                    Some(serde_json::from_slice::<DiffRecord>(line).map_err(|_| "invalid diff")?);
            }
            "report" if context.is_some() && diff.is_some() && count.is_none() => {
                count = Some(
                    serde_json::from_slice::<DiffCount>(line)
                        .map_err(|_| "invalid diff count")?
                        .count,
                );
            }
            "result" => {
                let value: ResultRecord =
                    serde_json::from_slice(line).map_err(|_| "invalid result")?;
                if value.exit_code != exit
                    || value.outcome != if exit == 0 { "done" } else { "error" }
                    || !matches!(exit, 0 | 1 | 3 | 4)
                    || (exit == 0 && (value.error_kind.is_some() || value.recovery.is_some()))
                {
                    return Err("diff exit mismatch".into());
                }
                if let Some(steps) = &value.recovery {
                    validate_recovery(steps)?;
                }
                result = Some(value);
            }
            _ => return Err("unexpected diff record".into()),
        }
    }
    let result = result.ok_or("missing result")?;
    if exit != 0 {
        return Ok(DiffReply::Error {
            exit_code: exit,
            context: context.map(Box::new),
            error_kind: result.error_kind,
            recovery: result.recovery,
        });
    }
    if count != Some(1)
        || !context
            .as_ref()
            .is_some_and(|value| same_context(&selected.context, value))
    {
        return Err("invalid diff count or context".into());
    }
    bind_diff(diff.ok_or("missing diff")?, selected)
}

fn bind_diff(value: DiffRecord, selected: &DiffSelection) -> Result<DiffReply, String> {
    let entry = &selected.entry;
    let expected = if entry.scope == "home" {
        selected.context.home.path.join(&entry.key.raw)
    } else {
        PathBuf::from(&entry.key.raw)
    };
    if value.key.raw != entry.key.raw
        || value.scope != entry.scope
        || !value.path.path.is_absolute()
        || (entry.scope == "home" && value.path.path != expected)
        || value.source.as_ref().map(|s| &s.raw) != entry.source.as_ref().map(|s| &s.raw)
        || !matches!(
            value.status.as_str(),
            "equal"
                | "different"
                | "blocked"
                | "locked"
                | "template_error"
                | "unreadable"
                | "invalid"
                | "ambiguous"
                | "refused"
        )
    {
        return Err("diff selection changed".into());
    }
    for (before, after) in [
        (&entry.r, &value.r),
        (&entry.source_tuple, &value.source_tuple),
        (&entry.h, &value.h),
        (&entry.raw_h, &value.raw_h),
    ] {
        after.validate()?;
        if !same_observation(before, after) {
            return Err("diff snapshot changed".into());
        }
    }
    if entry.physical_plain != value.physical_plain {
        return Err("diff physical source changed".into());
    }
    if let Some(observation) = &value.physical_plain {
        observation.validate()?;
    }
    if let Some(edits) = &value.line_edits {
        let mut ids = std::collections::BTreeSet::new();
        if value.status != "different"
            || edits.is_empty()
            || edits.iter().any(|edit| {
                edit.id == 0
                    || !ids.insert(edit.id)
                    || edit.old_line == Some(0)
                    || edit.new_line == Some(0)
                    || edit.old_line.is_some() != edit.before.is_some()
                    || edit.new_line.is_some() != edit.after.is_some()
                    || (edit.before.is_none() && edit.after.is_none())
            })
        {
            return Err("invalid line edits".into());
        }
    }
    let text = match value.payload {
        Some(payload) if value.status == "different" => {
            let bytes = match (payload.raw, payload.bytes) {
                (Some(raw), None) => raw.into_bytes(),
                (None, Some(bytes)) => bytes,
                _ => return Err("invalid payload".into()),
            };
            Some(escape_diff(&bytes))
        }
        None if value.status != "different" => None,
        _ => return Err("invalid diff payload state".into()),
    };
    Ok(DiffReply::Report(DiffView {
        status: value.status,
        system_off: value.system_off,
        text,
        line_edits: value.line_edits,
    }))
}

#[cfg(test)]
mod tests {
    use super::{StatusReply, parse_status};

    const CONTEXT: &str = r#"{"record":"context","schema_version":1,"experimental":true,"command":"status","home":{"raw":"/home","shown":"/home"},"repo":{"raw":"/repo","shown":"/repo"},"config":{"raw":"/config","shown":"/config"},"repo_config":{"raw":"/repo/.dotman.toml","shown":"/repo/.dotman.toml"},"state":{"raw":"/state","shown":"/state"},"identity":{"raw":"/identity","shown":"/identity"},"age_key_file":null,"repo_id":{"raw":"id","shown":"id"},"machine":{"host":{"raw":"h","shown":"h"},"user":{"raw":"u","shown":"u"},"os":{"raw":"linux","shown":"linux"},"distro":{"raw":"","shown":""},"like":{"raw":"","shown":""},"arch":{"raw":"x","shown":"x"},"profiles":[]}}"#;
    const ENTRY: &str = r#"{"record":"entry","status":"clean","key":{"raw":"x","shown":"x"},"scope":"home","path":null,"source":null,"recorded_source":null,"encrypted":false,"template":false,"system":false,"row":"clean","row_number":4,"category":null,"r":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}},"source_tuple":{"state":"missing"},"h":{"state":"missing"},"raw_h":{"state":"blocked","reason":{"raw":"directory","shown":"directory"}},"s":{"state":"unavailable","reason":"not_observed"},"identity_changed":false,"update_state":false}"#;
    const REVIEW: &str = r#"{"record":"review_context","hooks":{"standing":"empty"},"remote":{"basis":"unknown","ahead":null,"behind":null}}"#;
    const SUMMARY: &str = r#"{"record":"summary","rows":1,"clean":1,"new":0,"held":0,"held_in_home":1,"held_in_repo":0,"unrecorded":0}"#;
    const RESULT: &str = r#"{"record":"result","outcome":"done","exit_code":0,"error_kind":null}"#;

    fn report() -> String {
        let held = ENTRY.replace("\"status\":\"clean\"", "\"status\":\"held-in-home\"");
        format!("{CONTEXT}\n{ENTRY}\n{held}\n{REVIEW}\n{SUMMARY}\n{RESULT}\n")
    }

    #[test]
    fn hook_projection_is_retained_only_when_complete() -> Result<(), Box<dyn std::error::Error>> {
        let hooks = r#"{"standing":"unapproved","canonical":{"raw":"[[\"x\",\"run\"]]","shown":"table"},"entries":[{"pattern":{"raw":"x","shown":"x"},"command":{"raw":"run","shown":"run"}}],"pending_keys":[{"raw":"x","shown":"x"}],"pending_commands":[{"raw":"run","shown":"run"}]}"#;
        let input = report().replace(r#"{"standing":"empty"}"#, hooks);
        let StatusReply::Report(report) = parse_status(input.as_bytes(), Some(0))? else {
            return Err("expected report".into());
        };
        let (standing, projection) = *report.hooks.ok_or("missing projection")?;
        assert_eq!(standing, "unapproved");
        assert_eq!(projection.canonical.raw, r#"[["x","run"]]"#);
        assert_eq!(
            projection
                .entries
                .first()
                .ok_or("missing hook entry")?
                .command
                .raw,
            "run"
        );
        assert_eq!(
            projection
                .pending_keys
                .first()
                .ok_or("missing pending key")?
                .raw,
            "x"
        );
        assert_eq!(
            projection
                .pending_commands
                .first()
                .ok_or("missing pending command")?
                .raw,
            "run"
        );

        let incomplete = input.replace(r#","pending_commands":[{"raw":"run","shown":"run"}]"#, "");
        let StatusReply::Report(report) = parse_status(incomplete.as_bytes(), Some(0))? else {
            return Err("expected report".into());
        };
        assert!(report.hooks.is_none());
        let unsupported = input.replace(
            r#""standing":"unapproved""#,
            r#""standing":"unapproved","redacted":true"#,
        );
        let StatusReply::Report(report) = parse_status(unsupported.as_bytes(), Some(0))? else {
            return Err("expected report".into());
        };
        assert!(report.hooks.is_none());
        Ok(())
    }

    fn capture_pair() -> Result<(super::SyncCapture, super::StatusEntry), serde_json::Error> {
        let tuple = r#"{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}}"#;
        let entry = ENTRY
            .replace(
                r#""source":null"#,
                r#""source":{"raw":"x","shown":"alias"}"#,
            )
            .replace(
                r#""source_tuple":{"state":"missing"}"#,
                &format!(r#""source_tuple":{tuple},"physical_plain":{tuple}"#),
            )
            .replace(r#""h":{"state":"missing"}"#, &format!(r#""h":{tuple}"#))
            .replace(
                r#""raw_h":{"state":"blocked","reason":{"raw":"directory","shown":"directory"}}"#,
                &format!(r#""raw_h":{tuple}"#),
            );
        let capture = format!(
            r#"{{"key":{{"raw":"x","shown":"other"}},"source":{{"raw":"x","shown":"alias"}},"scope":"home","logical_r":{tuple},"raw_h":{tuple},"portable_h":{tuple},"stored_before":{tuple},"stored_after":{tuple},"kept_mode":null,"repo_mode":null,"hypothetical":true,"encrypted":false}}"#
        );
        Ok((
            serde_json::from_str(&capture)?,
            serde_json::from_str(&entry)?,
        ))
    }

    #[test]
    fn capture_matches_exact_plain_file_snapshot() -> Result<(), serde_json::Error> {
        use super::{StatusObservation, capture_matches_status};
        let (mut capture, mut entry) = capture_pair()?;
        assert!(capture_matches_status(&capture, &entry));
        entry.key.shown = "another alias".into();
        assert!(capture_matches_status(&capture, &entry));
        capture.key.raw = "other".into();
        assert!(!capture_matches_status(&capture, &entry));
        capture.key.raw = "x".into();
        capture.source.path = "other".into();
        assert!(!capture_matches_status(&capture, &entry));
        capture.source.path = "./x".into();
        assert!(!capture_matches_status(&capture, &entry));
        capture.source.path = "x".into();
        if let Some(source) = &mut entry.source {
            source.raw = "other".into();
            source.shown = "x".into();
        }
        assert!(!capture_matches_status(&capture, &entry));
        if let Some(source) = &mut entry.source {
            source.raw = "x{host}".into();
        }
        capture.source.path = "x{host}".into();
        assert!(capture_matches_status(&capture, &entry));
        for which in 0..4 {
            let (mut changed, original) = capture_pair()?;
            let observation = match which {
                0 => &mut changed.logical_r,
                1 => &mut changed.raw_h,
                2 => &mut changed.portable_h,
                _ => &mut changed.stored_before,
            };
            if let StatusObservation::Present { tuple } = observation {
                tuple.hash = "1123456789abcdef0123456789abcdef".into();
            }
            assert!(!capture_matches_status(&changed, &original));
        }
        let (mut capture, entry) = capture_pair()?;
        if let StatusObservation::Present { tuple } = &mut capture.stored_before {
            tuple.mode = "0644".into();
        }
        assert!(!capture_matches_status(&capture, &entry));
        let (mut capture, mut entry) = capture_pair()?;
        if let Some(StatusObservation::Present { tuple }) = &mut entry.physical_plain {
            tuple.mode = "0644".into();
        }
        if let StatusObservation::Present { tuple } = &mut capture.stored_before {
            tuple.mode = "0644".into();
        }
        assert!(capture_matches_status(&capture, &entry));
        entry.physical_plain = None;
        assert!(!capture_matches_status(&capture, &entry));
        Ok(())
    }

    #[test]
    fn capture_match_new_and_fail_closed_cases() -> Result<(), serde_json::Error> {
        use super::{StatusObservation, capture_matches_status};
        let (mut capture, mut entry) = capture_pair()?;
        capture.logical_r = StatusObservation::Missing;
        capture.stored_before = StatusObservation::Missing;
        entry.r = StatusObservation::Missing;
        entry.source_tuple = StatusObservation::Missing;
        entry.physical_plain = None;
        entry.source = None;
        assert!(capture_matches_status(&capture, &entry));
        capture.stored_before = capture.raw_h.clone();
        assert!(!capture_matches_status(&capture, &entry));
        capture.stored_before = StatusObservation::Missing;
        entry.physical_plain = Some(capture.raw_h.clone());
        assert!(!capture_matches_status(&capture, &entry));
        entry.physical_plain = None;
        entry.recorded_source = Some(super::StatusText {
            raw: "x".into(),
            shown: "x".into(),
        });
        assert!(!capture_matches_status(&capture, &entry));
        entry.recorded_source = None;
        capture.source.path = "other".into();
        assert!(!capture_matches_status(&capture, &entry));

        let (mut capture, mut entry) = capture_pair()?;
        capture.logical_r = StatusObservation::Unavailable {
            reason: "not_observed".into(),
        };
        entry.r = capture.logical_r.clone();
        assert!(!capture_matches_status(&capture, &entry));
        let (mut capture, mut entry) = capture_pair()?;
        capture.stored_before = StatusObservation::Blocked {
            reason: super::StatusText {
                raw: "blocked".into(),
                shown: "blocked".into(),
            },
        };
        entry.source_tuple = capture.stored_before.clone();
        entry.physical_plain = Some(capture.stored_before.clone());
        assert!(!capture_matches_status(&capture, &entry));

        for which in 0..7 {
            let (mut capture, mut entry) = capture_pair()?;
            match which {
                0 => capture.encrypted = true,
                1 => entry.encrypted = true,
                2 => entry.template = true,
                3 => entry.system = true,
                4 => entry.scope = "system".into(),
                5 => capture.scope = "system".into(),
                _ => capture.hypothetical = false,
            }
            assert!(!capture_matches_status(&capture, &entry));
        }
        let (mut capture, mut entry) = capture_pair()?;
        if let StatusObservation::Present { tuple } = &mut capture.raw_h {
            tuple.kind = "symlink".into();
        }
        entry.raw_h = capture.raw_h.clone();
        assert!(!capture_matches_status(&capture, &entry));
        Ok(())
    }

    #[test]
    fn preserves_occurrences() {
        let parsed = parse_status(report().as_bytes(), Some(0));
        assert!(matches!(&parsed, Ok(StatusReply::Report(report)) if report.entries.len() == 2));
    }

    #[test]
    fn rejects_truncation_version_order_and_exit_mismatch() {
        let report = report();
        assert!(parse_status(report.trim_end().as_bytes(), Some(0)).is_err());
        assert!(parse_status(report.as_bytes(), None).is_err());
        assert!(parse_status(report.as_bytes(), Some(3)).is_err());
        assert!(
            parse_status(
                report
                    .replace("\"schema_version\":1", "\"schema_version\":2")
                    .as_bytes(),
                Some(0)
            )
            .is_err()
        );
        assert!(
            parse_status(
                report
                    .replace("\"record\":\"review_context\"", "\"record\":\"entry\"")
                    .as_bytes(),
                Some(0)
            )
            .is_err()
        );
        assert!(parse_status(&vec![b'x'; super::MAX_REPORT + 1], Some(0)).is_err());
    }

    #[test]
    fn accepts_early_error_without_context() {
        let error = b"{\"record\":\"result\",\"outcome\":\"error\",\"exit_code\":1,\"error_kind\":\"root_refused\"}\n";
        assert!(matches!(
            parse_status(error, Some(1)),
            Ok(StatusReply::Error { exit_code: 1, .. })
        ));
    }

    #[test]
    fn rejects_duplicate_protocol_fields() {
        let report = report();
        let duplicate_context = report.replace(
            "\"schema_version\":1",
            "\"schema_version\":1,\"schema_version\":1",
        );
        let duplicate_record = report.replace(
            "\"record\":\"entry\"",
            "\"record\":\"entry\",\"record\":\"entry\"",
        );
        let duplicate_result = report.replace("\"exit_code\":0", "\"exit_code\":0,\"exit_code\":0");
        for input in [duplicate_context, duplicate_record, duplicate_result] {
            assert!(parse_status(input.as_bytes(), Some(0)).is_err());
        }
    }

    #[test]
    fn validates_context_entries_and_bytes() {
        let report = report();
        let bytes = report.replace(
            r#""repo":{"raw":"/repo","shown":"/repo"}"#,
            r#""repo":{"bytes":[47,114,255],"shown":"/r�"}"#,
        );
        assert!(matches!(
            parse_status(bytes.as_bytes(), Some(0)),
            Ok(StatusReply::Report(value)) if value.context.repo.path.as_os_str().as_encoded_bytes() == b"/r\xff"
                && value.context.repo.shown == "/r�"
                && value.entries.len() == 2
        ));
        for bad in [
            report.replace(
                r#""repo":{"raw":"/repo","shown":"/repo"}"#,
                r#""repo":{"raw":"/repo","bytes":[47],"shown":"/repo"}"#,
            ),
            report.replace(
                r#""home":{"raw":"/home","shown":"/home"}"#,
                r#""home":{"raw":"relative","shown":"relative"}"#,
            ),
            report.replace(
                r#""key":{"raw":"x","shown":"x"}"#,
                r#""key":{"raw":"../x","shown":"x"}"#,
            ),
            report.replace(r#""mode":"0600""#, r#""mode":"0888""#),
            report.replace(
                r#""hash":"0123456789abcdef0123456789abcdef""#,
                r#""hash":"BAD""#,
            ),
            report.replace(r#","identity_changed":false"#, ""),
            report.replace(r#""rows":1"#, r#""rows":1,"rows":1"#),
            report.replace(
                r#""repo_id":{"raw":"id","shown":"id"}"#,
                r#""repo_id":null"#,
            ),
            report.replace(r#""command":"status""#, r#""command":"sync""#),
            report.replace(r#""path":null"#, r#""path":{"raw":"x","shown":"x"}"#),
            report.replace(r#""raw":"/repo""#, r#""raw":"/r\u0000epo""#),
        ] {
            assert!(parse_status(bad.as_bytes(), Some(0)).is_err());
        }
    }

    #[test]
    fn special_scopes_and_additive_fields() {
        let system = ENTRY
            .replace(
                r#""key":{"raw":"x","shown":"x"}"#,
                r#""key":{"raw":"/etc/x","shown":"/etc/x"}"#,
            )
            .replace(r#""scope":"home""#, r#""scope":"system""#)
            .replace(r#""system":false"#, r#""system":true"#);
        let repo = ENTRY
            .replace(r#""scope":"home""#, r#""scope":"repo_source""#)
            .replace(
                r#""path":null"#,
                r#""path":{"bytes":[120,255],"shown":"x�"}"#,
            );
        let input = format!("{CONTEXT}\n{system}\n{repo}\n{REVIEW}\n{SUMMARY}\n{RESULT}\n");
        assert!(parse_status(input.as_bytes(), Some(0)).is_ok());
        let additive = report().replace(
            r#""schema_version":1"#,
            r#""future":{"x":true},"schema_version":1"#,
        );
        assert!(parse_status(additive.as_bytes(), Some(0)).is_ok());
    }

    #[test]
    fn accepts_ten_thousand_occurrences() {
        let mut input = format!("{CONTEXT}\n");
        for _ in 0..10_000 {
            input.push_str(ENTRY);
            input.push('\n');
        }
        for record in [REVIEW, SUMMARY, RESULT] {
            input.push_str(record);
            input.push('\n');
        }
        assert!(matches!(
            parse_status(input.as_bytes(), Some(0)),
            Ok(StatusReply::Report(report)) if report.entries.len() == 10_000
        ));
    }

    #[test]
    fn error_keeps_optional_context_without_report() {
        let error =
            r#"{"record":"result","outcome":"error","exit_code":4,"error_kind":"in_progress"}"#;
        let context = CONTEXT.replace(
            r#""repo_id":{"raw":"id","shown":"id"}"#,
            r#""repo_id":null"#,
        );
        let input = format!("{context}\n{error}\n");
        assert!(matches!(
            parse_status(input.as_bytes(), Some(4)),
            Ok(StatusReply::Error { context: Some(ctx), recovery: None, .. }) if ctx.repo_id.is_none()
        ));
    }

    #[test]
    fn recovery_is_unquoted_data_and_validates_argv() -> Result<(), String> {
        let input = br#"[{"kind":"command","argv":["git",{"bytes":[255]}],"shown":["git","display"],"when":"after stop"},{"kind":"instruction","text":{"raw":"a\nb","shown":"a\\nb"}}]"#;
        let steps = super::parse_recovery(input)?;
        assert!(matches!(
            steps.first(),
            Some(super::RecoveryStep::Command { argv, shown, when })
                if matches!(argv.get(1), Some(super::RecoveryArgument::Bytes { bytes }) if bytes == &[255])
                    && shown.get(1).is_some_and(|value| value == "display")
                    && when.as_deref() == Some("after stop")
        ));
        assert!(
            super::parse_recovery(br#"[{"kind":"command","argv":[],"shown":[],"when":null}]"#)
                .is_err()
        );
        assert!(
            super::parse_recovery(br#"[{"kind":"command","argv":["x"],"shown":[],"when":null}]"#)
                .is_err()
        );
        assert!(
            super::parse_recovery(
                br#"[{"kind":"command","argv":[{"bytes":[0]}],"shown":["x"],"when":null}]"#
            )
            .is_err()
        );
        assert!(
            super::parse_recovery(br#"[{"kind":"command","argv":["x"],"shown":["x"]}]"#).is_err()
        );
        Ok(())
    }

    #[test]
    fn exact_diff_binds_snapshot_and_preserves_payload_bytes() -> Result<(), String> {
        let StatusReply::Report(report) = parse_status(report().as_bytes(), Some(0))? else {
            return Err("missing report".into());
        };
        let selection = super::DiffSelection {
            context: report.context,
            entry: report.entries.into_iter().next().ok_or("missing entry")?,
        };
        let context = CONTEXT.replace(r#""command":"status""#, r#""command":"diff""#);
        let diff = r#"{"record":"diff","key":{"raw":"x","shown":"x"},"scope":"home","path":{"raw":"/home/x","shown":"/home/x"},"status":"different","system_off":false,"source":null,"r":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}},"source_tuple":{"state":"missing"},"h":{"state":"missing"},"raw_h":{"state":"blocked","reason":{"raw":"directory","shown":"directory"}},"payload":{"bytes":[97,10,9,255,27,226,128,174],"shown":"ignored"}}"#;
        let input = format!("{context}\n{diff}\n{{\"record\":\"report\",\"count\":1}}\n{RESULT}\n");
        let mut physical_selection = selection.clone();
        let physical = r#"{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0644"}}"#;
        physical_selection.entry.physical_plain =
            Some(serde_json::from_str(physical).map_err(|e| e.to_string())?);
        let physical_input = input.replace(
            r#""system_off":false"#,
            &format!(r#""physical_plain":{physical},"system_off":false"#),
        );
        assert!(super::parse_diff(physical_input.as_bytes(), Some(0), &physical_selection).is_ok());
        assert!(
            super::parse_diff(
                physical_input
                    .replace(r#""mode":"0644""#, r#""mode":"0600""#)
                    .as_bytes(),
                Some(0),
                &physical_selection,
            )
            .is_err()
        );
        assert!(super::parse_diff(input.as_bytes(), Some(0), &physical_selection).is_err());
        let line = r#"{"id":1,"old_line":1,"new_line":1,"before":"old\n","after":"new\n"}"#;
        let with_lines = input.replace(
            r#""system_off":false"#,
            &format!(r#""line_edits":[{line}],"system_off":false"#),
        );
        assert!(
            matches!(super::parse_diff(with_lines.as_bytes(), Some(0), &selection), Ok(super::DiffReply::Report(view)) if view.line_edits.is_some())
        );
        for bad_line in [
            line.replace(r#""id":1"#, r#""id":0"#),
            line.replace(r#""old_line":1"#, r#""old_line":null"#),
            line.replace(r#""new_line":1,"#, ""),
        ] {
            assert!(
                super::parse_diff(
                    with_lines.replace(line, &bad_line).as_bytes(),
                    Some(0),
                    &selection
                )
                .is_err()
            );
        }
        assert!(
            super::parse_diff(
                with_lines
                    .replace(line, &format!("{line},{line}"))
                    .as_bytes(),
                Some(0),
                &selection
            )
            .is_err()
        );
        match super::parse_diff(input.as_bytes(), Some(0), &selection)? {
            super::DiffReply::Report(view) => {
                assert_eq!(view.text.as_deref(), Some("a\n\t�\\u{1b}\\u{202e}"));
            }
            super::DiffReply::Error { .. } => return Err("missing diff".into()),
        }
        let empty = input.replace(r#""bytes":[97,10,9,255,27,226,128,174]"#, r#""raw":"""#);
        assert!(
            matches!(super::parse_diff(empty.as_bytes(), Some(0), &selection), Ok(super::DiffReply::Report(view)) if view.text.as_deref() == Some(""))
        );
        for bad in [
            input.replace(r#""count":1"#, r#""count":2"#),
            input.replace(r#""schema_version":1"#, r#""schema_version":2"#),
            input.replace(r#""raw":"/repo""#, r#""raw":"/other""#),
            input.replace(r#""raw":"/home/x""#, r#""raw":"/home/y""#),
            input.replace(r#""raw":"x","shown":"x""#, r#""raw":"y","shown":"x""#),
            input.replace(
                r#""source":null"#,
                r#""source":{"raw":"other","shown":"other"}"#,
            ),
            input.replace(
                "0123456789abcdef0123456789abcdef",
                "1123456789abcdef0123456789abcdef",
            ),
            input.replace(
                r#""bytes":[97,10,9,255,27,226,128,174]"#,
                r#""raw":"x","bytes":[1]"#,
            ),
        ] {
            assert!(super::parse_diff(bad.as_bytes(), Some(0), &selection).is_err());
        }
        assert!(super::parse_diff(input.as_bytes(), Some(1), &selection).is_err());
        assert!(super::parse_diff(input.trim_end().as_bytes(), Some(0), &selection).is_err());
        Ok(())
    }

    #[test]
    fn sync_review_validates_context_framing_and_partial_exit() -> Result<(), String> {
        let StatusReply::Report(status) = parse_status(report().as_bytes(), Some(0))? else {
            return Err("missing status".into());
        };
        let context = CONTEXT.replace(r#""command":"status""#, r#""command":"sync""#);
        let outgoing = r#"{"record":"outgoing","head":null,"tracking_ref":null,"tracking_commit":null,"knowledge":"last_fetched","warnings":[]}"#;
        let stages = ["capture", "commit", "hooks"]
            .map(|stage| format!(r#"{{"record":"stage","stage":"{stage}","dry_run":true}}"#))
            .join("\n");
        let edit = r#"{"record":"repo_edit","path":{"raw":"private-path","shown":"<img>"},"stored":{"state":"missing"},"observed":null,"basis":"index","remove_tracked_temporary":false}"#;
        let input = format!("{context}\n{outgoing}\n{stages}\n{edit}\n{RESULT}\n");
        assert!(
            matches!(super::parse_sync(input.as_bytes(), Some(0), &status.context), Ok(super::SyncReply::Review(value)) if value.records.len() == 5)
        );
        for bad in [
            input.trim_end().to_owned(),
            input.replace(r#""schema_version":1"#, r#""schema_version":2"#),
            input.replace(r#""raw":"/repo""#, r#""raw":"/other""#),
            input.replace(r#""dry_run":true"#, r#""dry_run":false"#),
            input.replace(r#""record":"result""#, r#""record":"unknown""#),
            input.replace(r#""stage":"hooks""#, r#""stage":"other""#),
            input
                .replace(r#""stage":"capture""#, r#""stage":"temporary""#)
                .replace(r#""stage":"commit""#, r#""stage":"capture""#)
                .replace(r#""stage":"temporary""#, r#""stage":"commit""#),
            input.replace(r#""raw":"private-path""#, r#""raw":"../escape""#),
        ] {
            assert!(super::parse_sync(bad.as_bytes(), Some(0), &status.context).is_err());
        }
        assert!(super::parse_sync(input.as_bytes(), Some(3), &status.context).is_err());
        assert!(
            super::parse_sync(&vec![b'x'; super::MAX_REPORT + 1], Some(0), &status.context)
                .is_err()
        );
        let attention = r#"{"record":"result","outcome":"error","exit_code":3,"error_kind":null}"#;
        let partial = format!("{context}\n{outgoing}\n{attention}\n");
        assert!(
            matches!(super::parse_sync(partial.as_bytes(), Some(3), &status.context), Ok(super::SyncReply::Review(value)) if value.exit_code == 3)
        );
        let recovery = r#"{"record":"result","outcome":"error","exit_code":4,"error_kind":"in_progress","recovery":[{"kind":"command","argv":["private"],"shown":["shown"],"when":null}]}"#;
        let stopped = format!("{context}\n{recovery}\n");
        assert!(
            matches!(super::parse_sync(stopped.as_bytes(), Some(4), &status.context),
            Ok(super::SyncReply::Error { exit_code: 4, context: Some(_), error_kind: Some(kind), recovery: Some(_), .. }) if kind == "in_progress")
        );
        Ok(())
    }
}
