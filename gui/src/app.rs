use dotman_gui::{
    decisions::{ApplyAction, CompleteReview, ReviewedApply, ReviewedIdentity},
    hook_trust::HookReview,
    protocol, recovery, runner, setup, writer_outcome,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

pub mod held;
pub mod import_setup;
pub mod keys;
pub mod policy_trust;
pub mod variant;

#[derive(Serialize)]
struct ContextView {
    home: String,
    repo: String,
    config: String,
    state: String,
    identity: String,
    machine: String,
    machine_details: MachineDetails,
}

#[derive(Serialize)]
struct MachineDetails {
    host: String,
    os: String,
    arch: String,
    profiles: Vec<String>,
}

impl From<&protocol::StatusContext> for ContextView {
    fn from(context: &protocol::StatusContext) -> Self {
        Self {
            home: context.home.shown.clone(),
            repo: context.repo.shown.clone(),
            config: context.config.shown.clone(),
            state: context.state.shown.clone(),
            identity: String::from("Not shown"),
            machine: format!(
                "{} · {} · {} · {}",
                context.machine.user.shown,
                context.machine.host.shown,
                context.machine.os.shown,
                context.machine.arch.shown
            ),
            machine_details: MachineDetails {
                host: context.machine.host.shown.clone(),
                os: context.machine.os.shown.clone(),
                arch: context.machine.arch.shown.clone(),
                profiles: context
                    .machine
                    .profiles
                    .iter()
                    .map(|p| p.shown.clone())
                    .collect(),
            },
        }
    }
}

#[derive(Serialize)]
struct EntryView {
    id: String,
    key: String,
    status: String,
    group: &'static str,
    supported: bool,
    system: bool,
    hold: Option<HoldView>,
    held_actions: Vec<&'static str>,
    host_variant: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    edit_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    machine_file: Option<MachineFile>,
}

fn edit_command(
    entry: &protocol::StatusEntry,
    paths: &HeldPaths<'_>,
    home: &Path,
) -> Option<String> {
    if entry.scope != "home"
        || entry.encrypted
        || paths.matches(entry, false)
        || paths.matches(entry, true)
        || !matches!(
            entry.status.as_str(),
            "clean"
                | "changed-locally"
                | "changed-in-repo"
                | "not-applied"
                | "both-changed"
                | "template-drift"
        )
        || entry.source.is_none()
        || !matches!(&entry.source_tuple, protocol::StatusObservation::Present { tuple } if tuple.kind == "file")
    {
        return None;
    }
    let key = &entry.key.raw;
    if key.is_empty()
        || key
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    let path = home.join(key);
    let path = path.to_str()?;
    if path.chars().any(runner::unsafe_display_char) {
        return None;
    }
    Some(format!("dotman edit -- '{}'", path.replace('\'', "'\\''")))
}

#[derive(Serialize)]
struct MachineFile {
    selected: Option<String>,
    recorded: Option<String>,
    alternatives: Vec<String>,
    kind: &'static str,
    identity_changed: bool,
    ambiguous: bool,
}

#[derive(Serialize)]
struct HoldView {
    origin: &'static str,
    reason: &'static str,
    rule: Option<&'static str>,
    location: Option<String>,
    guidance: &'static str,
}

#[derive(Default, Serialize)]
struct HeldCounts {
    new_files: usize,
    home_edits: usize,
    repo_edits: usize,
}

#[derive(Serialize)]
pub struct View {
    success: bool,
    repo_unavailable: bool,
    finding: &'static str,
    message: &'static str,
    context: Option<ContextView>,
    entries: Vec<EntryView>,
    key_stripped: bool,
    clean: usize,
    remote: String,
    held_counts: HeldCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity_review_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    setup_review_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    policy_review_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hook_review_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_review_id: Option<String>,
}

fn machine_file(entry: &protocol::StatusEntry, paths: &HeldPaths<'_>) -> Option<MachineFile> {
    if entry.scope != "home" || paths.matches(entry, false) || paths.matches(entry, true) {
        return None;
    }
    let sides = entry
        .source
        .iter()
        .chain(entry.recorded_source.iter())
        .chain(&entry.alternatives)
        .chain(entry.invalid_sources.iter().map(|source| &source.source));
    for side in sides {
        let raw = side.raw.as_str();
        if raw.is_empty()
            || raw.contains('\0')
            || raw
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || paths.repo.contains(Path::new(raw))
            || paths.redacted_repo.contains(Path::new(raw))
        {
            return None;
        }
    }
    let selected = entry.source.as_ref();
    let alternatives: Vec<_> = entry
        .alternatives
        .iter()
        .filter(|side| selected.is_none_or(|source| source.raw != side.raw))
        .map(|side| side.shown.clone())
        .collect();
    let ambiguous = entry.status == "ambiguous";
    if alternatives.is_empty()
        && !entry.template
        && !entry.encrypted
        && !entry.identity_changed
        && !ambiguous
        && entry.status != "invalid"
        && entry.invalid_sources.is_empty()
        && selected.is_none_or(|source| source.raw == entry.key.raw)
        && entry
            .recorded_source
            .as_ref()
            .is_none_or(|source| source.raw == entry.key.raw)
    {
        return None;
    }
    Some(MachineFile {
        selected: selected.map(|source| source.shown.clone()),
        recorded: entry
            .recorded_source
            .as_ref()
            .map(|source| source.shown.clone()),
        alternatives,
        kind: if selected.is_none() || ambiguous || entry.status == "invalid" {
            "unknown"
        } else if entry.template {
            "template"
        } else if entry.encrypted {
            "encrypted"
        } else {
            "plain"
        },
        identity_changed: entry.identity_changed,
        ambiguous,
    })
}

fn held(entry: &protocol::StatusEntry) -> bool {
    matches!(
        entry.status.as_str(),
        "held" | "held-in-home" | "held-in-repo"
    )
}

fn redacted(entry: &protocol::StatusEntry) -> bool {
    held(entry)
        && (matches!(
            entry.hold,
            Some(protocol::Hold::SecretBinary | protocol::Hold::Unknown)
        ) || (matches!(entry.hold, Some(protocol::Hold::SecretName))
            && !held::plain_eligible(entry))
            || matches!(
                entry.finding.as_ref().map(|f| f.place),
                Some(protocol::FindingPlace::Name | protocol::FindingPlace::Unknown)
            )
            || (entry.hold.is_none() && entry.finding.is_none()))
}

#[derive(Default)]
struct HeldPaths<'a> {
    keys: HashSet<(&'a str, &'a str)>,
    redacted_keys: HashSet<(&'a str, &'a str)>,
    repo: HashSet<&'a Path>,
    redacted_repo: HashSet<&'a Path>,
}

impl<'a> HeldPaths<'a> {
    fn new(report: &'a protocol::StatusReport) -> Self {
        let mut paths = Self::default();
        for entry in report.entries.iter().filter(|entry| held(entry)) {
            if entry.scope == "repo_source" {
                if let Some(path) = &entry.path {
                    paths.repo.insert(path.path.as_path());
                    if redacted(entry) {
                        paths.redacted_repo.insert(path.path.as_path());
                    }
                }
            } else {
                paths.keys.insert((&entry.scope, &entry.key.raw));
                if redacted(entry) {
                    paths.redacted_keys.insert((&entry.scope, &entry.key.raw));
                }
            }
        }
        for entry in &report.entries {
            if entry.scope != "repo_source"
                && entry
                    .source
                    .as_ref()
                    .is_some_and(|source| paths.redacted_repo.contains(Path::new(&source.raw)))
            {
                paths.redacted_keys.insert((&entry.scope, &entry.key.raw));
            }
        }
        paths
    }

    fn matches(&self, entry: &protocol::StatusEntry, redaction: bool) -> bool {
        let (keys, repo) = if redaction {
            (&self.redacted_keys, &self.redacted_repo)
        } else {
            (&self.keys, &self.repo)
        };
        if entry.scope == "repo_source" {
            entry
                .path
                .as_ref()
                .is_some_and(|path| repo.contains(path.path.as_path()))
        } else {
            keys.contains(&(entry.scope.as_str(), entry.key.raw.as_str()))
                || entry
                    .source
                    .as_ref()
                    .is_some_and(|source| repo.contains(Path::new(&source.raw)))
        }
    }

    fn redacts_key(&self, scope: &str, key: &str) -> bool {
        self.redacted_keys.contains(&(scope, key))
            || (scope == "repo_source" && self.redacted_repo.contains(Path::new(key)))
    }
}

fn hold_view(entry: &protocol::StatusEntry) -> Option<HoldView> {
    use protocol::{FindingPlace, Hold};
    let origin = match entry.status.as_str() {
        "held" => "new_file",
        "held-in-home" => "home_edit",
        "held-in-repo" => "repo_edit",
        _ => return None,
    };
    let reason = match entry.hold {
        Some(Hold::Large) => "File is too large",
        Some(Hold::Binary) => "Binary file",
        Some(Hold::SecretName | Hold::SecretBinary) => "Sensitive file",
        Some(Hold::Special) => "Special file",
        Some(Hold::Unreadable) => "File is unreadable",
        Some(Hold::Collision) => "Path collision",
        Some(Hold::Unapproved) => "Directory is not approved",
        Some(Hold::Content) | None if entry.finding.is_some() => "Guard finding",
        _ => "Held for review",
    };
    let finding = entry.finding.as_ref();
    let rule = finding.and_then(|f| {
        const RULES: &[&str] = &[
            "age-secret-key",
            "github-token",
            "github-fine-grained-pat",
            "gitlab-pat",
            "slack-token",
            "slack-webhook",
            "aws-access-key-id",
            "stripe-live-key",
            "openai-api-key",
            "anthropic-api-key",
            "npm-token",
            "pypi-token",
            "digitalocean-token",
            "sendgrid-key",
            "huggingface-token",
            "private-key",
            "openrouter-key",
            "groq-key",
            "xai-key",
            "perplexity-key",
            "docker-pat",
        ];
        RULES.iter().copied().find(|rule| *rule == f.rule)
    });
    let location = finding.and_then(|f| match f.place {
        FindingPlace::Content if f.line > 0 => Some(format!("line {}", f.line)),
        FindingPlace::Name => Some("filename".into()),
        FindingPlace::Target => Some("symlink target".into()),
        _ => None,
    });
    let guidance = match finding.map(|f| f.place) {
        Some(FindingPlace::Name) => "Rename the file; encryption cannot hide its name.",
        Some(FindingPlace::Target) => "Point the symlink elsewhere.",
        Some(FindingPlace::Content) => "Review the finding in a terminal.",
        _ => "Review this hold in a terminal.",
    };
    Some(HoldView {
        origin,
        reason,
        rule,
        location,
        guidance,
    })
}

fn group(entry: &protocol::StatusEntry) -> &'static str {
    match entry.status.as_str() {
        "clean" => "clean",
        "held" | "held-in-home" | "held-in-repo" => "held",
        "blocked" | "symlink-at-target" | "ambiguous" | "invalid" | "locked" | "unreadable"
        | "template-error" | "system-off" | "collision" => "unavailable",
        "changed-locally" | "new" => "outgoing",
        "changed-in-repo" | "not-applied" => "incoming",
        _ if entry.category.is_some() => "decision",
        _ => "other",
    }
}

fn supported(entry: &protocol::StatusEntry) -> bool {
    matches!(entry.scope.as_str(), "home" | "system")
        && entry.row.is_some()
        && !held(entry)
        && !matches!(entry.status.as_str(), "new" | "collision")
        && (!entry.encrypted || entry.source.is_some())
}

fn status_finding(reply: &protocol::StatusReply) -> &'static str {
    let protocol::StatusReply::Error {
        exit_code,
        error_kind,
        context,
        ..
    } = reply
    else {
        return "available";
    };
    match (*exit_code, error_kind.as_deref()) {
        (4, Some("in_progress")) => "in_progress",
        (4, Some("pull_conflict")) => "pull_conflict",
        (4, _) => "stopped",
        (1, Some("other")) if context.as_ref().is_some_and(|c| c.repo_id.is_none()) => {
            "unavailable"
        }
        _ => "unknown",
    }
}

fn project(reply: &protocol::StatusReply, key_stripped: bool, generation: u64) -> View {
    match reply {
        protocol::StatusReply::Report(report) => {
            let paths = HeldPaths::new(report);
            View {
                success: true,
                repo_unavailable: false,
                finding: "available",
                message: "Status loaded",
                context: Some((&report.context).into()),
                entries: report
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(id, entry)| EntryView {
                        id: format!("{generation}:{id}"),
                        key: if paths.matches(entry, true) {
                            "[redacted filename]".into()
                        } else if entry.scope == "repo_source" {
                            entry
                                .path
                                .as_ref()
                                .map_or_else(|| "[unavailable]".into(), |path| path.shown.clone())
                        } else {
                            entry.key.shown.clone()
                        },
                        status: entry.status.clone(),
                        group: group(entry),
                        supported: supported(entry) && !paths.matches(entry, false),
                        system: entry.system,
                        hold: hold_view(entry),
                        held_actions: held::actions(entry, &paths, generation, report),
                        host_variant: generation != u64::MAX && variant::eligible(entry, &paths),
                        edit_command: edit_command(entry, &paths, &report.context.home.path),
                        machine_file: machine_file(entry, &paths),
                    })
                    .collect(),
                key_stripped,
                clean: report.summary.clean,
                remote: match report.remote.basis.as_str() {
                    "last_fetched" => format!(
                        "Last fetched: {} ahead · {} behind",
                        report
                            .remote
                            .ahead
                            .map_or_else(|| "unknown".into(), |n| n.to_string()),
                        report
                            .remote
                            .behind
                            .map_or_else(|| "unknown".into(), |n| n.to_string())
                    ),
                    "not_fetched" => "Remote has not been fetched".into(),
                    _ => "Remote position unknown".into(),
                },
                held_counts: HeldCounts {
                    new_files: report.summary.held,
                    home_edits: report.summary.held_in_home,
                    repo_edits: report.summary.held_in_repo,
                },
                identity_review_id: (generation != u64::MAX
                    && ReviewedIdentity::new(report).is_ok())
                .then(|| format!("identity:{generation}")),
                setup_review_id: None,
                policy_review_id: (generation != u64::MAX).then(|| format!("policy:{generation}")),
                hook_review_id: (generation != u64::MAX
                    && !report.entries.iter().any(redacted)
                    && HookReview::new(report).is_ok())
                .then(|| format!("hooks:{generation}")),
                key_review_id: (generation != u64::MAX).then(|| format!("key:{generation}")),
            }
        }
        protocol::StatusReply::Error {
            exit_code, context, ..
        } => View {
            success: false,
            repo_unavailable: status_finding(reply) == "unavailable",
            finding: status_finding(reply),
            message: if *exit_code == 4 {
                "An operation requires terminal recovery."
            } else {
                "Status could not be loaded."
            },
            context: context.as_ref().map(ContextView::from),
            entries: Vec::new(),
            key_stripped,
            clean: 0,
            remote: String::new(),
            held_counts: HeldCounts::default(),
            identity_review_id: None,
            setup_review_id: None,
            policy_review_id: None,
            hook_review_id: None,
            key_review_id: None,
        },
    }
}

const fn failure(message: &'static str, key_stripped: bool) -> View {
    View {
        success: false,
        repo_unavailable: false,
        finding: "unknown",
        message,
        context: None,
        entries: Vec::new(),
        key_stripped,
        clean: 0,
        remote: String::new(),
        held_counts: HeldCounts {
            new_files: 0,
            home_edits: 0,
            repo_edits: 0,
        },
        identity_review_id: None,
        setup_review_id: None,
        policy_review_id: None,
        hook_review_id: None,
        key_review_id: None,
    }
}

#[derive(Default)]
struct Inner {
    active: Option<Arc<AtomicBool>>,
    reply: Option<protocol::StatusReply>,
    review: Option<RetainedReview>,
    generation: u64,
    review_sequence: u64,
    inspection_sequence: u64,
    recovery: Option<Vec<protocol::RecoveryStep>>,
    diff_receipts: HashSet<usize>,
    setup: Option<setup::SetupIntent>,
    key_recovery: Vec<std::path::PathBuf>,
}

struct RetainedReview {
    id: String,
    review: protocol::SyncReview,
    capture_status: Vec<Option<usize>>,
    lines: HashMap<usize, Vec<RetainedLine>>,
}

struct RetainedLine {
    id: String,
    edit: protocol::LineEdit,
}

pub struct AppState {
    inner: Arc<Mutex<Inner>>,
    key_stripped: bool,
}

impl AppState {
    pub fn new(key_stripped: bool) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            key_stripped,
        }
    }

    pub fn stop(&self) -> Result<(), &'static str> {
        let mut inner = self.inner.lock().map_err(|_| "Status state unavailable")?;
        if let Some(cancelled) = &inner.active {
            cancelled.store(true, Ordering::Release);
        }
        inner.reply = None;
        inner.review = None;
        inner.diff_receipts.clear();
        inner.setup = None;
        inner.generation = inner.generation.saturating_add(1);
        drop(inner);
        Ok(())
    }
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri extracts State by value; borrowed State does not implement CommandArg."
)]
pub fn cancel(state: tauri::State<'_, AppState>) -> Result<(), &'static str> {
    state.stop()
}

#[tauri::command]
#[expect(
    clippy::needless_pass_by_value,
    reason = "Tauri extracts State by value; borrowed State does not implement CommandArg."
)]
pub fn discard_review(id: String, state: tauri::State<'_, AppState>) -> Result<(), &'static str> {
    state
        .inner
        .lock()
        .map_err(|_| "Status state unavailable")?
        .discard_review(&id);
    Ok(())
}

#[tauri::command]
pub async fn refresh(state: tauri::State<'_, AppState>) -> Result<View, String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    {
        let Ok(mut inner) = state.inner.lock() else {
            return Ok(failure("Status state unavailable", state.key_stripped));
        };
        if inner.active.is_some() {
            return Ok(failure(
                "A status read is already running",
                state.key_stripped,
            ));
        }
        inner.reply = None;
        inner.review = None;
        inner.diff_receipts.clear();
        inner.setup = None;
        inner.active = Some(cancelled.clone());
    }
    let inner = state.inner.clone();
    let key_stripped = state.key_stripped;
    let worker = tauri::async_runtime::spawn_blocking(move || {
        let result = run_status(&cancelled);
        let Ok(mut inner) = inner.lock() else {
            return failure("Status state unavailable", key_stripped);
        };
        inner.finish(result, &cancelled, key_stripped)
    })
    .await;
    let Ok(view) = worker else {
        if let Ok(mut inner) = state.inner.lock() {
            inner.active = None;
            inner.reply = None;
        }
        return Ok(failure("Status worker failed", key_stripped));
    };
    Ok(view)
}

impl Inner {
    #[cfg(test)]
    fn consume_review(
        &mut self,
        id: &str,
        message: Option<&runner::CommitMessage>,
    ) -> Result<(protocol::SyncReview, protocol::StatusContext, String), &'static str> {
        self.consume_review_mode(id, message, None)
    }

    fn consume_review_mode(
        &mut self,
        id: &str,
        message: Option<&runner::CommitMessage>,
        selected_text: Option<String>,
    ) -> Result<(protocol::SyncReview, protocol::StatusContext, String), &'static str> {
        if self.active.is_some() {
            return Err("Another operation is running");
        }
        if self.review.as_ref().is_none_or(|review| review.id != id) {
            return Err("Review is stale or unavailable");
        }
        let review = &self.review.as_ref().ok_or("Review is unavailable")?.review;
        let Some(protocol::StatusReply::Report(status)) = &self.reply else {
            return Err("Status is unavailable");
        };
        CompleteReview::new(review, &status.context)?;
        let suggested = editable_message(review);
        if message.is_some() && suggested.is_none() {
            return Err("Commit message editing is unavailable");
        }
        let text = match selected_text {
            Some(text) => text,
            None => modal_text(review, message.or(suggested.as_ref()), message.is_some())?,
        };
        let context = status.context.clone();
        let review = self.review.take().ok_or("Review is unavailable")?.review;
        Ok((review, context, text))
    }

    fn discard_review(&mut self, id: &str) {
        if self.review.as_ref().is_some_and(|review| review.id == id) {
            self.review = None;
        }
    }

    fn finish(
        &mut self,
        result: Result<protocol::StatusReply, &'static str>,
        cancelled: &AtomicBool,
        key_stripped: bool,
    ) -> View {
        self.active = None;
        self.reply = None;
        self.diff_receipts.clear();
        self.setup = None;
        if cancelled.load(Ordering::Acquire) {
            return failure(
                if matches!(result, Err("Could not cancel status")) {
                    "Could not cancel status"
                } else {
                    "Status read cancelled"
                },
                key_stripped,
            );
        }
        match result {
            Ok(reply) => {
                if matches!(&reply, protocol::StatusReply::Report(_)) {
                    self.recovery = None;
                }
                self.generation = self.generation.saturating_add(1);
                let mut view = project(&reply, key_stripped, self.generation);
                if self.recovery.is_none()
                    && !matches!(
                        &reply,
                        protocol::StatusReply::Error {
                            recovery: Some(_),
                            ..
                        }
                    )
                    && self.generation != u64::MAX
                    && let Ok(intent) = setup::SetupIntent::from_status(&reply)
                    && intent.encode().is_ok()
                {
                    view.setup_review_id = Some(format!("setup:{}", self.generation));
                    self.setup = Some(intent);
                }
                self.reply = Some(reply);
                view
            }
            Err(message) => failure(message, key_stripped),
        }
    }
}

#[derive(Serialize)]
pub struct DiffView {
    success: bool,
    outcome: DiffOutcome,
    message: String,
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_actions: Option<Vec<ApplyAction>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line_edits: Option<Vec<LineView>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_push: Option<bool>,
}

#[derive(Serialize)]
struct LineView {
    id: String,
    old_line: Option<u32>,
    new_line: Option<u32>,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum DiffOutcome {
    Ready,
    Error,
    TerminalRecovery,
}

fn diff_failure(message: &str) -> DiffView {
    DiffView {
        success: false,
        outcome: DiffOutcome::Error,
        message: message.into(),
        text: None,
        file_actions: None,
        line_edits: None,
        selected_push: None,
    }
}

fn start_if_approved<T>(
    cancelled: &AtomicBool,
    approved: bool,
    start: impl FnOnce() -> Result<T, &'static str>,
) -> Result<T, &'static str> {
    if cancelled.load(Ordering::Acquire) {
        return Err("Diff cancelled");
    }
    if !approved {
        return Err("Encrypted diff declined");
    }
    start()
}

fn select(inner: &Inner, id: &str) -> Result<protocol::DiffSelection, &'static str> {
    if let Some(capture_id) = id.strip_prefix("capture:") {
        let retained = inner.review.as_ref().ok_or("Review is unavailable")?;
        let index_text = capture_id
            .strip_prefix(&format!("{}:", retained.id))
            .ok_or("Review selection is stale")?;
        let index: usize = index_text.parse().map_err(|_| "Invalid selection")?;
        if index.to_string() != index_text {
            return Err("Invalid selection");
        }
        let Some(protocol::StatusReply::Report(report)) = &inner.reply else {
            return Err("Status is unavailable");
        };
        let paths = HeldPaths::new(report);
        CompleteReview::new(&retained.review, &report.context)?;
        let Some(protocol::SyncRecord::Capture(capture)) = retained.review.records.get(index)
        else {
            return Err("Invalid selection");
        };
        let status_index = retained
            .capture_status
            .get(index)
            .copied()
            .flatten()
            .ok_or("Diff is unavailable for this capture")?;
        if report
            .entries
            .iter()
            .filter(|entry| entry.scope == "home" && entry.key.raw == capture.key.raw)
            .take(2)
            .count()
            != 1
        {
            return Err("Diff is unavailable for this capture");
        }
        let entry = report
            .entries
            .get(status_index)
            .ok_or("Invalid selection")?;
        if !supported(entry)
            || paths.matches(entry, false)
            || paths.repo.contains(capture.source.path.as_path())
            || !protocol::capture_matches_status(capture, entry)
        {
            return Err("Diff is unavailable for this capture");
        }
        return Ok(protocol::DiffSelection {
            context: report.context.clone(),
            entry: entry.clone(),
        });
    }
    let (generation, index_text) = id.split_once(':').ok_or("Invalid selection")?;
    if generation != inner.generation.to_string() {
        return Err("Status selection is stale");
    }
    let index: usize = index_text.parse().map_err(|_| "Invalid selection")?;
    if index.to_string() != index_text {
        return Err("Invalid selection");
    }
    let Some(protocol::StatusReply::Report(report)) = &inner.reply else {
        return Err("Status is unavailable");
    };
    let entry = report.entries.get(index).ok_or("Invalid selection")?;
    if !supported(entry) || HeldPaths::new(report).matches(entry, false) {
        return Err("Diff is unavailable for this occurrence");
    }
    Ok(protocol::DiffSelection {
        context: report.context.clone(),
        entry: entry.clone(),
    })
}

fn capture_index(inner: &Inner, id: &str) -> Option<usize> {
    let review = inner.review.as_ref()?;
    let suffix = id
        .strip_prefix("capture:")?
        .strip_prefix(&format!("{}:", review.id))?;
    let index: usize = suffix.parse().ok()?;
    (index.to_string() == suffix).then_some(index)
}

fn take_capture_lines(inner: &mut Inner, id: &str) -> Option<Vec<RetainedLine>> {
    if let Some(index) = capture_index(inner, id)
        && let Some(review) = &mut inner.review
    {
        return review.lines.remove(&index);
    }
    None
}

fn retain_lines(
    inner: &mut Inner,
    id: &str,
    view: &protocol::DiffView,
    previous: Option<Vec<RetainedLine>>,
) -> Option<(Vec<LineView>, bool)> {
    let index = capture_index(inner, id)?;
    select(inner, id).ok()?;
    let Some(protocol::StatusReply::Report(status)) = &inner.reply else {
        return None;
    };
    let review = inner.review.as_ref()?;
    let binding = CompleteReview::new(&review.review, &status.context)
        .ok()?
        .selected_target()
        .ok()?;
    let selected_push = binding != "local";
    let protocol::StatusReply::Report(status) = inner.reply.as_ref()? else {
        return None;
    };
    CompleteReview::new(&inner.review.as_ref()?.review, &status.context)
        .ok()?
        .selected_target()
        .ok()?;
    if view.status != "different" || view.system_off {
        return None;
    }
    let edits = view.line_edits.as_ref()?;
    if edits.is_empty() || edits.len() > 4096 {
        return None;
    }
    let unchanged = previous
        .as_ref()
        .is_some_and(|lines| lines.iter().map(|line| &line.edit).eq(edits.iter()));
    let sequence = if unchanged {
        inner.inspection_sequence
    } else {
        inner.inspection_sequence.checked_add(1)?
    };
    inner.inspection_sequence = sequence;
    let review = inner.review.as_mut()?;
    let retained = if unchanged {
        previous?
    } else {
        edits
            .iter()
            .enumerate()
            .map(|(ordinal, edit)| RetainedLine {
                id: format!("line:{}:{sequence}:{ordinal}", review.id),
                edit: edit.clone(),
            })
            .collect()
    };
    let displayed = retained
        .iter()
        .map(|line| {
            let edit = &line.edit;
            LineView {
                id: line.id.clone(),
                old_line: edit.old_line,
                new_line: edit.new_line,
                before: edit
                    .before
                    .as_ref()
                    .map(|text| protocol::escape_diff(text.as_bytes())),
                after: edit
                    .after
                    .as_ref()
                    .map(|text| protocol::escape_diff(text.as_bytes())),
            }
        })
        .collect();
    review.lines.insert(index, retained);
    Some((displayed, selected_push))
}

fn status_index(inner: &Inner, id: &str) -> Option<usize> {
    let (generation, index) = id.split_once(':')?;
    if inner.generation == u64::MAX || generation != inner.generation.to_string() {
        return None;
    }
    let parsed: usize = index.parse().ok()?;
    (parsed.to_string() == index).then_some(parsed)
}

fn file_actions(inner: &Inner, id: &str) -> Option<(usize, Vec<ApplyAction>)> {
    let index = status_index(inner, id)?;
    let protocol::StatusReply::Report(report) = inner.reply.as_ref()? else {
        return None;
    };
    let entry = report.entries.get(index)?;
    if !supported(entry)
        || report.summary.held_in_repo != 0
        || report
            .entries
            .iter()
            .any(|other| other.status == "held-in-repo")
        || report.entries.iter().any(|other| {
            other.scope == "home"
                && other.key.raw == entry.key.raw
                && matches!(other.status.as_str(), "held" | "held-in-home")
        })
        || report
            .entries
            .iter()
            .filter(|other| {
                other.scope == entry.scope
                    && other.key.raw == entry.key.raw
                    && other.row.is_some()
                    && !matches!(other.status.as_str(), "held" | "held-in-home")
            })
            .take(2)
            .count()
            != 1
    {
        return None;
    }
    let actions: Vec<_> = [
        ApplyAction::KeepLocal,
        ApplyAction::TakeRepo,
        ApplyAction::Restore,
    ]
    .into_iter()
    .filter(|action| ReviewedApply::new(&report.context, vec![(entry, *action)]).is_ok())
    .collect();
    (!actions.is_empty()).then_some((index, actions))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileAnswer {
    id: String,
    action: FileAction,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FileAction {
    KeepLocal,
    TakeRepo,
    Restore,
}

impl FileAction {
    const fn apply(self) -> ApplyAction {
        match self {
            Self::KeepLocal => ApplyAction::KeepLocal,
            Self::TakeRepo => ApplyAction::TakeRepo,
            Self::Restore => ApplyAction::Restore,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::KeepLocal => "Keep local",
            Self::TakeRepo => "Take repository",
            Self::Restore => "Restore",
        }
    }
}

struct ApplyReservation {
    context: protocol::StatusContext,
    choices: Vec<(protocol::StatusEntry, FileAction)>,
    text: String,
}

impl ApplyReservation {
    fn reviewed(&self) -> Result<ReviewedApply<'_>, &'static str> {
        ReviewedApply::new(
            &self.context,
            self.choices
                .iter()
                .map(|(entry, action)| (entry, action.apply()))
                .collect(),
        )
    }
}

fn apply_modal(
    context: &protocol::StatusContext,
    choices: &[(protocol::StatusEntry, FileAction)],
) -> Result<String, &'static str> {
    const LIMIT: usize = 32 * 1024;
    fn quoted(text: &mut String, value: &str) -> Result<(), &'static str> {
        text.push('"');
        for c in value.chars() {
            if runner::unsafe_display_char(c) {
                text.extend(c.escape_unicode());
            } else if matches!(c, '"' | '\\') {
                text.push('\\');
                text.push(c);
            } else {
                text.push(c);
            }
            if text.len() > LIMIT {
                return Err("File review is too large for confirmation");
            }
        }
        text.push('"');
        Ok(())
    }
    let mut text = String::from("Repository: ");
    quoted(&mut text, &context.repo.shown)?;
    text.push_str("\nHome: ");
    quoted(&mut text, &context.home.shown)?;
    for (entry, action) in choices {
        text.push('\n');
        text.push_str(action.label());
        text.push_str(": ");
        quoted(&mut text, &entry.key.shown)?;
    }
    if choices
        .iter()
        .any(|(_, action)| matches!(action, FileAction::KeepLocal))
    {
        text.push_str("\n\nKeep local leaves the home file and records the repository version.");
    }
    if choices
        .iter()
        .any(|(_, action)| matches!(action, FileAction::TakeRepo))
    {
        text.push_str("\n\nTake repository backs up replaced bytes before applying the repository version to the home file.");
    }
    if choices
        .iter()
        .any(|(_, action)| matches!(action, FileAction::Restore))
    {
        text.push_str(
            "\n\nRestore applies the repository version only when the home file is missing.",
        );
    }
    text.push_str("\n\nNo hooks or system files; no commit or push.");
    if text.len() > LIMIT {
        return Err("File review is too large for confirmation");
    }
    Ok(text)
}

fn reserve_apply(
    inner: &mut Inner,
    answers: &[FileAnswer],
    cancelled: Arc<AtomicBool>,
) -> Result<ApplyReservation, &'static str> {
    if inner.active.is_some() {
        return Err("Another operation is running");
    }
    if answers.is_empty() || answers.len() > 256 {
        return Err("Invalid file decision batch");
    }
    let Some(protocol::StatusReply::Report(report)) = &inner.reply else {
        return Err("Status is unavailable");
    };
    let mut choices = Vec::with_capacity(answers.len());
    let mut indices = HashSet::new();
    for answer in answers {
        let (index, _) = file_actions(inner, &answer.id).ok_or("File decision is unavailable")?;
        if !inner.diff_receipts.contains(&index) || !indices.insert(index) {
            return Err("File decision is unreviewed or duplicated");
        }
        let entry = report.entries.get(index).ok_or("Invalid selection")?;
        ReviewedApply::new(&report.context, vec![(entry, answer.action.apply())])?;
        choices.push((entry.clone(), answer.action));
    }
    let context = report.context.clone();
    let text = apply_modal(&context, &choices)?;
    let reservation = ApplyReservation {
        context,
        choices,
        text,
    };
    reservation.reviewed()?.encode()?;
    for index in indices {
        inner.diff_receipts.remove(&index);
    }
    inner.review = None;
    inner.active = Some(cancelled);
    Ok(reservation)
}

fn identity_modal(review: &ReviewedIdentity<'_>) -> Result<String, &'static str> {
    const LIMIT: usize = 32 * 1024;
    fn add(text: &mut String, label: &str, value: &str) -> Result<(), &'static str> {
        text.push_str(label);
        text.push_str(": \"");
        for c in value.chars() {
            if runner::unsafe_display_char(c) {
                text.extend(c.escape_unicode());
            } else if matches!(c, '"' | '\\') {
                text.push('\\');
                text.push(c);
            } else {
                text.push(c);
            }
            if text.len() > LIMIT {
                return Err("Identity review is too large for confirmation");
            }
        }
        text.push_str("\"\n");
        Ok(())
    }
    fn machine(
        text: &mut String,
        name: &str,
        identity: &protocol::StatusIdentity,
    ) -> Result<(), &'static str> {
        text.push_str(name);
        text.push('\n');
        for (label, value) in [
            ("User", &identity.user.raw),
            ("Host", &identity.host.raw),
            ("OS", &identity.os.raw),
            ("Distribution", &identity.distro.raw),
            ("Distribution family", &identity.like.raw),
            ("Architecture", &identity.arch.raw),
        ] {
            add(text, label, value)?;
        }
        if identity.profiles.is_empty() {
            text.push_str("Profiles: []\n");
        } else {
            for profile in &identity.profiles {
                add(text, "Profile", &profile.raw)?;
            }
        }
        Ok(())
    }
    let mut text = String::new();
    add(&mut text, "Repository", &review.context().repo.shown)?;
    add(&mut text, "Home", &review.context().home.shown)?;
    machine(&mut text, "\nPrevious identity", review.recorded())?;
    machine(&mut text, "\nCurrent identity", review.current())?;
    text.push_str("\nOnly the recorded identity is approved for change. Settings, home and repository files, hooks, system files and remote actions are not approved. Pending file choices require separate review.\n\nContinue?");
    if text.len() > LIMIT {
        return Err("Identity review is too large for confirmation");
    }
    Ok(text)
}

fn reserve_identity(
    inner: &mut Inner,
    id: &str,
    cancelled: Arc<AtomicBool>,
) -> Result<(protocol::StatusReport, String), &'static str> {
    if inner.active.is_some() {
        return Err("Another operation is running");
    }
    if inner.generation == u64::MAX || id != format!("identity:{}", inner.generation) {
        return Err("Identity review is stale");
    }
    let Some(protocol::StatusReply::Report(report)) = &inner.reply else {
        return Err("Status is unavailable");
    };
    let review = ReviewedIdentity::new(report)?;
    let text = identity_modal(&review)?;
    review.encode()?;
    let Some(protocol::StatusReply::Report(report)) = inner.reply.take() else {
        return Err("Status is unavailable");
    };
    inner.review = None;
    inner.diff_receipts.clear();
    inner.recovery = None;
    inner.generation = inner.generation.saturating_add(1);
    inner.active = Some(cancelled);
    Ok((report, text))
}

fn setup_modal(intent: &setup::SetupIntent) -> Result<String, &'static str> {
    const LIMIT: usize = 32 * 1024;
    let path = intent.context().repo.path.as_os_str().as_bytes();
    let mut text = if intent.source().is_some() {
        String::from("Clone a repository to:\n")
    } else {
        String::from("Create a new repository at:\n")
    };
    if let Ok(value) = std::str::from_utf8(path) {
        text.push('"');
        for c in value.chars() {
            if runner::unsafe_display_char(c) {
                text.extend(c.escape_unicode());
            } else if matches!(c, '"' | '\\') {
                text.push('\\');
                text.push(c);
            } else {
                text.push(c);
            }
            if text.len() > LIMIT {
                return Err("Setup path is too large for confirmation");
            }
        }
        text.push('"');
    } else {
        text.push_str("Unix path bytes (hex): ");
        for byte in path {
            use std::fmt::Write as _;
            write!(&mut text, "{byte:02x}").map_err(|_| "Cannot render setup path")?;
            if text.len() > LIMIT {
                return Err("Setup path is too large for confirmation");
            }
        }
    }
    if let Some(source) = intent.source() {
        text.push_str("\nSource: \"");
        text.push_str(source);
        text.push('"');
        text.push_str("\n\nThe destination must be absent. No existing repository is overwritten or adopted. No home files are applied; no hooks, keys or imports are run. Review files separately after cloning.\n\nContinue?");
    } else {
        text.push_str("\n\nThe destination must be absent. No existing repository is overwritten or adopted. No home files are applied; no hooks, keys, imports or remote actions are run.\n\nContinue?");
    }
    if text.len() > LIMIT {
        return Err("Setup path is too large for confirmation");
    }
    Ok(text)
}

fn reserve_setup(
    inner: &mut Inner,
    id: &str,
    source: Option<setup::CloneSource>,
    cancelled: Arc<AtomicBool>,
) -> Result<(setup::SetupIntent, String), String> {
    if inner.active.is_some() || inner.recovery.is_some() {
        return Err("Another operation or terminal recovery is pending".into());
    }
    if inner.generation == u64::MAX || id != format!("setup:{}", inner.generation) {
        return Err("Setup review is stale".into());
    }
    let intent = inner.setup.take().ok_or("Setup review is unavailable")?;
    let intent = match source {
        Some(source) => intent.for_clone(source),
        None => intent,
    };
    inner.reply = None;
    inner.review = None;
    inner.diff_receipts.clear();
    inner.generation = inner.generation.saturating_add(1);
    let text = setup_modal(&intent)?;
    intent.encode()?;
    inner.active = Some(cancelled);
    Ok((intent, text))
}

#[tauri::command]
pub async fn open_diff(
    id: String,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<DiffView, String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let selection = {
        let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
        if inner.active.is_some() {
            take_capture_lines(&mut inner, &id);
            return Ok(diff_failure("Another read is running"));
        }
        if let Some(index) = status_index(&inner, &id) {
            inner.diff_receipts.remove(&index);
        }
        let selection = match select(&inner, &id) {
            Ok(value) => value,
            Err(message) => {
                take_capture_lines(&mut inner, &id);
                return Ok(diff_failure(message));
            }
        };
        inner.active = Some(cancelled.clone());
        selection
    };
    let inner = state.inner.clone();
    let worker_cancelled = Arc::clone(&cancelled);
    let worker = tauri::async_runtime::spawn_blocking(move || {
        let approved = if selection.entry.encrypted {
            let source = selection
                .entry
                .source
                .as_ref()
                .map_or("", |s| s.shown.as_str());
            crate::confirm::ask(
                &window,
                "Show encrypted diff?",
                "Decrypt and show diff",
                format!(
                    "Key: {}\nSource: {}\n\nDecrypt and show this comparison?",
                    selection.entry.key.shown, source
                ),
                worker_cancelled.clone(),
            )
        } else {
            Ok(true)
        };
        let mut run = {
            let guard = inner.lock().map_err(|_| "Status state unavailable")?;
            // Holding the state lock makes cancellation and child creation mutually exclusive.
            let run = start_if_approved(&worker_cancelled, approved?, || {
                runner::start_diff(&selection, selection.entry.encrypted)
                    .map_err(|_| "Could not start exact diff")
            })?;
            drop(guard);
            run
        };
        let start = Instant::now();
        loop {
            if worker_cancelled.load(Ordering::Acquire) {
                run.cancel().map_err(|_| "Could not cancel diff")?;
                return Err("Diff cancelled");
            }
            if start.elapsed() >= Duration::from_secs(30) {
                run.cancel()
                    .map_err(|_| "Could not cancel timed-out diff")?;
                return Err("Diff timed out");
            }
            if let Some(receipt) = run.poll().map_err(|_| "Diff process failed")? {
                return protocol::parse_diff(&receipt.stdout, receipt.status.code(), &selection)
                    .map_err(|_| "Diff changed or returned an invalid response");
            }
            run.wait_readable().map_err(|_| "Diff process failed")?;
        }
    })
    .await;
    let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
    inner.active = None;
    if cancelled.load(Ordering::Acquire) {
        drop(inner);
        return Ok(diff_failure("Diff cancelled"));
    }
    let view = finish_diff(&mut inner, &id, worker.unwrap_or(Err("Diff worker failed")));
    drop(inner);
    Ok(view)
}

fn finish_diff(
    inner: &mut Inner,
    id: &str,
    worker: Result<protocol::DiffReply, &'static str>,
) -> DiffView {
    if let Some(index) = status_index(inner, id) {
        inner.diff_receipts.remove(&index);
    }
    let previous = take_capture_lines(inner, id);
    match worker {
        Ok(protocol::DiffReply::Report(view)) => {
            let lines = retain_lines(inner, id, &view, previous);
            let selected_push = lines.as_ref().map(|(_, push)| *push);
            let line_edits = lines.map(|(edits, _)| edits);
            let actions = if view.status == "different" && !view.system_off {
                file_actions(inner, id)
            } else {
                None
            };
            if let Some((index, _)) = &actions {
                inner.diff_receipts.insert(*index);
            }
            DiffView {
                success: true,
                outcome: DiffOutcome::Ready,
                message: if view.system_off {
                    format!("{} · system scope disabled", view.status)
                } else {
                    view.status
                },
                text: view.text,
                file_actions: actions.map(|(_, actions)| actions),
                line_edits,
                selected_push,
            }
        }
        Ok(protocol::DiffReply::Error {
            exit_code,
            context,
            error_kind,
            recovery,
        }) => {
            if exit_code == 4 {
                inner.review = None;
                inner.diff_receipts.clear();
                inner.reply = Some(protocol::StatusReply::Error {
                    exit_code,
                    context: context.map(|value| *value),
                    error_kind,
                    recovery,
                });
                inner.generation = inner.generation.saturating_add(1);
                DiffView {
                    success: false,
                    outcome: DiffOutcome::TerminalRecovery,
                    message: "An operation requires terminal recovery.".into(),
                    text: None,
                    file_actions: None,
                    line_edits: None,
                    selected_push: None,
                }
            } else {
                diff_failure("Diff could not be loaded.")
            }
        }
        Err(message) => diff_failure(message),
    }
}

#[derive(Serialize)]
struct ReviewItem {
    group: &'static str,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff_id: Option<String>,
}

#[derive(Serialize)]
pub struct ReviewView {
    outcome: &'static str,
    message: &'static str,
    items: Vec<ReviewItem>,
    id: Option<String>,
    commit_message: Option<String>,
}

fn editable_message(review: &protocol::SyncReview) -> Option<runner::CommitMessage> {
    let mut messages = review.records.iter().filter_map(|record| match record {
        protocol::SyncRecord::Message { message } => Some(&message.raw),
        _ => None,
    });
    let message = messages.next()?;
    if messages.next().is_some() {
        return None;
    }
    runner::CommitMessage::new(message.clone()).ok()
}

fn capture_status_index(
    review: &protocol::SyncReview,
    status: &protocol::StatusReport,
) -> Vec<Option<usize>> {
    let mut entries = HashMap::new();
    let paths = HeldPaths::new(status);
    for (index, entry) in status.entries.iter().enumerate() {
        if entry.scope != "home" {
            continue;
        }
        entries
            .entry(entry.key.raw.as_str())
            .and_modify(|value| *value = None)
            .or_insert(Some(index));
    }
    review
        .records
        .iter()
        .map(|record| {
            let protocol::SyncRecord::Capture(capture) = record else {
                return None;
            };
            let status_index = entries.get(capture.key.raw.as_str()).copied().flatten()?;
            let entry = status.entries.get(status_index)?;
            (supported(entry)
                && !paths.matches(entry, false)
                && !paths.repo.contains(capture.source.path.as_path())
                && protocol::capture_matches_status(capture, entry))
            .then_some(status_index)
        })
        .collect()
}

fn review_key<'a>(key: &'a protocol::StatusText, scope: &str, paths: &HeldPaths<'_>) -> &'a str {
    if paths.redacts_key(scope, &key.raw) {
        "[redacted filename]"
    } else {
        &key.shown
    }
}

fn capture_text(capture: &protocol::SyncCapture, paths: &HeldPaths<'_>) -> String {
    if paths.redacts_key(&capture.scope, &capture.key.raw)
        || paths.redacted_repo.contains(capture.source.path.as_path())
    {
        "[redacted filename]".into()
    } else if capture.key.raw.as_bytes() == capture.source.path.as_os_str().as_bytes() {
        format!("{} · {}", capture.key.shown, capture.scope)
    } else {
        format!(
            "{} · {} · {}",
            capture.key.shown, capture.source.shown, capture.scope
        )
    }
}

fn refusal_key<'a>(
    key: Option<&'a protocol::StatusText>,
    source: Option<&protocol::StatusPath>,
    paths: &HeldPaths<'_>,
) -> &'a str {
    if source.is_some_and(|path| paths.redacted_repo.contains(path.path.as_path())) {
        "[redacted filename]"
    } else {
        key.map_or("Context", |key| {
            if paths.redacts_key("repo_source", &key.raw) {
                "[redacted filename]"
            } else {
                review_key(key, "home", paths)
            }
        })
    }
}

fn project_review(
    review: &protocol::SyncReview,
    links: Option<(&str, &[Option<usize>])>,
    paths: &HeldPaths<'_>,
) -> ReviewView {
    let mut items = Vec::new();
    for (index, record) in review.records.iter().enumerate() {
        let (group, text) = match record {
            protocol::SyncRecord::Capture(capture) => ("Captures", capture_text(capture, paths)),
            protocol::SyncRecord::RepoEdit(edit)
                if paths.redacted_repo.contains(edit.path.path.as_path()) =>
            {
                ("Repository changes", "[redacted filename]".into())
            }
            protocol::SyncRecord::RepoEdit(edit) => ("Repository changes", edit.path.shown.clone()),
            protocol::SyncRecord::Own(edit)
                if paths.redacted_repo.contains(edit.path.path.as_path()) =>
            {
                ("Repository changes", "[redacted filename]".into())
            }
            protocol::SyncRecord::Own(edit) => (
                "Repository changes",
                format!("{} · dotman file", edit.path.shown),
            ),
            protocol::SyncRecord::AheadCommit { subject, .. } => {
                ("Already-ahead commits", subject.shown.clone())
            }
            protocol::SyncRecord::Message { message } => {
                ("Suggested message", message.shown.clone())
            }
            protocol::SyncRecord::Undecided {
                key, reason, scope, ..
            } => (
                "Held or undecided",
                format!(
                    "{} · {}",
                    key.as_ref()
                        .map_or("Context", |key| review_key(key, scope, paths)),
                    safe_token(reason)
                ),
            ),
            protocol::SyncRecord::Refusal { key, kind, source } => (
                "Held or undecided",
                format!(
                    "{} · {}",
                    refusal_key(key.as_ref(), source.as_ref(), paths),
                    safe_token(kind)
                ),
            ),
            protocol::SyncRecord::Hooks {
                commands,
                pending_commands,
                pending_keys,
                approved,
                unshown,
                ..
            } => {
                append_hooks(
                    &mut items,
                    commands,
                    pending_commands,
                    pending_keys,
                    *approved,
                    *unshown,
                    paths,
                );
                continue;
            }
            protocol::SyncRecord::Resolved {
                key, effect, scope, ..
            } => (
                "Hypothetical effects",
                format!("{} · {}", review_key(key, scope, paths), safe_token(effect)),
            ),
            protocol::SyncRecord::Stage { stage, .. } if stage == "blocked_by_repo_hold" => (
                "Held or undecided",
                "Repository hold stopped planning before home capture".into(),
            ),
            protocol::SyncRecord::Hook {
                command: Some(command),
                outcome,
                ..
            } => (
                "Hook outcomes",
                format!("{} · {}", command.shown, safe_token(outcome)),
            ),
            _ => continue,
        };
        let diff_id = links.and_then(|(id, mapping)| {
            mapping
                .get(index)
                .is_some_and(Option::is_some)
                .then(|| format!("capture:{id}:{index}"))
        });
        items.push(ReviewItem {
            group,
            text,
            diff_id,
        });
    }
    review_view(review, items)
}

const fn review_view(review: &protocol::SyncReview, items: Vec<ReviewItem>) -> ReviewView {
    ReviewView {
        outcome: if review.exit_code == 0 {
            "review"
        } else {
            "partial"
        },
        message: if review.exit_code == 0 {
            "Outgoing preview loaded."
        } else {
            "Attention or incomplete planning: this preview may not cover all outgoing changes."
        },
        items,
        id: None,
        commit_message: None,
    }
}

fn append_hooks(
    items: &mut Vec<ReviewItem>,
    commands: &[protocol::StatusText],
    pending_commands: &[protocol::StatusText],
    pending_keys: &[protocol::StatusText],
    approved: bool,
    unshown: bool,
    paths: &HeldPaths<'_>,
) {
    items.push(ReviewItem {
        group: "Hook standing",
        text: if unshown {
            "Hook table not shown for approval".into()
        } else if approved {
            "Current hook table approved".into()
        } else {
            "Current hook table not approved".into()
        },
        diff_id: None,
    });
    for (group, values) in [
        ("Hooks due", commands),
        ("Pending hooks", pending_commands),
        ("Pending hook keys", pending_keys),
    ] {
        for value in values {
            items.push(ReviewItem {
                group,
                text: if group == "Pending hook keys" {
                    review_key(value, "home", paths).to_owned()
                } else {
                    value.shown.clone()
                },
                diff_id: None,
            });
        }
    }
}

fn safe_token(value: &str) -> &str {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        value
    } else {
        "unknown"
    }
}

#[tauri::command]
pub async fn open_review(state: tauri::State<'_, AppState>) -> Result<ReviewView, String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let context = {
        let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
        if inner.active.is_some() {
            return Err("Another read is running".into());
        }
        let Some(protocol::StatusReply::Report(report)) = &inner.reply else {
            return Err("Status is unavailable".into());
        };
        let context = report.context.clone();
        inner.review = None;
        inner.active = Some(cancelled.clone());
        context
    };
    let worker_cancelled = cancelled.clone();
    let shared_inner = state.inner.clone();
    let worker = tauri::async_runtime::spawn_blocking(move || {
        let mut run = {
            let guard = shared_inner
                .lock()
                .map_err(|_| "Status state unavailable")?;
            let run = start_if_approved(&worker_cancelled, true, || {
                runner::start_review(&context).map_err(|_| "Could not start sync preview")
            })?;
            drop(guard);
            run
        };
        let start = Instant::now();
        loop {
            if worker_cancelled.load(Ordering::Acquire) {
                run.cancel().map_err(|_| "Could not cancel sync preview")?;
                return Err("Sync preview cancelled");
            }
            if start.elapsed() >= Duration::from_secs(30) {
                run.cancel()
                    .map_err(|_| "Could not cancel timed-out sync preview")?;
                return Err("Sync preview timed out");
            }
            if let Some(receipt) = run.poll().map_err(|_| "Sync preview process failed")? {
                return protocol::parse_sync(&receipt.stdout, receipt.status.code(), &context)
                    .map_err(|_| "Invalid sync preview");
            }
            run.wait_readable()
                .map_err(|_| "Sync preview process failed")?;
        }
    })
    .await;
    let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
    inner.active = None;
    if cancelled.load(Ordering::Acquire) {
        drop(inner);
        return Err("Sync preview cancelled".into());
    }
    let response = finish_review(
        &mut inner,
        worker.unwrap_or(Err("Sync preview worker failed")),
    );
    drop(inner);
    response
}

fn finish_review(
    inner: &mut Inner,
    result: Result<protocol::SyncReply, &'static str>,
) -> Result<ReviewView, String> {
    inner.review = None;
    match result {
        Ok(protocol::SyncReply::Review(review)) => {
            let paths = match &inner.reply {
                Some(protocol::StatusReply::Report(status)) => HeldPaths::new(status),
                _ => HeldPaths::default(),
            };
            if let Some(protocol::StatusReply::Report(status)) = &inner.reply
                && CompleteReview::new(&review, &status.context).is_ok()
            {
                let sequence = inner
                    .review_sequence
                    .checked_add(1)
                    .ok_or("Review identifiers exhausted")?;
                inner.review_sequence = sequence;
                let id = format!("{}:{sequence}", inner.generation);
                let capture_status = capture_status_index(&review, status);
                let mut view = project_review(&review, Some((&id, &capture_status)), &paths);
                if review.error_kind.as_deref() == Some("mass_delete") {
                    view.outcome = "mass_delete";
                    view.message = "Mass deletion requires separate explicit approval.";
                }
                view.id = Some(id.clone());
                view.commit_message =
                    editable_message(&review).map(|message| message.as_str().to_owned());
                inner.review = Some(RetainedReview {
                    id,
                    review: *review,
                    capture_status,
                    lines: HashMap::new(),
                });
                return Ok(view);
            }
            Ok(project_review(&review, None, &paths))
        }
        Ok(protocol::SyncReply::Error {
            exit_code,
            context,
            error_kind,
            recovery,
        }) => {
            if exit_code == 4 {
                inner.diff_receipts.clear();
                inner.reply = Some(protocol::StatusReply::Error {
                    exit_code,
                    context: context.map(|value| *value),
                    error_kind,
                    recovery,
                });
                inner.generation = inner.generation.saturating_add(1);
                Ok(ReviewView {
                    outcome: "terminal_recovery",
                    message: "An operation requires terminal recovery.",
                    items: Vec::new(),
                    id: None,
                    commit_message: None,
                })
            } else {
                Ok(ReviewView {
                    outcome: "error",
                    message: "Sync preview could not be loaded.",
                    items: Vec::new(),
                    id: None,
                    commit_message: None,
                })
            }
        }
        Err(message) => Err(message.into()),
    }
}

#[derive(Serialize)]
pub struct WriterView {
    outcome: &'static str,
    message: &'static str,
    items: Vec<ReviewItem>,
    refresh: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery: Option<Vec<recovery::RecoveryRow>>,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WriterStage {
    Started,
    Capture,
    Commit,
    Pull,
    Apply,
    Reencrypt,
    Hooks,
    Push,
    BlockedByRepoHold,
}

impl From<writer_outcome::ProgressStage> for WriterStage {
    fn from(stage: writer_outcome::ProgressStage) -> Self {
        match stage {
            writer_outcome::ProgressStage::Capture => Self::Capture,
            writer_outcome::ProgressStage::Commit => Self::Commit,
            writer_outcome::ProgressStage::Pull => Self::Pull,
            writer_outcome::ProgressStage::Apply => Self::Apply,
            writer_outcome::ProgressStage::Reencrypt => Self::Reencrypt,
            writer_outcome::ProgressStage::Hooks => Self::Hooks,
            writer_outcome::ProgressStage::Push => Self::Push,
            writer_outcome::ProgressStage::BlockedByRepoHold => Self::BlockedByRepoHold,
        }
    }
}

const fn writer_view(outcome: &'static str, message: &'static str, refresh: bool) -> WriterView {
    WriterView {
        outcome,
        message,
        items: Vec::new(),
        refresh,
        recovery: None,
    }
}

fn modal_text(
    review: &protocol::SyncReview,
    message: Option<&runner::CommitMessage>,
    edited: bool,
) -> Result<String, &'static str> {
    const LIMIT: usize = 32 * 1024;
    fn add(text: &mut String, label: &str, value: &str) -> Result<(), &'static str> {
        text.push_str(label);
        for c in value.chars() {
            if runner::unsafe_display_char(c) {
                text.extend(c.escape_unicode());
            } else {
                text.push(c);
            }
            if text.len() > LIMIT {
                return Err("Review is too large for confirmation");
            }
        }
        text.push('\n');
        Ok(())
    }
    let mut text = String::from("Repository: ");
    add(&mut text, "", &review.context.repo.shown)?;
    if let Some(message) = message {
        text.push_str(if edited {
            "Edited commit message (each line follows |):\n"
        } else {
            "Suggested commit message (CLI may recompute it; each line follows |):\n"
        });
        for line in message.as_str().split('\n') {
            add(&mut text, "| ", line)?;
        }
        text.push_str("End of commit message\n");
    }
    for record in &review.records {
        match record {
            protocol::SyncRecord::Capture(capture) => {
                add(&mut text, "Capture: ", &capture.key.shown)?;
            }
            protocol::SyncRecord::RepoEdit(edit) | protocol::SyncRecord::Own(edit) => {
                add(&mut text, "Repository change: ", &edit.path.shown)?;
            }
            protocol::SyncRecord::AheadCommit { subject, paths, .. } => {
                add(&mut text, "Already-ahead commit: ", &subject.shown)?;
                for path in paths {
                    add(&mut text, "  Path: ", &path.shown)?;
                }
            }
            _ => {}
        }
    }
    text.push_str("\nSync may commit, pull and push using the locally configured origin. The preview cannot predict incoming changes or verify the origin URL. Hooks are disabled and no system files will be written.\n\nContinue?");
    if text.len() > LIMIT {
        return Err("Review is too large for confirmation");
    }
    Ok(text)
}

fn mass_delete_modal(
    mut text: String,
    review: &CompleteReview<'_>,
) -> Result<String, &'static str> {
    text.push_str("\n\nApprove these exact repository deletions:\n");
    for path in review.mass_delete_paths().ok_or("Missing deletion set")? {
        text.push_str("  ");
        for byte in path.path.as_os_str().as_bytes() {
            for escaped in std::ascii::escape_default(*byte) {
                text.push(char::from(escaped));
            }
        }
        text.push('\n');
        if text.len() > 32 * 1024 {
            return Err("Review is too large for confirmation");
        }
    }
    text.push_str("\nApprove all listed deletions?");
    if text.len() > 32 * 1024 {
        return Err("Review is too large for confirmation");
    }
    Ok(text)
}

struct SelectedReview {
    lines: BTreeMap<String, Vec<u32>>,
    text: String,
    target: writer_outcome::SelectedTarget,
}

#[expect(
    clippy::too_many_lines,
    reason = "Selection validation and its native confirmation stay together."
)]
fn selected_review(
    inner: &Inner,
    id: &str,
    tokens: &[String],
    message: Option<&runner::CommitMessage>,
) -> Result<SelectedReview, &'static str> {
    if inner.active.is_some() || tokens.is_empty() || tokens.len() > 4096 {
        return Err("Invalid selected lines");
    }
    let review = inner
        .review
        .as_ref()
        .filter(|r| r.id == id)
        .ok_or("Review is stale or unavailable")?;
    let Some(protocol::StatusReply::Report(status)) = &inner.reply else {
        return Err("Status is unavailable");
    };
    let binding = CompleteReview::new(&review.review, &status.context)?.selected_target()?;
    let target = match binding {
        "local" => writer_outcome::SelectedTarget::Local,
        _ => writer_outcome::SelectedTarget::Push,
    };
    let mut unique = HashSet::new();
    let mut selected = BTreeMap::<String, Vec<u32>>::new();
    let mut details = String::new();
    for token in tokens {
        if token.len() > 256 || !unique.insert(token) {
            return Err("Invalid selected lines");
        }
        let (index, line) = review
            .lines
            .iter()
            .find_map(|(index, lines)| {
                lines
                    .iter()
                    .find(|line| line.id == *token)
                    .map(|line| (*index, line))
            })
            .ok_or("Selected line is stale or unavailable")?;
        select(inner, &format!("capture:{id}:{index}"))?;
        let Some(protocol::SyncRecord::Capture(capture)) = review.review.records.get(index) else {
            return Err("Selected capture is unavailable");
        };
        selected
            .entry(capture.key.raw.clone())
            .or_default()
            .push(line.edit.id);
        if selected.len() > 1 {
            return Err("Select edits from one capture only");
        }
        let kind = match (line.edit.old_line, line.edit.new_line) {
            (Some(_), Some(_)) => "replacement",
            (None, Some(_)) => "insertion",
            (Some(_), None) => "deletion",
            (None, None) => return Err("Invalid selected line"),
        };
        for c in capture.key.raw.chars() {
            if runner::unsafe_display_char(c) {
                details.extend(c.escape_unicode());
            } else {
                details.push(c);
            }
            if details.len() > 24 * 1024 {
                return Err("Review is too large for confirmation");
            }
        }
        writeln!(
            details,
            ": {kind}, old {}, new {}",
            line.edit
                .old_line
                .map_or_else(|| "-".into(), |n| n.to_string()),
            line.edit
                .new_line
                .map_or_else(|| "-".into(), |n| n.to_string()),
        )
        .map_err(|_| "Review is too large for confirmation")?;
        if details.len() > 24 * 1024 {
            return Err("Review is too large for confirmation");
        }
    }
    let mut text = String::from(
        "The NEW commit is limited to these chosen edits; the remainder is preserved:\n",
    );
    text.push_str(&details);
    writeln!(text, "Selected edits: {}", tokens.len())
        .map_err(|_| "Review is too large for confirmation")?;
    if let Some(message) = message {
        text.push_str("Commit message:\n");
        for c in message.as_str().chars() {
            if runner::unsafe_display_char(c) {
                text.extend(c.escape_unicode());
            } else {
                text.push(c);
            }
        }
        text.push('\n');
    }
    writeln!(text, "Reviewed target binding: {binding}")
        .map_err(|_| "Review is too large for confirmation")?;
    match target {
        writer_outcome::SelectedTarget::Local => text.push_str("This creates a local commit only; no push will run. No pull, rebase, autostash, apply or hooks will run.\n\nContinue?"),
        writer_outcome::SelectedTarget::Push => text.push_str("A normal push can publish existing local branch ancestry. Last-fetched counts are not the live remote. No pull, rebase, autostash, apply or hooks will run. A rejected push leaves the local commit in place; it will not be retried.\n\nContinue?"),
    }
    if text.len() > 32 * 1024 {
        return Err("Review is too large for confirmation");
    }
    Ok(SelectedReview {
        lines: selected,
        text,
        target,
    })
}

fn actual_items(events: &[writer_outcome::WriterEvent]) -> Vec<ReviewItem> {
    events
        .iter()
        .filter(|event| event.phase == writer_outcome::WriterPhase::Actual)
        .filter_map(|event| {
            let (group, text) = match &event.record {
                protocol::SyncRecord::Stage { stage, .. } => {
                    ("Reached stages", safe_token(stage).to_owned())
                }
                protocol::SyncRecord::Resolved { key, effect, .. } => {
                    ("Changes", format!("{} · {}", key.shown, safe_token(effect)))
                }
                protocol::SyncRecord::Undecided { key, reason, .. } => (
                    "Attention",
                    format!(
                        "{} · {}",
                        key.as_ref().map_or("Context", |k| k.shown.as_str()),
                        safe_token(reason)
                    ),
                ),
                protocol::SyncRecord::Refusal { key, kind, .. } => (
                    "Refusals",
                    format!(
                        "{} · {}",
                        key.as_ref().map_or("Context", |k| k.shown.as_str()),
                        safe_token(kind)
                    ),
                ),
                protocol::SyncRecord::Hook { outcome, .. } => {
                    ("Hooks", safe_token(outcome).to_owned())
                }
                _ => return None,
            };
            Some(ReviewItem {
                group,
                text,
                diff_id: None,
            })
        })
        .collect()
}

fn finish_writer(
    inner: &mut Inner,
    receipt: &runner::Receipt,
    context: &protocol::StatusContext,
) -> WriterView {
    finish_writer_mode(inner, receipt, context, None)
}

fn selected_error_message(
    target: writer_outcome::SelectedTarget,
    events: &[writer_outcome::WriterEvent],
) -> &'static str {
    if target == writer_outcome::SelectedTarget::Local {
        "Selected sync failed; a local commit may exist. It was not retried."
    } else if events.iter().any(|event| {
        matches!(
            &event.record,
            protocol::SyncRecord::Stage { stage, dry_run: false } if stage == "push"
        )
    }) {
        "Selected sync failed after reaching push; the local commit was created and the remote outcome may be uncertain. It was not retried."
    } else {
        "Selected sync failed; a local commit may exist or a push may have happened. It was not retried."
    }
}

fn finish_writer_mode(
    inner: &mut Inner,
    receipt: &runner::Receipt,
    context: &protocol::StatusContext,
    selected: Option<writer_outcome::SelectedTarget>,
) -> WriterView {
    let code = receipt.status.code();
    let parsed = selected.map_or_else(
        || writer_outcome::parse_writer_outcome(&receipt.stdout, code, context),
        |target| writer_outcome::parse_selected_outcome(&receipt.stdout, code, context, target),
    );
    if code == Some(4) {
        let selected_message = selected.map(|target| {
            let events = match &parsed {
                Ok(writer_outcome::WriterOutcome::Error { events, .. }) => events.as_slice(),
                _ => &[],
            };
            selected_error_message(target, events)
        });
        let mut view = writer_view(
            "terminal_recovery",
            selected_message.unwrap_or(
                "Terminal recovery is required. Check dotman in a terminal before refreshing.",
            ),
            false,
        );
        if let Ok(writer_outcome::WriterOutcome::Error {
            context: actual,
            error_kind,
            recovery,
            ..
        }) = parsed
        {
            let kind = error_kind.as_deref().unwrap_or("");
            if matches!(kind, "pull_conflict" | "in_progress")
                && actual
                    .as_ref()
                    .is_some_and(|actual| protocol::terminal_context_matches(context, actual, kind))
                && let Some(rows) = recovery
                    .as_deref()
                    .and_then(|steps| recovery::project(steps).ok())
            {
                view.message = if kind == "pull_conflict" {
                    "A pull conflict requires terminal recovery."
                } else {
                    "An operation in progress requires terminal recovery."
                };
                view.recovery = Some(rows);
            }
            inner.recovery = recovery;
        }
        return view;
    }
    match parsed {
        Ok(writer_outcome::WriterOutcome::Receipt(result)) => {
            let mut view = if result.exit_code == 0 {
                writer_view("done", "Sync completed.", true)
            } else {
                writer_view(
                    "attention",
                    "Sync needs attention; some changes may have occurred.",
                    true,
                )
            };
            view.items = actual_items(&result.events);
            view
        }
        Ok(writer_outcome::WriterOutcome::Error { events, .. }) => {
            let mut view = writer_view(
                "error",
                selected.map_or("Sync failed; some changes may have occurred.", |target| {
                    selected_error_message(target, &events)
                }),
                true,
            );
            view.items = actual_items(&events);
            view
        }
        Err(_) => writer_view(
            "error",
            selected.map_or(
                "Sync returned an invalid or incomplete result; some changes may have occurred.",
                |target| selected_error_message(target, &[]),
            ),
            true,
        ),
    }
}

fn finish_apply_writer(
    inner: &mut Inner,
    receipt: &runner::Receipt,
    context: &protocol::StatusContext,
) -> WriterView {
    let code = receipt.status.code();
    let parsed = writer_outcome::parse_apply_outcome(&receipt.stdout, code, context);
    if code == Some(4) {
        let mut view = writer_view(
            "terminal_recovery",
            "Terminal recovery is required. Check dotman in a terminal before refreshing.",
            false,
        );
        if let Ok(writer_outcome::WriterOutcome::Error {
            context: actual,
            error_kind,
            recovery,
            ..
        }) = parsed
        {
            let kind = error_kind.as_deref().unwrap_or("");
            if matches!(kind, "pull_conflict" | "in_progress")
                && actual
                    .as_ref()
                    .is_some_and(|actual| protocol::terminal_context_matches(context, actual, kind))
                && let Some(rows) = recovery
                    .as_deref()
                    .and_then(|steps| recovery::project(steps).ok())
            {
                view.message = if kind == "pull_conflict" {
                    "A pull conflict requires terminal recovery."
                } else {
                    "An operation in progress requires terminal recovery."
                };
                view.recovery = Some(rows);
            }
            inner.recovery = recovery;
        }
        return view;
    }
    match parsed {
        Ok(writer_outcome::WriterOutcome::Receipt(result)) => {
            let mut view = if result.exit_code == 0 {
                writer_view("done", "File decisions applied.", true)
            } else {
                writer_view(
                    "attention",
                    "File decisions need attention; some changes may have occurred.",
                    true,
                )
            };
            view.items = actual_items(&result.events);
            view
        }
        Ok(writer_outcome::WriterOutcome::Error { events, .. }) => {
            let mut view = writer_view(
                "error",
                "File decisions failed; some changes may have occurred.",
                true,
            );
            view.items = actual_items(&events);
            view
        }
        Err(_) => writer_view(
            "error",
            "Apply returned an invalid or incomplete result; some changes may have occurred.",
            true,
        ),
    }
}

fn wait_apply(
    run: &mut runner::StatusRun,
    flag: &AtomicBool,
) -> Result<runner::Receipt, WriterView> {
    let start = Instant::now();
    let mut cleanup = None;
    loop {
        if (flag.load(Ordering::Acquire) || start.elapsed() >= Duration::from_secs(300))
            && cleanup.is_none()
        {
            let _ = run.cancel();
            cleanup = Some(Instant::now());
        }
        match run.poll() {
            Ok(Some(receipt)) => return Ok(receipt),
            Err(_) if run.is_reaped() => {
                return Err(writer_view(
                    if flag.load(Ordering::Acquire) {
                        "cancelled"
                    } else {
                        "error"
                    },
                    "Apply ended without a usable result; check the terminal. Cancellation does not undo changes.",
                    false,
                ));
            }
            Err(_) => {
                if cleanup.is_none() {
                    let _ = run.cancel();
                    cleanup = Some(Instant::now());
                }
            }
            Ok(None) => {}
        }
        if cleanup.is_some_and(|at| at.elapsed() >= Duration::from_secs(5)) {
            return Err(writer_view(
                "cleanup_unresolved",
                "Apply cleanup is unresolved. Check the terminal; cancellation does not undo changes.",
                false,
            ));
        }
        if run.wait_readable().is_err() && cleanup.is_none() {
            let _ = run.cancel();
            cleanup = Some(Instant::now());
        }
    }
}

#[tauri::command]
pub async fn run_fresh_setup(
    id: String,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<WriterView, String> {
    run_setup(id, None, window, state).await
}

#[tauri::command]
pub async fn run_clone_setup(
    id: String,
    source: String,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<WriterView, String> {
    let Ok(source) = setup::CloneSource::parse(source) else {
        return Ok(writer_view(
            "attention",
            "Clone did not start. Enter a public HTTPS URL without credentials or an absolute file URL, then refresh status to try again.",
            false,
        ));
    };
    run_setup(id, Some(source), window, state).await
}

async fn run_setup(
    id: String,
    source: Option<setup::CloneSource>,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<WriterView, String> {
    let flag = Arc::new(AtomicBool::new(false));
    let (intent, text) = {
        let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
        reserve_setup(&mut inner, &id, source, flag.clone())?
    };
    let (title, action) = if intent.source().is_some() {
        ("Clone dotman repository?", "Clone repository")
    } else {
        ("Create new dotman repository?", "Create repository")
    };
    let shared = state.inner.clone();
    let cancelled = flag.clone();
    let uncertain = Arc::new(AtomicBool::new(false));
    let worker_uncertain = uncertain.clone();
    let worker = tauri::async_runtime::spawn_blocking(move || -> Result<WriterView, String> {
        let approved = crate::confirm::ask(&window, title, action, text, cancelled.clone())?;
        let mut run = {
            let guard = shared.lock().map_err(|_| "Status state unavailable")?;
            if cancelled.load(Ordering::Acquire) {
                return Ok(writer_view("cancelled", "Setup cancelled before starting.", false));
            }
            if !approved {
                return Ok(writer_view("declined", "Setup declined.", false));
            }
            let run = runner::start_setup(&intent).map_err(|_| {
                worker_uncertain.store(true, Ordering::Release);
                "Could not start setup; the repository may already exist. Check status in a terminal."
            })?;
            drop(guard);
            run
        };
        let receipt = match wait_apply(&mut run, &cancelled) {
            Ok(receipt) => receipt,
            Err(mut view) => {
                view.message = "Setup ended without a usable result; the repository may already exist. Check status in a terminal.";
                return Ok(view);
            }
        };
        if receipt.status.code() == Some(4) {
            let mut inner = shared.lock().map_err(|_| "Status state unavailable")?;
            inner.recovery = Some(Vec::new());
            drop(inner);
            return Ok(writer_view("terminal_recovery", "Terminal recovery is required. Check dotman in a terminal before refreshing.", false));
        }
        let Ok(publication) = intent.parse_publication(&receipt.stdout, receipt.status.code()) else {
            return Ok(writer_view("attention", "Setup could not be verified; the repository may already exist. Refresh status manually.", false));
        };
        if cancelled.load(Ordering::Acquire) {
            return Ok(writer_view("attention", "Setup could not be verified; the repository may already exist. Refresh status manually.", false));
        }
        match run_status(&cancelled) {
            Ok(protocol::StatusReply::Report(after)) if intent.is_published_in(&publication, &after) => {
                Ok(writer_view("done", if intent.source().is_some() {
                    "Repository cloned and verified. Review files separately."
                } else {
                    "New repository created and verified."
                }, true))
            }
            Ok(protocol::StatusReply::Error { exit_code: 4, context, error_kind, recovery }) => {
                let mut view = writer_view("terminal_recovery", "Terminal recovery is required. Check dotman in a terminal before refreshing.", false);
                let kind = error_kind.as_deref().unwrap_or("");
                let bound = matches!(kind, "pull_conflict" | "in_progress")
                    && context.as_ref().is_some_and(|actual| protocol::terminal_context_matches(publication.context(), actual, kind));
                let recovery = recovery.filter(|_| bound);
                view.recovery = recovery.as_deref().and_then(|steps| recovery::project(steps).ok());
                let mut inner = shared.lock().map_err(|_| "Status state unavailable")?;
                inner.recovery = Some(recovery.unwrap_or_default());
                drop(inner);
                Ok(view)
            }
            _ => Ok(writer_view("attention", "Setup could not be verified; the repository may already exist. Refresh status manually.", false)),
        }
    }).await;
    let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
    let Ok(result) = worker else {
        return Err(
            "Setup worker failed; the repository may already exist. Check status in a terminal."
                .into(),
        );
    };
    if !uncertain.load(Ordering::Acquire)
        && !matches!(&result, Ok(view) if view.outcome == "cleanup_unresolved")
    {
        inner.active = None;
    }
    drop(inner);
    result
}

fn identity_receipt(receipt: &runner::Receipt, review: &ReviewedIdentity<'_>) -> Option<i32> {
    let writer_outcome::WriterOutcome::Receipt(result) = writer_outcome::parse_apply_outcome(
        &receipt.stdout,
        receipt.status.code(),
        review.context(),
    )
    .ok()?
    else {
        return None;
    };
    if result.error_kind.is_some()
        || result.events.iter().any(|event| match &event.record {
            protocol::SyncRecord::Resolved { .. } | protocol::SyncRecord::Refusal { .. } => true,
            protocol::SyncRecord::Undecided { reason, key, .. } => {
                reason == "review_stale" && key.is_none()
            }
            _ => false,
        })
        || !result.events.iter().any(|event| {
            matches!(&event.record,
            protocol::SyncRecord::Stage { stage, dry_run: false } if stage == "hooks")
        })
        || !result.events.iter().any(|event| {
            matches!(&event.record,
            protocol::SyncRecord::Hook { outcome, .. } if outcome == "disabled")
        })
    {
        return None;
    }
    Some(result.exit_code)
}

fn identity_verification(
    review: &ReviewedIdentity<'_>,
    exit: i32,
    after: Option<&protocol::StatusReport>,
) -> WriterView {
    if after.is_some_and(|report| review.is_recorded_in(report)) {
        if exit == 3 {
            writer_view(
                "attention",
                "Identity recorded. File decisions remain pending for separate review.",
                true,
            )
        } else {
            writer_view("done", "Identity recorded.", true)
        }
    } else {
        writer_view(
            "attention",
            "Identity recording could not be verified; refresh status before continuing.",
            true,
        )
    }
}

fn verify_identity_recording(
    shared: &Mutex<Inner>,
    flag: &AtomicBool,
    review: &ReviewedIdentity<'_>,
    exit: i32,
) -> Result<WriterView, String> {
    if flag.load(Ordering::Acquire) {
        return Ok(identity_verification(review, exit, None));
    }
    match run_status(flag) {
        Ok(protocol::StatusReply::Report(after)) => {
            Ok(identity_verification(review, exit, Some(&after)))
        }
        Ok(protocol::StatusReply::Error {
            exit_code: 4,
            context,
            error_kind,
            recovery,
        }) => {
            let mut inner = shared.lock().map_err(|_| "Status state unavailable")?;
            let mut view = writer_view(
                "terminal_recovery",
                "Terminal recovery is required. Check dotman in a terminal before refreshing.",
                false,
            );
            let kind = error_kind.as_deref().unwrap_or("");
            if matches!(kind, "pull_conflict" | "in_progress")
                && context.as_ref().is_some_and(|actual| {
                    protocol::terminal_context_matches(review.context(), actual, kind)
                })
                && let Some(rows) = recovery
                    .as_deref()
                    .and_then(|steps| recovery::project(steps).ok())
            {
                view.recovery = Some(rows);
            }
            inner.recovery = recovery;
            drop(inner);
            Ok(view)
        }
        _ => Ok(identity_verification(review, exit, None)),
    }
}

fn reserve_hooks(
    inner: &mut Inner,
    id: &str,
    cancelled: Arc<AtomicBool>,
) -> Result<(protocol::StatusReport, String), &'static str> {
    if inner.active.is_some() {
        return Err("Another operation is running");
    }
    if inner.generation == u64::MAX || id != format!("hooks:{}", inner.generation) {
        return Err("Hook review is stale");
    }
    let Some(protocol::StatusReply::Report(report)) = &inner.reply else {
        return Err("Status is unavailable");
    };
    if report.entries.iter().any(redacted) {
        return Err("Hook review is unavailable");
    }
    let text = HookReview::new(report)?.modal()?;
    let Some(protocol::StatusReply::Report(report)) = inner.reply.take() else {
        return Err("Status is unavailable");
    };
    inner.review = None;
    inner.diff_receipts.clear();
    inner.setup = None;
    inner.generation = inner.generation.saturating_add(1);
    inner.active = Some(cancelled);
    Ok((report, text))
}

#[tauri::command]
pub async fn approve_pending_hooks(
    id: String,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<WriterView, String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let (report, text) = {
        let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
        reserve_hooks(&mut inner, &id, cancelled.clone())?
    };
    let shared = state.inner.clone();
    let flag = cancelled.clone();
    let uncertain = Arc::new(AtomicBool::new(false));
    let worker_uncertain = uncertain.clone();
    let worker = tauri::async_runtime::spawn_blocking(move || -> Result<WriterView, String> {
        let approved = crate::confirm::ask(&window, "Approve pending hooks?", "Approve and run hooks", text, flag.clone())?;
        let review = HookReview::new(&report)?;
        let mut run = {
            let guard = shared.lock().map_err(|_| "Status state unavailable")?;
            if flag.load(Ordering::Acquire) {
                return Ok(writer_view("cancelled", "Hook review cancelled before starting.", true));
            }
            if !approved {
                return Ok(writer_view("declined", "Hook approval declined; no hooks were run.", true));
            }
            let run = runner::start_reviewed_hooks(&review).map_err(|_| {
                worker_uncertain.store(true, Ordering::Release);
                "Could not start hook approval; check the terminal before continuing"
            })?;
            drop(guard);
            run
        };
        let receipt = match wait_apply(&mut run, &flag) {
            Ok(receipt) => receipt,
            Err(view) => return Ok(view),
        };
        if receipt.status.code() == Some(3) {
            let message = if review.stale_receipt(&receipt.stdout, receipt.status.code()) {
                "Hook review became stale; no hooks ran. Refresh and review again."
            } else {
                "Hook review needs attention; its result could not be verified. Hooks may have run."
            };
            return Ok(writer_view("attention", message, true));
        }
        if receipt.status.code() == Some(4) {
            return Ok(writer_view("terminal_recovery", "An operation requires terminal recovery.", false));
        }
        if !review.receipt(&receipt.stdout, receipt.status.code()) {
            return Ok(writer_view("error", "Hook approval failed or returned an incomplete receipt; hooks may have run. Check the terminal.", true));
        }
        match run_status(&flag) {
            Ok(protocol::StatusReply::Report(after)) if review.verified(&after) =>
                Ok(writer_view("done", "Pending hooks ran and approval was verified.", true)),
            _ => Ok(writer_view("attention", "Hooks ran, but approval could not be verified in fresh status.", true)),
        }
    }).await;
    let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
    let Ok(result) = worker else {
        return Err("Hook worker failed; hooks may have run. Check the terminal.".into());
    };
    if !uncertain.load(Ordering::Acquire)
        && !matches!(&result, Ok(view) if view.outcome == "cleanup_unresolved")
    {
        inner.active = None;
    }
    drop(inner);
    result
}

#[tauri::command]
pub async fn accept_reviewed_identity(
    id: String,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<WriterView, String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let (report, text) = {
        let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
        reserve_identity(&mut inner, &id, cancelled.clone())?
    };
    let shared = state.inner.clone();
    let flag = cancelled.clone();
    let uncertain = Arc::new(AtomicBool::new(false));
    let worker_uncertain = uncertain.clone();
    let worker = tauri::async_runtime::spawn_blocking(move || -> Result<WriterView, String> {
        let approved = crate::confirm::ask(
            &window,
            "Accept machine identity change?",
            "Accept identity change",
            text,
            flag.clone(),
        )?;
        let review = ReviewedIdentity::new(&report)?;
        let mut run = {
            let guard = shared.lock().map_err(|_| "Status state unavailable")?;
            if flag.load(Ordering::Acquire) {
                return Ok(writer_view(
                    "cancelled",
                    "Identity review cancelled before starting.",
                    false,
                ));
            }
            if !approved {
                return Ok(writer_view("declined", "Identity change declined.", false));
            }
            let run = runner::start_reviewed_identity(&review).map_err(|_| {
                worker_uncertain.store(true, Ordering::Release);
                "Could not start identity approval; check the terminal before continuing"
            })?;
            drop(guard);
            run
        };
        let receipt = match wait_apply(&mut run, &flag) {
            Ok(receipt) => receipt,
            Err(view) => return Ok(view),
        };
        if receipt.status.code() == Some(4) {
            let mut inner = shared.lock().map_err(|_| "Status state unavailable")?;
            return Ok(finish_apply_writer(&mut inner, &receipt, review.context()));
        }
        let Some(exit) = identity_receipt(&receipt, &review) else {
            return Ok(writer_view(
                "attention",
                "Identity recording could not be verified; refresh status before continuing.",
                true,
            ));
        };
        verify_identity_recording(&shared, &flag, &review, exit)
    })
    .await;
    let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
    let Ok(result) = worker else {
        return Err("Identity worker failed; check the terminal before continuing".into());
    };
    if !uncertain.load(Ordering::Acquire)
        && !matches!(&result, Ok(view) if view.outcome == "cleanup_unresolved")
    {
        inner.active = None;
    }
    drop(inner);
    result
}

#[tauri::command]
pub async fn apply_reviewed_files(
    answers: Vec<FileAnswer>,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<WriterView, String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let reservation = {
        let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
        reserve_apply(&mut inner, &answers, cancelled.clone())?
    };
    let shared = state.inner.clone();
    let flag = cancelled.clone();
    let uncertain = Arc::new(AtomicBool::new(false));
    let worker_uncertain = uncertain.clone();
    let worker = tauri::async_runtime::spawn_blocking(move || -> Result<WriterView, String> {
        let approved = crate::confirm::ask(
            &window,
            "Apply reviewed file decisions?",
            "Apply reviewed decisions",
            reservation.text.clone(),
            flag.clone(),
        )?;
        let mut run = {
            let mut inner = shared.lock().map_err(|_| "Status state unavailable")?;
            if flag.load(Ordering::Acquire) {
                return Ok(writer_view(
                    "cancelled",
                    "Apply cancelled before starting.",
                    false,
                ));
            }
            if !approved {
                return Ok(writer_view("declined", "Apply declined.", false));
            }
            let reviewed = reservation.reviewed()?;
            inner.reply = None;
            inner.review = None;
            inner.diff_receipts.clear();
            inner.recovery = None;
            inner.generation = inner.generation.saturating_add(1);
            let run = runner::start_reviewed_apply(&reviewed).map_err(|_| {
                worker_uncertain.store(true, Ordering::Release);
                "Could not start reviewed apply; check the terminal before continuing"
            })?;
            drop(inner);
            run
        };
        match wait_apply(&mut run, &flag) {
            Ok(receipt) => {
                let mut inner = shared.lock().map_err(|_| "Status state unavailable")?;
                Ok(finish_apply_writer(
                    &mut inner,
                    &receipt,
                    &reservation.context,
                ))
            }
            Err(view) => Ok(view),
        }
    })
    .await;
    let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
    let Ok(result) = worker else {
        inner.reply = None;
        inner.review = None;
        inner.diff_receipts.clear();
        return Err("Apply worker failed; check the terminal before continuing".into());
    };
    if !uncertain.load(Ordering::Acquire)
        && !matches!(&result, Ok(view) if view.outcome == "cleanup_unresolved")
    {
        inner.active = None;
    }
    drop(inner);
    result
}

#[tauri::command]
#[expect(
    clippy::too_many_lines,
    reason = "The confirmation, spawn and cancellation lifecycle is kept together."
)]
pub async fn run_reviewed_sync(
    id: String,
    commit_message: Option<String>,
    selected_lines: Option<Vec<String>>,
    mass_delete: Option<bool>,
    progress: tauri::ipc::Channel<WriterStage>,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<WriterView, String> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let commit_message = commit_message.map(runner::CommitMessage::new).transpose()?;
    let (review, context, text, selected, selected_target) = {
        let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
        let selected = selected_lines
            .as_ref()
            .map(|tokens| selected_review(&inner, &id, tokens, commit_message.as_ref()))
            .transpose()?;
        let (selected, selected_text, selected_target) =
            selected.map_or((None, None, None), |selected| {
                (
                    Some(selected.lines),
                    Some(selected.text),
                    Some(selected.target),
                )
            });
        let retained = inner
            .review
            .as_ref()
            .filter(|review| review.id == id)
            .ok_or("Review is stale or unavailable")?;
        let is_mass = retained.review.error_kind.as_deref() == Some("mass_delete");
        let approve_mass = mass_delete.unwrap_or(false);
        if is_mass != approve_mass || (approve_mass && selected.is_some()) {
            return Err("Mass deletion requires its own review and explicit approval".into());
        }
        let (review, context, text) =
            inner.consume_review_mode(&id, commit_message.as_ref(), selected_text)?;
        let text = if approve_mass {
            let complete = CompleteReview::new(&review, &context)?;
            mass_delete_modal(text, &complete)?
        } else {
            text
        };
        inner.active = Some(cancelled.clone());
        drop(inner);
        (review, context, text, selected, selected_target)
    };
    let shared = state.inner.clone();
    let flag = cancelled.clone();
    let start_uncertain = Arc::new(AtomicBool::new(false));
    let worker_uncertain = start_uncertain.clone();
    let worker = tauri::async_runtime::spawn_blocking(move || -> Result<WriterView, String> {
        let (title, action) = if selected.is_some() {
            ("Commit selected lines?", "Commit selected lines")
        } else {
            ("Run reviewed sync?", "Run reviewed sync")
        };
        let approved = crate::confirm::ask(&window, title, action, text, flag.clone())?;
        let mut run = {
            let mut inner = shared.lock().map_err(|_| "Status state unavailable")?;
            if flag.load(Ordering::Acquire) { return Ok(writer_view("cancelled", "Sync cancelled before starting.", false)); }
            if !approved { return Ok(writer_view("declined", "Sync declined.", false)); }
            let complete = CompleteReview::new(&review, &context)?;
            inner.reply = None;
            inner.review = None;
            inner.diff_receipts.clear();
            inner.recovery = None;
            inner.generation = inner.generation.saturating_add(1);
            let run = runner::start_reviewed_sync(complete, commit_message.as_ref(), selected.as_ref(), mass_delete.unwrap_or(false)).map_err(|_| {
                worker_uncertain.store(true, Ordering::Release);
                "Could not start reviewed sync; check the terminal before continuing"
            })?;
            drop(inner);
            run
        };
        if !flag.load(Ordering::Acquire) {
            let _ = progress.send(WriterStage::Started);
        }
        let mut observer = writer_outcome::WriterProgress::new(&context).ok();
        let start = Instant::now();
        let mut cleanup = None;
        loop {
            if (flag.load(Ordering::Acquire) || start.elapsed() >= Duration::from_secs(300)) && cleanup.is_none() {
                let _ = run.cancel();
                cleanup = Some(Instant::now());
            }
            let polled = run.poll();
            if matches!(&polled, Ok(None)) && cleanup.is_none()
                && !flag.load(Ordering::Acquire)
                && let Some(observer) = &mut observer
            {
                observer.observe(run.stdout(), |stage| {
                    if !flag.load(Ordering::Acquire) {
                        let _ = progress.send(stage.into());
                    }
                });
            }
            match polled {
                Ok(Some(receipt)) => {
                    let mut inner = shared.lock().map_err(|_| "Status state unavailable")?;
                    return Ok(if selected_target.is_some() {
                        finish_writer_mode(&mut inner, &receipt, &context, selected_target)
                    } else {
                        finish_writer(&mut inner, &receipt, &context)
                    });
                }
                Err(_) if run.is_reaped() => {
                    return Ok(writer_view(if flag.load(Ordering::Acquire) { "cancelled" } else { "error" }, "Sync ended without a usable result; check the terminal. Cancellation does not undo changes.", false));
                }
                Err(_) => {
                    if cleanup.is_none() { let _ = run.cancel(); cleanup = Some(Instant::now()); }
                }
                Ok(None) => {}
            }
            if cleanup.is_some_and(|at| at.elapsed() >= Duration::from_secs(5)) {
                return Ok(writer_view("cleanup_unresolved", "Sync cleanup is unresolved. Check the terminal; cancellation does not undo changes.", false));
            }
            if run.wait_readable().is_err() && cleanup.is_none() {
                let _ = run.cancel();
                cleanup = Some(Instant::now());
            }
        }
    }).await;
    let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
    let Ok(result) = worker else {
        inner.reply = None;
        inner.review = None;
        inner.diff_receipts.clear();
        drop(inner);
        return Err("Sync worker failed; check the terminal before continuing".into());
    };
    if !start_uncertain.load(Ordering::Acquire)
        && !matches!(&result, Ok(view) if view.outcome == "cleanup_unresolved")
    {
        inner.active = None;
    }
    result
}

fn run_status(cancelled: &AtomicBool) -> Result<protocol::StatusReply, &'static str> {
    let mut run = runner::start_status().map_err(|_| "Could not start the dotman companion")?;
    let start = Instant::now();
    loop {
        if cancelled.load(Ordering::Acquire) {
            run.cancel().map_err(|_| "Could not cancel status")?;
            return Err("Status read cancelled");
        }
        if start.elapsed() >= Duration::from_secs(30) {
            run.cancel()
                .map_err(|_| "Could not cancel timed-out status")?;
            return Err("Status read timed out");
        }
        match run.poll().map_err(|_| "Status process failed")? {
            Some(receipt) => {
                return protocol::parse_status(&receipt.stdout, receipt.status.code())
                    .map_err(|_| "Invalid status response");
            }
            None => run.wait_readable().map_err(|_| "Status process failed")?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTEXT: &str = r#"{"record":"context","schema_version":1,"experimental":true,"command":"status","home":{"raw":"/private-raw-home","shown":"/home"},"repo":{"raw":"/private-raw-repo","shown":"/repo"},"config":{"raw":"/config","shown":"/config"},"repo_config":{"raw":"/repo/.dotman.toml","shown":"/repo/.dotman.toml"},"state":{"raw":"/state","shown":"/state"},"identity":{"raw":"/identity","shown":"/identity"},"age_key_file":null,"repo_id":{"raw":"private-repo-id","shown":"id"},"machine":{"host":{"raw":"private-host","shown":"h"},"user":{"raw":"u","shown":"u"},"os":{"raw":"linux","shown":"linux"},"distro":{"raw":"","shown":""},"like":{"raw":"","shown":""},"arch":{"raw":"x","shown":"x"},"profiles":[]}}"#;
    const ENTRY: &str = r#"{"record":"entry","status":"clean","key":{"raw":"private-key","shown":"é<img src=x onerror=alert(1)>"},"scope":"home","path":null,"source":null,"recorded_source":null,"encrypted":false,"template":false,"system":false,"row":"clean","row_number":4,"category":null,"r":{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}},"source_tuple":{"state":"missing"},"h":{"state":"missing"},"raw_h":{"state":"missing"},"s":{"state":"missing"},"identity_changed":false,"update_state":false}"#;
    const REVIEW: &str = r#"{"record":"review_context","hooks":{"standing":"empty"},"remote":{"basis":"unknown","ahead":null,"behind":null}}"#;
    const SUMMARY: &str = r#"{"record":"summary","rows":1,"clean":1,"new":0,"held":0,"held_in_home":0,"held_in_repo":0,"unrecorded":0}"#;
    const RESULT: &str = r#"{"record":"result","outcome":"done","exit_code":0,"error_kind":null}"#;

    pub(super) fn report() -> Result<protocol::StatusReply, String> {
        protocol::parse_status(
            format!("{CONTEXT}\n{ENTRY}\n{ENTRY}\n{REVIEW}\n{SUMMARY}\n{RESULT}\n").as_bytes(),
            Some(0),
        )
    }

    #[test]
    fn edit_command_uses_safe_managed_path_only() -> Result<(), Box<dyn std::error::Error>> {
        let protocol::StatusReply::Report(mut report) = report()? else {
            return Err("missing report".into());
        };
        report.entries.truncate(1);
        let entry = report.entries.first_mut().ok_or("missing entry")?;
        entry.key.raw = "-a b'c".into();
        entry.key.shown = "different".into();
        entry.source = Some(protocol::StatusText {
            raw: "-a b'c".into(),
            shown: "different".into(),
        });
        entry.source_tuple = entry.r.clone();
        entry.status = "template-drift".into();
        let paths = HeldPaths::new(&report);
        let entry = report.entries.first().ok_or("missing entry")?;
        assert_eq!(
            edit_command(entry, &paths, Path::new("/home")),
            Some("dotman edit -- '/home/-a b'\\''c'".into())
        );
        let mut unavailable = entry.clone();
        unavailable.status = "clean".into();
        assert!(edit_command(&unavailable, &paths, Path::new("/home")).is_some());
        unavailable.key.raw = "bad\nname".into();
        assert!(edit_command(&unavailable, &paths, Path::new("/home")).is_none());
        unavailable.key.raw = entry.key.raw.clone();
        unavailable.status = "locked".into();
        assert!(edit_command(&unavailable, &paths, Path::new("/home")).is_none());
        unavailable.status = "clean".into();
        unavailable.encrypted = true;
        assert!(edit_command(&unavailable, &paths, Path::new("/home")).is_none());
        unavailable.encrypted = false;
        unavailable.source_tuple = protocol::StatusObservation::Missing;
        assert!(edit_command(&unavailable, &paths, Path::new("/home")).is_none());
        unavailable.source_tuple = entry.source_tuple.clone();
        unavailable.scope = "system".into();
        assert!(edit_command(&unavailable, &paths, Path::new("/home")).is_none());
        let mut held = entry.clone();
        held.status = "held".into();
        held.hold = Some(protocol::Hold::SecretName);
        report.entries.push(held);
        let paths = HeldPaths::new(&report);
        assert!(
            edit_command(
                report.entries.first().ok_or("missing entry")?,
                &paths,
                Path::new("/home")
            )
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn pending_hooks_reservation_is_one_use() -> Result<(), Box<dyn std::error::Error>> {
        let text = |raw: &str| serde_json::json!({"raw": raw, "shown": raw});
        let marker = serde_json::json!({
            "record":"review_context",
            "hooks":{"standing":"unapproved","canonical":text(r#"{".a":"echo ran"}"#),
                "entries":[{"pattern":text(".a"),"command":text("echo ran")}],
                "pending_keys":[text(".a")],"pending_commands":[text("echo ran")]},
            "remote":{"basis":"unknown","ahead":null,"behind":null}
        });
        let held = ENTRY.replace(r#""status":"clean""#, r#""status":"held""#);
        let hidden = protocol::parse_status(
            format!("{CONTEXT}\n{held}\n{marker}\n{SUMMARY}\n{RESULT}\n").as_bytes(),
            Some(0),
        )?;
        let hidden = project(&hidden, false, 1);
        assert!(hidden.hook_review_id.is_none());
        let reply = protocol::parse_status(
            format!("{CONTEXT}\n{marker}\n{SUMMARY}\n{RESULT}\n").as_bytes(),
            Some(0),
        )?;
        let mut inner = Inner::default();
        let view = inner.finish(Ok(reply), &AtomicBool::new(false), false);
        let id = view.hook_review_id.ok_or("missing hook review")?;
        assert!(reserve_hooks(&mut inner, "hooks:0", Arc::new(AtomicBool::new(false))).is_err());
        let (_, modal) = reserve_hooks(&mut inner, &id, Arc::new(AtomicBool::new(false)))?;
        assert!(modal.contains("echo ran"));
        assert!(reserve_hooks(&mut inner, &id, Arc::new(AtomicBool::new(false))).is_err());
        assert!(inner.reply.is_none());
        Ok(())
    }

    #[test]
    fn fresh_setup_requires_absence_and_is_consumed_once() -> Result<(), Box<dyn std::error::Error>>
    {
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = std::env::temp_dir().join(format!("dotman-app-fresh-{}", std::process::id()));
        std::fs::create_dir(&root)?;
        let fixture = Fixture(root);
        let mut context: serde_json::Value = serde_json::from_str(CONTEXT)?;
        *context.get_mut("repo").ok_or("missing repo")? =
            serde_json::json!({"raw":fixture.0.join("repo"),"shown":"not-the-path"});
        *context.get_mut("repo_id").ok_or("missing repo id")? = serde_json::Value::Null;
        let error = r#"{"record":"result","outcome":"error","exit_code":1,"error_kind":"other"}"#;
        let status = || protocol::parse_status(format!("{context}\n{error}\n").as_bytes(), Some(1));
        let mut inner = Inner::default();
        let view = inner.finish(Ok(status()?), &AtomicBool::new(false), false);
        let id = view.setup_review_id.ok_or("missing setup id")?;
        assert!(
            reserve_setup(
                &mut inner,
                "setup:0",
                None,
                Arc::new(AtomicBool::new(false))
            )
            .is_err()
        );
        let (_, text) = reserve_setup(&mut inner, &id, None, Arc::new(AtomicBool::new(false)))?;
        assert!(text.contains(fixture.0.to_str().ok_or("path")?));
        assert!(!text.contains("not-the-path"));
        assert!(text.contains("No home files are applied"));
        assert!(inner.reply.is_none() && inner.setup.is_none() && inner.active.is_some());
        assert!(reserve_setup(&mut inner, &id, None, Arc::new(AtomicBool::new(false))).is_err());
        inner.active = None;
        let view = inner.finish(Ok(status()?), &AtomicBool::new(false), false);
        let clone_id = view.setup_review_id.ok_or("missing clone setup id")?;
        assert!(setup::CloneSource::parse("https://user@example.com/repo".into()).is_err());
        let source = setup::CloneSource::parse("file:///fixture/source".into())?;
        assert!(
            reserve_setup(
                &mut inner,
                &id,
                Some(source),
                Arc::new(AtomicBool::new(false))
            )
            .is_err()
        );
        let source = setup::CloneSource::parse("file:///fixture/source".into())?;
        let (intent, text) = reserve_setup(
            &mut inner,
            &clone_id,
            Some(source),
            Arc::new(AtomicBool::new(false)),
        )?;
        assert_eq!(intent.source(), Some("file:///fixture/source"));
        assert!(text.contains("file:///fixture/source"));
        assert!(text.contains(fixture.0.to_str().ok_or("path")?));
        assert!(text.contains("Review files separately"));
        assert!(!text.contains("no hooks, keys, imports or remote actions"));
        assert!(
            reserve_setup(
                &mut inner,
                &clone_id,
                None,
                Arc::new(AtomicBool::new(false))
            )
            .is_err()
        );
        inner.active = None;
        std::fs::write(fixture.0.join("repo"), b"occupied")?;
        let view = inner.finish(Ok(status()?), &AtomicBool::new(false), false);
        assert!(view.setup_review_id.is_none());
        std::fs::remove_file(fixture.0.join("repo"))?;
        inner.recovery = Some(Vec::new());
        let view = inner.finish(Ok(status()?), &AtomicBool::new(false), false);
        assert!(view.setup_review_id.is_none());
        Ok(())
    }

    fn identity_fixture() -> Result<protocol::StatusReport, Box<dyn std::error::Error>> {
        let protocol::StatusReply::Report(mut report) = report()? else {
            return Err("missing report".into());
        };
        report.entries.truncate(1);
        let entry = report.entries.first_mut().ok_or("missing entry")?;
        entry.status = "identity-changed".into();
        entry.row = Some("changed-in-repo".into());
        entry.identity_changed = true;
        report.current_identity = Some(Box::new(report.context.machine.clone()));
        let mut previous = report.context.machine.clone();
        previous.host.raw = "alpha".into();
        report.recorded_identity = Some(Box::new(previous));
        Ok(report)
    }

    #[test]
    fn identity_review_is_current_one_use_and_reserves_whole_status()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut inner = Inner {
            reply: Some(protocol::StatusReply::Report(identity_fixture()?)),
            generation: 7,
            ..Inner::default()
        };
        inner.diff_receipts.insert(0);
        let view = project(inner.reply.as_ref().ok_or("reply")?, false, 7);
        assert_eq!(view.identity_review_id.as_deref(), Some("identity:7"));
        assert!(
            reserve_identity(&mut inner, "identity:6", Arc::new(AtomicBool::new(false))).is_err()
        );
        assert!(inner.active.is_none() && inner.reply.is_some());
        let (report, text) =
            reserve_identity(&mut inner, "identity:7", Arc::new(AtomicBool::new(false)))?;
        assert!(ReviewedIdentity::new(&report).is_ok());
        assert!(text.contains("Pending file choices require separate review"));
        assert!(inner.reply.is_none() && inner.diff_receipts.is_empty() && inner.active.is_some());
        assert!(
            reserve_identity(&mut inner, "identity:7", Arc::new(AtomicBool::new(false))).is_err()
        );
        inner.active = None;
        assert!(
            reserve_identity(&mut inner, "identity:7", Arc::new(AtomicBool::new(false))).is_err()
        );
        let mut invalid = identity_fixture()?;
        invalid.recorded_identity = None;
        inner.reply = Some(protocol::StatusReply::Report(invalid));
        assert!(
            reserve_identity(&mut inner, "identity:8", Arc::new(AtomicBool::new(false))).is_err()
        );
        assert!(inner.active.is_none() && inner.reply.is_some());
        Ok(())
    }

    #[test]
    fn identity_confirmation_quotes_full_raw_identity_and_bounds()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut report = identity_fixture()?;
        report.context.machine.host.raw = "beta\n\"\\\u{202e}".into();
        report.current_identity = Some(Box::new(report.context.machine.clone()));
        report
            .current_identity
            .as_mut()
            .ok_or("current")?
            .profiles
            .push(protocol::StatusText {
                raw: "profile-one".into(),
                shown: "misleading".into(),
            });
        report.context.machine.profiles = report
            .current_identity
            .as_ref()
            .ok_or("current")?
            .profiles
            .clone();
        let text = identity_modal(&ReviewedIdentity::new(&report)?)?;
        assert!(text.contains(r#"beta\u{a}\"\\\u{202e}"#));
        assert!(text.contains("profile-one") && !text.contains("misleading"));
        assert!(text.contains("Previous identity") && text.contains("Current identity"));
        assert!(text.contains("Distribution family") && text.contains("Architecture"));
        report.context.repo.shown = "x".repeat(33 * 1024);
        assert!(identity_modal(&ReviewedIdentity::new(&report)?).is_err());
        Ok(())
    }

    #[test]
    fn identity_receipts_require_complete_nonstale_writer_and_fresh_proof()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::process::ExitStatusExt;
        let report = identity_fixture()?;
        let review = ReviewedIdentity::new(&report)?;
        let context = CONTEXT.replace(r#""command":"status""#, r#""command":"apply""#);
        let stages = r#"{"record":"stage","stage":"apply","dry_run":false}
{"record":"undecided","reason":"review_stale","key":{"raw":"private-key","shown":"shown"},"scope":"home","pass":"apply","attention":true,"skip_next_pass":false}
{"record":"stage","stage":"hooks","dry_run":false}
{"record":"hook","command":null,"outcome":"disabled","exit_code":null}"#;
        let result = r#"{"record":"result","outcome":"error","exit_code":3,"error_kind":null}"#;
        let make = |events: &str| runner::Receipt {
            stdout: format!("{context}\n{events}\n{result}\n").into_bytes(),
            stderr: vec![],
            status: std::process::ExitStatus::from_raw(3 << 8),
        };
        let receipt = make(stages);
        assert_eq!(identity_receipt(&receipt, &review), Some(3));
        let mut after = identity_fixture()?;
        assert_eq!(
            identity_verification(&review, 3, Some(&after)).outcome,
            "attention"
        );
        assert!(
            identity_verification(&review, 3, Some(&after))
                .message
                .contains("could not be verified")
        );
        after.recorded_identity = after.current_identity.clone();
        let view = identity_verification(&review, 3, Some(&after));
        assert_eq!(view.outcome, "attention");
        assert!(view.message.contains("Identity recorded") && view.refresh);
        after.context.repo_id = None;
        assert!(
            identity_verification(&review, 3, Some(&after))
                .message
                .contains("could not be verified")
        );
        assert!(
            identity_receipt(
                &make(&stages.replace(
                    r#""key":{"raw":"private-key","shown":"shown"}"#,
                    r#""key":null"#
                )),
                &review
            )
            .is_none()
        );
        assert!(
            identity_receipt(
                &make(&stages.replace(
                    r#"{"record":"hook","command":null,"outcome":"disabled","exit_code":null}"#,
                    ""
                )),
                &review
            )
            .is_none()
        );
        let resolved = r#"{"record":"resolved","key":{"raw":"private-key","shown":"shown"},"scope":"home","effect":"written","hypothetical":false}"#;
        assert!(
            identity_receipt(
                &make(&stages.replace(
                    r#"{"record":"stage","stage":"hooks","dry_run":false}"#,
                    &format!(
                        "{resolved}\n{{\"record\":\"stage\",\"stage\":\"hooks\",\"dry_run\":false}}"
                    )
                )),
                &review
            )
            .is_none()
        );
        Ok(())
    }

    #[test]
    fn unavailable_repo_requires_other_error_with_null_id() -> Result<(), Box<dyn std::error::Error>>
    {
        let absent = CONTEXT.replace(
            r#""repo_id":{"raw":"private-repo-id","shown":"id"}"#,
            r#""repo_id":null"#,
        );
        for (context, code, kind, expected, finding) in [
            (Some(absent.as_str()), 1, "other", true, "unavailable"),
            (Some(CONTEXT), 1, "other", false, "unknown"),
            (
                Some(absent.as_str()),
                4,
                "in_progress",
                false,
                "in_progress",
            ),
            (Some(CONTEXT), 4, "pull_conflict", false, "pull_conflict"),
            (None, 4, "other", false, "stopped"),
            (None, 1, "root_refused", false, "unknown"),
            (None, 1, "other", false, "unknown"),
        ] {
            let result = format!(
                r#"{{"record":"result","outcome":"error","exit_code":{code},"error_kind":"{kind}","recovery":[{{"kind":"instruction","text":{{"raw":"secret recovery","shown":"recovery shown"}}}}]}}"#
            );
            let input = format!(
                "{}{}\n",
                context.map_or(String::new(), |c| format!("{c}\n")),
                result
            );
            let reply = protocol::parse_status(input.as_bytes(), Some(code))?;
            let view = project(&reply, false, 1);
            assert_eq!(view.repo_unavailable, expected);
            assert_eq!(view.finding, finding);
            assert!(!serde_json::to_string(&view)?.contains("recovery shown"));
        }
        Ok(())
    }

    #[test]
    fn machine_projection_uses_shown_values_and_cli_selection()
    -> Result<(), Box<dyn std::error::Error>> {
        let context = CONTEXT.replace(
            r#""profiles":[]"#,
            r#""profiles":[{"raw":"secret-profile","shown":"work"}]"#,
        );
        let entry = ENTRY.replace(r#""source":null"#, r#""source":{"raw":"private-key{host}.tmpl","shown":"selected"}"#)
            .replace(r#""recorded_source":null"#, r#""recorded_source":{"raw":"private-key{old}","shown":"recorded"}"#)
            .replace(r#""template":false"#, r#""template":true"#)
            .replace(r#""identity_changed":false"#, r#""identity_changed":true"#)
            .replace(r#""update_state":false"#, r#""update_state":false,"alternatives":[{"raw":"private-key{host}.tmpl","shown":"selected"},{"raw":"private-key{other}","shown":"alternative"}]"#);
        let ambiguous = entry
            .replace(r#""status":"clean""#, r#""status":"ambiguous""#)
            .replace(
                r#""source":{"raw":"private-key{host}.tmpl","shown":"selected"}"#,
                r#""source":null"#,
            );
        let reply = protocol::parse_status(
            format!("{context}\n{entry}\n{ambiguous}\n{ENTRY}\n{REVIEW}\n{SUMMARY}\n{RESULT}\n")
                .as_bytes(),
            Some(0),
        )?;
        let json = serde_json::to_value(project(&reply, false, 1))?;
        assert_eq!(
            json.pointer("/context/machine_details"),
            Some(&serde_json::json!({"host":"h","os":"linux","arch":"x","profiles":["work"]}))
        );
        assert_eq!(
            json.pointer("/entries/0/machine_file"),
            Some(
                &serde_json::json!({"selected":"selected","recorded":"recorded","alternatives":["alternative"],"kind":"template","identity_changed":true,"ambiguous":false})
            )
        );
        assert_eq!(
            json.pointer("/entries/1/machine_file/kind"),
            Some(&serde_json::json!("unknown"))
        );
        assert_eq!(
            json.pointer("/entries/1/machine_file/ambiguous"),
            Some(&serde_json::json!(true))
        );
        assert_eq!(
            json.pointer("/entries/1/machine_file/alternatives"),
            Some(&serde_json::json!(["selected", "alternative"]))
        );
        assert!(json.pointer("/entries/2/machine_file").is_none());
        assert!(!json.to_string().contains("secret-profile"));
        Ok(())
    }

    #[test]
    fn machine_projection_withholds_held_and_invalid_sides()
    -> Result<(), Box<dyn std::error::Error>> {
        let base = ENTRY.replace(r#""source":null"#, r#""source":{"raw":"source","shown":"source-shown"}"#)
            .replace(r#""update_state":false"#, r#""update_state":false,"alternatives":[{"raw":"held-source","shown":"held-alternative"}]"#);
        let held_repo = ENTRY
            .replace(r#""status":"clean""#, r#""status":"held-in-repo""#)
            .replace(
                r#""scope":"home","path":null"#,
                r#""scope":"repo_source","path":{"raw":"held-source","shown":"held-display"}"#,
            );
        let held_home = ENTRY.replace(r#""status":"clean""#, r#""status":"held-in-home""#);
        let invalid = base.replace("held-source", "../bad");
        let alias = base.replace(
            r#""raw":"source","shown":"source-shown""#,
            r#""raw":"held-source","shown":"source-shown""#,
        );
        let reply = protocol::parse_status(
            format!("{CONTEXT}\n{base}\n{held_repo}\n{REVIEW}\n{SUMMARY}\n{RESULT}\n").as_bytes(),
            Some(0),
        )?;
        assert!(
            project(&reply, false, 1)
                .entries
                .first()
                .is_some_and(|e| e.machine_file.is_none())
        );
        let reply = protocol::parse_status(
            format!("{CONTEXT}\n{base}\n{held_home}\n{REVIEW}\n{SUMMARY}\n{RESULT}\n").as_bytes(),
            Some(0),
        )?;
        assert!(
            project(&reply, false, 1)
                .entries
                .first()
                .is_some_and(|e| e.machine_file.is_none())
        );
        for entry in [invalid, alias] {
            let reply = protocol::parse_status(
                format!("{CONTEXT}\n{entry}\n{held_repo}\n{REVIEW}\n{SUMMARY}\n{RESULT}\n")
                    .as_bytes(),
                Some(0),
            )?;
            assert!(
                project(&reply, false, 1)
                    .entries
                    .first()
                    .is_some_and(|e| e.machine_file.is_none())
            );
        }
        let invalid_source = base.replace(
            r#""update_state":false"#,
            r#""invalid_sources":[{"source":{"raw":"../bad","shown":"bad"},"reason":{"raw":"private-reason","shown":"reason"}}],"update_state":false"#,
        );
        let reply = protocol::parse_status(
            format!("{CONTEXT}\n{invalid_source}\n{REVIEW}\n{SUMMARY}\n{RESULT}\n").as_bytes(),
            Some(0),
        )?;
        assert!(
            project(&reply, false, 1)
                .entries
                .first()
                .is_some_and(|e| e.machine_file.is_none())
        );
        Ok(())
    }

    #[test]
    fn held_occurrences_are_redacted_and_cannot_be_compared()
    -> Result<(), Box<dyn std::error::Error>> {
        let home = ENTRY.replace("\"status\":\"clean\"", "\"status\":\"held-in-home\"")
            .replace("\"update_state\":false", "\"update_state\":false,\"finding\":{\"rule\":\"github-token\",\"line\":1,\"place\":\"name\"}");
        let new = ENTRY.replace("\"status\":\"clean\"", "\"status\":\"held\"")
            .replace("private-key", "new-key")
            .replace("\"update_state\":false", "\"update_state\":false,\"hold\":\"content\",\"finding\":{\"rule\":\"github-token\",\"line\":2,\"place\":\"content\"}");
        let repo = ENTRY.replace("\"status\":\"clean\"", "\"status\":\"held-in-repo\"")
            .replace("private-key", "repo-key")
            .replace("\"scope\":\"home\",\"path\":null", "\"scope\":\"repo_source\",\"path\":{\"raw\":\"repo-key\",\"shown\":\"repo-display\"}")
            .replace("\"update_state\":false", "\"update_state\":false,\"finding\":{\"rule\":\"github-token\",\"line\":1,\"place\":\"target\"}");
        let summary = SUMMARY.replace(
            "\"held\":0,\"held_in_home\":0,\"held_in_repo\":0",
            "\"held\":1,\"held_in_home\":1,\"held_in_repo\":1",
        );
        let reply = protocol::parse_status(
            format!("{CONTEXT}\n{ENTRY}\n{home}\n{new}\n{repo}\n{REVIEW}\n{summary}\n{RESULT}\n")
                .as_bytes(),
            Some(0),
        )?;
        let view = project(&reply, false, 1);
        assert_eq!(
            (
                view.held_counts.new_files,
                view.held_counts.home_edits,
                view.held_counts.repo_edits
            ),
            (1, 1, 1)
        );
        assert_eq!(
            view.entries.first().map(|e| e.key.as_str()),
            Some("[redacted filename]")
        );
        assert_eq!(
            view.entries.get(1).map(|e| e.key.as_str()),
            Some("[redacted filename]")
        );
        assert_eq!(
            view.entries
                .get(2)
                .and_then(|e| e.hold.as_ref())
                .and_then(|h| h.location.as_deref()),
            Some("line 2")
        );
        assert_eq!(
            view.entries.get(3).map(|e| e.key.as_str()),
            Some("repo-display")
        );
        assert_eq!(
            view.entries
                .get(3)
                .and_then(|e| e.hold.as_ref())
                .and_then(|h| h.location.as_deref()),
            Some("symlink target")
        );
        let inner = Inner {
            reply: Some(reply),
            generation: 1,
            ..Inner::default()
        };
        for index in 0..4 {
            assert!(select(&inner, &format!("1:{index}")).is_err());
        }
        Ok(())
    }

    #[test]
    fn unknown_hold_metadata_is_not_projected() -> Result<(), Box<dyn std::error::Error>> {
        let entry = ENTRY.replace("\"status\":\"clean\"", "\"status\":\"held\"")
            .replace("\"update_state\":false", "\"update_state\":false,\"hold\":\"hostile-token\",\"finding\":{\"rule\":\"hostile-rule\",\"line\":2,\"place\":\"hostile-place\"}");
        let reply = protocol::parse_status(
            format!("{CONTEXT}\n{entry}\n{REVIEW}\n{SUMMARY}\n{RESULT}\n").as_bytes(),
            Some(0),
        )?;
        let json = serde_json::to_string(&project(&reply, false, 1))?;
        assert!(!json.contains("hostile"));
        assert!(json.contains("[redacted filename]"));
        Ok(())
    }

    #[test]
    fn projection_is_display_only_and_preserves_occurrences()
    -> Result<(), Box<dyn std::error::Error>> {
        let view = project(&report()?, true, 1);
        assert!(view.success);
        assert_eq!(view.entries.len(), 2);
        assert_eq!(
            view.entries.first().map(|entry| entry.key.as_str()),
            Some("é<img src=x onerror=alert(1)>")
        );
        assert_eq!(
            view.entries.get(1).map(|entry| entry.id.as_str()),
            Some("1:1")
        );
        let json = serde_json::to_string(&view)?;
        for secret in [
            "private-raw-home",
            "private-raw-repo",
            "private-key",
            "private-repo-id",
            "private-host",
            "0123456789abcdef",
            "0600",
        ] {
            assert!(!json.contains(secret));
        }
        Ok(())
    }

    #[test]
    fn error_context_and_recovery_remain_distinct() -> Result<(), Box<dyn std::error::Error>> {
        let result = r#"{"record":"result","outcome":"error","exit_code":4,"error_kind":"in_progress","recovery":[{"kind":"command","argv":["private-recovery-arg"],"shown":["shown-recovery"],"when":null}]}"#;
        let reply = protocol::parse_status(format!("{CONTEXT}\n{result}\n").as_bytes(), Some(4))?;
        let view = project(&reply, true, 1);
        assert!(!view.success);
        assert!(view.context.is_some());
        assert!(view.entries.is_empty());
        assert!(matches!(
            reply,
            protocol::StatusReply::Error {
                recovery: Some(_),
                ..
            }
        ));
        let json = serde_json::to_string(&view)?;
        assert!(!json.contains("private-recovery-arg"));
        assert!(!json.contains("shown-recovery"));
        assert!(
            protocol::parse_status(format!("{CONTEXT}\n{result}\n").as_bytes(), Some(0)).is_err()
        );
        Ok(())
    }

    #[test]
    fn failures_and_cancelled_late_results_clear_state_and_keep_notice()
    -> Result<(), Box<dyn std::error::Error>> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut inner = Inner {
            active: Some(cancelled.clone()),
            reply: Some(report()?),
            review: None,
            generation: 0,
            review_sequence: 0,
            inspection_sequence: 0,
            recovery: None,
            diff_receipts: HashSet::new(),
            setup: None,
            key_recovery: Vec::new(),
        };
        for message in [
            "Could not start the dotman companion",
            "Invalid status response",
        ] {
            let view = inner.finish(Err(message), &cancelled, true);
            assert!(
                !view.success
                    && view.key_stripped
                    && view.context.is_none()
                    && view.entries.is_empty()
            );
            assert!(inner.reply.is_none());
            inner.reply = Some(report()?);
        }
        cancelled.store(true, Ordering::Release);
        let view = inner.finish(Ok(report()?), &cancelled, true);
        assert!(
            !view.success && view.key_stripped && view.context.is_none() && view.entries.is_empty()
        );
        assert_eq!(view.message, "Status read cancelled");
        assert!(inner.reply.is_none() && inner.active.is_none());
        let view = inner.finish(Err("Could not cancel status"), &cancelled, true);
        assert_eq!(view.message, "Could not cancel status");
        Ok(())
    }

    #[test]
    fn selection_ids_are_generation_bound_and_denial_never_starts()
    -> Result<(), Box<dyn std::error::Error>> {
        let inner = Inner {
            active: None,
            reply: Some(report()?),
            review: None,
            generation: 7,
            review_sequence: 0,
            inspection_sequence: 0,
            recovery: None,
            diff_receipts: HashSet::new(),
            setup: None,
            key_recovery: Vec::new(),
        };
        assert!(select(&inner, "7:0").is_ok());
        assert!(select(&inner, "7:1").is_ok());
        for id in ["6:0", "7:2", "7:00", "7:-1", "7:0:0"] {
            assert!(select(&inner, id).is_err());
        }
        let cancelled = AtomicBool::new(false);
        let called = AtomicBool::new(false);
        let start = || {
            called.store(true, Ordering::Release);
            Ok(())
        };
        assert!(start_if_approved(&cancelled, false, start).is_err());
        assert!(!called.load(Ordering::Acquire));
        cancelled.store(true, Ordering::Release);
        assert!(start_if_approved(&cancelled, true, start).is_err());
        assert!(!called.load(Ordering::Acquire));
        Ok(())
    }

    #[test]
    fn late_cancel_invalidates_completed_report_and_future_diff()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = AppState::new(false);
        {
            let mut inner = state.inner.lock().map_err(|_| "lock")?;
            inner.reply = Some(report()?);
            inner.generation = 7;
            assert!(select(&inner, "7:0").is_ok());
            drop(inner);
        }
        state.stop()?;
        let inner = state.inner.lock().map_err(|_| "lock")?;
        assert!(inner.reply.is_none());
        assert!(select(&inner, "7:0").is_err());
        drop(inner);
        Ok(())
    }

    #[test]
    fn diff_error_retains_cli_recovery_without_projecting_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let result = r#"{"record":"result","outcome":"error","exit_code":4,"error_kind":"other","recovery":[{"kind":"command","argv":["private-recovery-arg"],"shown":["shown-recovery"],"when":null}]}"#;
        let diff_context = CONTEXT.replace(r#""command":"status""#, r#""command":"diff""#);
        let mut inner = Inner {
            active: None,
            reply: Some(report()?),
            review: None,
            generation: 7,
            review_sequence: 0,
            inspection_sequence: 0,
            recovery: None,
            diff_receipts: HashSet::new(),
            setup: None,
            key_recovery: Vec::new(),
        };
        let selection = select(&inner, "7:0")?;
        let reply = protocol::parse_diff(
            format!("{diff_context}\n{result}\n").as_bytes(),
            Some(4),
            &selection,
        )?;
        let view = finish_diff(&mut inner, "7:0", Ok(reply));
        assert!(matches!(view.outcome, DiffOutcome::TerminalRecovery));
        assert!(select(&inner, "7:0").is_err());
        assert!(matches!(&inner.reply, Some(protocol::StatusReply::Error {
            context: Some(_), error_kind: Some(kind), recovery: Some(_), ..
        }) if kind == "other"));
        let json = serde_json::to_string(&view)?;
        assert!(json.contains("terminal_recovery"));
        assert!(!json.contains("private-recovery-arg"));
        assert!(!json.contains("shown-recovery"));
        assert!(!json.contains("private-raw-home"));
        let ordinary = finish_diff(&mut inner, "7:0", Err("Diff failed"));
        assert!(matches!(ordinary.outcome, DiffOutcome::Error));
        Ok(())
    }

    fn review_input() -> String {
        let context = CONTEXT.replace(r#""command":"status""#, r#""command":"sync""#);
        let outgoing = r#"{"record":"outgoing","head":{"raw":"0123456789abcdef0123456789abcdef01234567","shown":"h"},"tracking_ref":null,"tracking_commit":null,"selected_target":"local","knowledge":"last_fetched","warnings":[]}"#;
        let stages = ["capture", "commit", "hooks"]
            .map(|stage| format!(r#"{{"record":"stage","stage":"{stage}","dry_run":true}}"#))
            .join("\n");
        let hooks = r#"{"record":"hooks","canonical":{"raw":"private-canonical","shown":"private-canonical"},"commands":[{"raw":"private-command","shown":"<script>"}],"pending_commands":[],"pending_keys":[],"approved":false,"unshown":true,"dry_run":true}"#;
        let message =
            r#"{"record":"message","message":{"raw":"private-message","shown":"shown message"}}"#;
        let edit = r#"{"record":"repo_edit","path":{"raw":"private-path","shown":"shown path"},"stored":{"state":"missing"},"observed":null,"basis":"index","remove_tracked_temporary":false}"#;
        format!("{context}\n{outgoing}\n{stages}\n{hooks}\n{message}\n{edit}\n{RESULT}\n")
    }

    #[test]
    fn mass_delete_confirmation_uses_retained_raw_paths_and_is_bounded()
    -> Result<(), Box<dyn std::error::Error>> {
        let protocol::StatusReply::Report(status) = report()? else {
            return Err("missing status".into());
        };
        let marker = r#"{"record":"mass_delete_review","paths":[{"raw":"private-path","shown":"wrong-path"}]}"#;
        let result =
            r#"{"record":"result","outcome":"error","exit_code":1,"error_kind":"mass_delete"}"#;
        let input = review_input().replace(RESULT, &format!("{marker}\n{result}"));
        let protocol::SyncReply::Review(review) =
            protocol::parse_sync(input.as_bytes(), Some(1), &status.context)?
        else {
            return Err("missing review".into());
        };
        let complete = CompleteReview::new(&review, &status.context)?;
        let text = mass_delete_modal("review".into(), &complete)?;
        assert!(text.contains("\n  private-path\n"));
        assert!(!text.contains("wrong-path"));
        assert!(mass_delete_modal("x".repeat(32 * 1024), &complete).is_err());
        Ok(())
    }

    fn capture_fixture() -> Result<(Inner, Box<protocol::SyncReview>), Box<dyn std::error::Error>> {
        let protocol::StatusReply::Report(mut status) = report()? else {
            return Err("missing status".into());
        };
        status.entries.truncate(1);
        let entry = status.entries.first_mut().ok_or("missing entry")?;
        entry.source = Some(protocol::StatusText {
            raw: "private-source".into(),
            shown: "shown source".into(),
        });
        entry.source_tuple = entry.r.clone();
        entry.physical_plain = Some(entry.source_tuple.clone());
        entry.h = entry.r.clone();
        entry.raw_h = entry.r.clone();
        let tuple = r#"{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}}"#;
        let capture = format!(
            r#"{{"key":{{"raw":"private-key","shown":"shown key"}},"source":{{"raw":"private-source","shown":"shown source"}},"scope":"home","logical_r":{tuple},"raw_h":{tuple},"portable_h":{tuple},"stored_before":{tuple},"stored_after":{tuple},"kept_mode":null,"repo_mode":null,"hypothetical":true,"encrypted":false}}"#
        );
        let protocol::SyncReply::Review(mut review) =
            protocol::parse_sync(review_input().as_bytes(), Some(0), &status.context)?
        else {
            return Err("missing review".into());
        };
        review.records.push(protocol::SyncRecord::Capture(Box::new(
            serde_json::from_str(&capture)?,
        )));
        Ok((
            Inner {
                reply: Some(protocol::StatusReply::Report(status)),
                generation: 7,
                ..Inner::default()
            },
            review,
        ))
    }

    #[test]
    fn held_repo_paths_block_alias_diffs_and_redact_review_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut inner, mut review) = capture_fixture()?;
        assert!(select(&inner, "7:0").is_ok());
        let Some(protocol::StatusReply::Report(status)) = &mut inner.reply else {
            return Err("missing status".into());
        };
        let repo_hold = ENTRY.replace("\"status\":\"clean\"", "\"status\":\"held-in-repo\"")
            .replace("\"raw\":\"private-key\"", "\"raw\":\"unrelated-lossy-key\"")
            .replace("\"scope\":\"home\",\"path\":null", "\"scope\":\"repo_source\",\"path\":{\"raw\":\"private-source\",\"shown\":\"sensitive-source\"}")
            .replace("\"update_state\":false", "\"update_state\":false,\"finding\":{\"rule\":\"github-token\",\"line\":1,\"place\":\"name\"}");
        let byte_hold = repo_hold
            .replace("unrelated-lossy-key", "b�")
            .replace("é<img src=x onerror=alert(1)>", "b�")
            .replace("private-source", "lossy-name")
            .replace(
                "{\"raw\":\"lossy-name\",\"shown\":\"sensitive-source\"}",
                "{\"bytes\":[98,255],\"shown\":\"sensitive-byte-name\"}",
            );
        status.entries.push(serde_json::from_str(&repo_hold)?);
        status.entries.push(serde_json::from_str(&byte_hold)?);
        let mut ordinary = status.entries.first().ok_or("missing entry")?.clone();
        ordinary.key.raw = "ordinary".into();
        ordinary.key.shown = "ordinary".into();
        ordinary.source = None;
        status.entries.push(ordinary);
        let capture_index = review.records.len().saturating_sub(1);
        let (_, complete_review) = capture_fixture()?;
        let byte_edit: protocol::SyncRecord = serde_json::from_str(
            r#"{"record":"repo_edit","path":{"bytes":[98,255],"shown":"leaked-byte-alias"},"stored":{"state":"missing"},"observed":null,"basis":"index","remove_tracked_temporary":false}"#,
        )?;
        review.records.push(byte_edit);
        review.records.push(serde_json::from_str(r#"{"record":"own","path":{"bytes":[98,255],"shown":"leaked-own-alias"},"stored":{"state":"missing"},"observed":null,"basis":"index","remove_tracked_temporary":false}"#)?);
        let byte_capture = r#"{"record":"capture","key":{"raw":"other","shown":"leaked-capture-key"},"source":{"bytes":[98,255],"shown":"leaked-capture-alias"},"scope":"home","logical_r":{"state":"missing"},"raw_h":{"state":"missing"},"portable_h":{"state":"missing"},"stored_before":{"state":"missing"},"stored_after":{"state":"missing"},"kept_mode":null,"repo_mode":null,"hypothetical":true,"encrypted":false}"#;
        review.records.push(serde_json::from_str(byte_capture)?);
        let undecided = r#"{"record":"undecided","key":{"raw":"private-source","shown":"leaked-undecided"},"scope":"repo_source","reason":"repo_held","pass":"capture","attention":true,"skip_next_pass":false}"#;
        review.records.push(serde_json::from_str(undecided)?);
        review.records.push(serde_json::from_str(r#"{"record":"refusal","key":{"raw":"private-source","shown":"leaked-refusal"},"kind":"held","source":null}"#)?);
        let view = project(inner.reply.as_ref().ok_or("missing status")?, false, 7);
        assert_eq!(
            view.entries.first().map(|entry| entry.key.as_str()),
            Some("[redacted filename]")
        );
        assert_eq!(
            view.entries.get(3).map(|entry| entry.key.as_str()),
            Some("ordinary")
        );
        assert!(select(&inner, "7:0").is_err());
        assert!(select(&inner, "7:3").is_ok());
        let view = finish_review(&mut inner, Ok(protocol::SyncReply::Review(complete_review)))?;
        let id = view.id.ok_or("missing review id")?;
        assert!(select(&inner, &format!("capture:{id}:{capture_index}")).is_err());
        let view = finish_review(&mut inner, Ok(protocol::SyncReply::Review(review)))?;
        let json = serde_json::to_string(&view)?;
        for forbidden in [
            "sensitive-source",
            "sensitive-byte-name",
            "leaked-byte-alias",
            "leaked-own-alias",
            "leaked-capture-key",
            "leaked-capture-alias",
            "leaked-undecided",
            "leaked-refusal",
            "shown source",
            "shown key",
        ] {
            assert!(!json.contains(forbidden), "{forbidden}");
        }
        assert!(json.contains("shown path"));
        Ok(())
    }

    fn action_fixture(row: &str) -> Result<Inner, Box<dyn std::error::Error>> {
        let (mut inner, _) = capture_fixture()?;
        let Some(protocol::StatusReply::Report(report)) = &mut inner.reply else {
            return Err("missing status".into());
        };
        let entry = report.entries.first_mut().ok_or("missing entry")?;
        entry.status = row.into();
        entry.row = Some(row.into());
        if row.starts_with("deleted-") {
            entry.h = protocol::StatusObservation::Missing;
            entry.raw_h = protocol::StatusObservation::Missing;
        }
        Ok(inner)
    }

    fn parsed_action_diff(inner: &Inner, id: &str) -> Result<protocol::DiffReply, String> {
        let selection = select(inner, id)?;
        let context = CONTEXT.replace(r#""command":"status""#, r#""command":"diff""#);
        let tuple = r#"{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}}"#;
        let home = if selection.entry.status.starts_with("deleted-") {
            r#"{"state":"missing"}"#
        } else {
            tuple
        };
        let diff = format!(
            r#"{{"record":"diff","key":{{"raw":"private-key","shown":"key"}},"scope":"home","path":{{"raw":"/private-raw-home/private-key","shown":"path"}},"status":"different","system_off":false,"source":{{"raw":"private-source","shown":"source"}},"r":{tuple},"source_tuple":{tuple},"physical_plain":{tuple},"h":{home},"raw_h":{home},"payload":{{"raw":"private diff","shown":"ignored"}}}}"#
        );
        protocol::parse_diff(
            format!("{context}\n{diff}\n{{\"record\":\"report\",\"count\":1}}\n{RESULT}\n")
                .as_bytes(),
            Some(0),
            &selection,
        )
    }

    #[test]
    fn action_choices_require_parsed_current_diff_and_are_display_only()
    -> Result<(), Box<dyn std::error::Error>> {
        for (row, expected) in [
            ("both-changed", r#"["keep_local","take_repo"]"#),
            ("existing", r#"["keep_local","take_repo"]"#),
            ("changed-in-repo", r#"["take_repo"]"#),
            ("deleted-locally", r#"["restore"]"#),
            ("deleted-changed-upstream", r#"["restore"]"#),
        ] {
            let mut inner = action_fixture(row)?;
            let reply = parsed_action_diff(&inner, "7:0")?;
            let view = finish_diff(&mut inner, "7:0", Ok(reply));
            assert_eq!(serde_json::to_string(&view.file_actions)?, expected);
            assert!(inner.diff_receipts.contains(&0));
            let json = serde_json::to_string(&view)?;
            assert!(!json.contains("private-key"));
            assert!(!json.contains("private-source"));
            assert!(!json.contains("0123456789abcdef"));
            let failed = finish_diff(&mut inner, "7:0", Err("Diff failed"));
            assert!(failed.file_actions.is_none());
            assert!(inner.diff_receipts.is_empty());
        }
        Ok(())
    }

    #[test]
    fn action_choices_fail_closed_for_holds_duplicates_and_unsupported()
    -> Result<(), Box<dyn std::error::Error>> {
        for case in 0..14 {
            let mut inner = action_fixture("both-changed")?;
            let Some(protocol::StatusReply::Report(report)) = &mut inner.reply else {
                return Err("missing status".into());
            };
            let entry = report.entries.first_mut().ok_or("missing entry")?;
            match case {
                0 => entry.encrypted = true,
                1 => entry.template = true,
                2 => entry.identity_changed = true,
                3 => entry.source = None,
                4 => entry.r = protocol::StatusObservation::Missing,
                5 => entry.raw_h = protocol::StatusObservation::Missing,
                6 => entry.status = "clean".into(),
                7 => {
                    let duplicate = entry.clone();
                    report.entries.push(duplicate);
                }
                8 | 9 => {
                    let mut hold = entry.clone();
                    hold.status = if case == 8 { "held" } else { "held-in-home" }.into();
                    report.entries.push(hold);
                }
                10 => report.summary.held_in_repo = 1,
                11 => {
                    let mut hold = entry.clone();
                    hold.status = "held-in-repo".into();
                    hold.key.raw = "unrelated-variant".into();
                    report.entries.push(hold);
                }
                12 => report.context.repo_id = None,
                _ => {
                    entry.system = true;
                    entry.scope = "system".into();
                }
            }
            assert!(file_actions(&inner, "7:0").is_none(), "case {case}");
        }
        let mut inner = action_fixture("both-changed")?;
        let reply = parsed_action_diff(&inner, "7:0")?;
        inner.generation = 8;
        let view = finish_diff(&mut inner, "7:0", Ok(reply));
        assert!(view.file_actions.is_none());
        assert!(inner.diff_receipts.is_empty());
        assert!(file_actions(&inner, "capture:7:1:0").is_none());
        Ok(())
    }

    #[test]
    fn capture_and_non_different_diff_preserve_old_view_without_authority()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut inner = action_fixture("both-changed")?;
        let (_, review) = capture_fixture()?;
        let review = finish_review(&mut inner, Ok(protocol::SyncReply::Review(review)))?;
        let id = review
            .items
            .iter()
            .find_map(|item| item.diff_id.as_deref())
            .ok_or("missing capture id")?;
        let reply = parsed_action_diff(&inner, id)?;
        let view = finish_diff(&mut inner, id, Ok(reply));
        assert!(view.success && view.text.is_some() && view.file_actions.is_none());
        assert!(inner.diff_receipts.is_empty());
        let view = finish_diff(
            &mut inner,
            "7:0",
            Ok(protocol::DiffReply::Report(protocol::DiffView {
                status: "equal".into(),
                system_off: false,
                text: None,
                line_edits: None,
            })),
        );
        assert!(view.success && view.text.is_none() && view.file_actions.is_none());
        assert!(inner.diff_receipts.is_empty());
        Ok(())
    }

    #[test]
    fn action_receipts_clear_on_cancel_refresh_and_recovery()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = AppState::new(false);
        {
            let mut inner = state.inner.lock().map_err(|_| "lock")?;
            *inner = action_fixture("both-changed")?;
            let reply = parsed_action_diff(&inner, "7:0")?;
            finish_diff(&mut inner, "7:0", Ok(reply));
            assert!(inner.diff_receipts.contains(&0));
            drop(inner);
        }
        state.stop()?;
        let mut inner = state.inner.lock().map_err(|_| "lock")?;
        assert!(inner.diff_receipts.is_empty());
        *inner = action_fixture("both-changed")?;
        inner.diff_receipts.insert(0);
        inner.finish(Err("failed"), &AtomicBool::new(false), false);
        assert!(inner.diff_receipts.is_empty());
        *inner = action_fixture("both-changed")?;
        inner.diff_receipts.insert(0);
        finish_diff(
            &mut inner,
            "7:0",
            Ok(protocol::DiffReply::Error {
                exit_code: 4,
                context: None,
                error_kind: None,
                recovery: None,
            }),
        );
        assert!(inner.diff_receipts.is_empty());
        drop(inner);
        Ok(())
    }

    fn answer(id: &str, action: FileAction) -> FileAnswer {
        FileAnswer {
            id: id.into(),
            action,
        }
    }

    fn authorize(inner: &mut Inner) -> Result<(), Box<dyn std::error::Error>> {
        let reply = parsed_action_diff(inner, "7:0")?;
        finish_diff(inner, "7:0", Ok(reply));
        Ok(())
    }

    #[test]
    fn apply_reserves_once_and_rejects_unreviewable_batches()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut inner = action_fixture("both-changed")?;
        authorize(&mut inner)?;
        for answers in [
            vec![],
            vec![answer("6:0", FileAction::TakeRepo)],
            vec![answer("7:0", FileAction::Restore)],
            vec![
                answer("7:0", FileAction::TakeRepo),
                answer("7:0", FileAction::KeepLocal),
            ],
            vec![
                answer("7:0", FileAction::TakeRepo),
                answer("7:1", FileAction::KeepLocal),
            ],
        ] {
            assert!(reserve_apply(&mut inner, &answers, Arc::new(AtomicBool::new(false))).is_err());
            assert!(inner.active.is_none());
            assert!(inner.diff_receipts.contains(&0));
        }
        let answers = [answer("7:0", FileAction::TakeRepo)];
        let reserved = reserve_apply(&mut inner, &answers, Arc::new(AtomicBool::new(false)))?;
        assert_eq!(reserved.choices.len(), 1);
        assert!(inner.diff_receipts.is_empty());
        assert!(inner.active.is_some());
        assert!(reserve_apply(&mut inner, &answers, Arc::new(AtomicBool::new(false))).is_err());
        inner.active = None;
        assert!(reserve_apply(&mut inner, &answers, Arc::new(AtomicBool::new(false))).is_err());
        assert!(serde_json::from_str::<FileAnswer>(r#"{"id":"7:0","action":"forget"}"#).is_err());
        assert!(
            serde_json::from_str::<FileAnswer>(
                r#"{"id":"7:0","action":"keep_local","extra":true}"#
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn changed_in_repo_requires_exact_action_and_current_receipt()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut inner = action_fixture("changed-in-repo")?;
        let cancelled = || Arc::new(AtomicBool::new(false));
        assert!(
            reserve_apply(
                &mut inner,
                &[answer("7:0", FileAction::TakeRepo)],
                cancelled()
            )
            .is_err()
        );
        authorize(&mut inner)?;
        for action in [FileAction::KeepLocal, FileAction::Restore] {
            assert!(reserve_apply(&mut inner, &[answer("7:0", action)], cancelled()).is_err());
        }
        inner.generation = 8;
        assert!(
            reserve_apply(
                &mut inner,
                &[answer("7:0", FileAction::TakeRepo)],
                cancelled()
            )
            .is_err()
        );
        let state = AppState::new(false);
        {
            let mut guard = state.inner.lock().map_err(|_| "lock")?;
            *guard = action_fixture("changed-in-repo")?;
            authorize(&mut guard)?;
            drop(guard);
        }
        state.stop()?;
        let guard = state.inner.lock().map_err(|_| "lock")?;
        assert!(guard.diff_receipts.is_empty());
        drop(guard);
        inner.generation = 7;
        let reservation = reserve_apply(
            &mut inner,
            &[answer("7:0", FileAction::TakeRepo)],
            cancelled(),
        )?;
        assert!(
            reservation
                .text
                .contains("Take repository backs up replaced bytes")
        );
        assert!(inner.diff_receipts.is_empty());
        Ok(())
    }

    fn run_apply_fixture(
        root: &std::path::Path,
        cli: &std::ffi::OsStr,
        args: &[&str],
        input: Option<&[u8]>,
    ) -> Result<std::process::Output, Box<dyn std::error::Error>> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let home = root.join("home");
        let mut child = Command::new(cli)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMPDIR", root)
            .env("DOTMAN_NON_INTERACTIVE", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(&home)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(input) = input {
            child.stdin.take().ok_or("stdin")?.write_all(input)?;
        }
        Ok(child.wait_with_output()?)
    }

    #[test]
    #[ignore = "requires DOTMAN_TEST_CLI pointing to the built CLI"]
    fn changed_in_repo_real_cli_write_backup_and_stale() -> Result<(), Box<dyn std::error::Error>> {
        struct Root(std::path::PathBuf);
        impl Drop for Root {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let path = std::env::temp_dir().join(format!("dotman-gui-apply-{}", std::process::id()));
        std::fs::create_dir(&path)?;
        let root = Root(path);
        for dir in ["home", "config", "data", "state", "cache", "runtime"] {
            std::fs::create_dir(root.0.join(dir))?;
        }
        let home = root.0.join("home");
        std::fs::write(
            home.join(".gitconfig"),
            "[user]\nname = Test\nemail = test@example.invalid\n",
        )?;
        let cli = std::env::var_os("DOTMAN_TEST_CLI").ok_or("missing DOTMAN_TEST_CLI")?;
        let run =
            |args: &[&str], input: Option<&[u8]>| run_apply_fixture(&root.0, &cli, args, input);
        let init = run(&["init"], None)?;
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        std::fs::write(home.join(".profile"), b"original\n")?;
        let add = run(&["add", ".profile"], None)?;
        assert!(
            add.status.success(),
            "{}",
            String::from_utf8_lossy(&add.stderr)
        );
        std::fs::write(root.0.join("data/dotman/.profile"), b"repository\n")?;
        let status = run(&["status", "--json"], None)?;
        let protocol::StatusReply::Report(report) =
            protocol::parse_status(&status.stdout, status.status.code())?
        else {
            return Err("status refused".into());
        };
        let index = report
            .entries
            .iter()
            .position(|entry| entry.key.raw == ".profile" && entry.status == "changed-in-repo")
            .ok_or("missing changed-in-repo")?;
        let mut inner = action_fixture("changed-in-repo")?;
        inner.reply = Some(protocol::StatusReply::Report(report));
        let id = format!("7:{index}");
        let selection = select(&inner, &id)?;
        let exact = format!(
            "--exact-repo={}",
            selection.context.repo_id.as_ref().ok_or("repo id")?.raw
        );
        let operand = home.join(".profile");
        let operand = operand.to_str().ok_or("operand")?;
        let diff = run(&["diff", "--json", &exact, "--", operand], None)?;
        let parsed = protocol::parse_diff(&diff.stdout, diff.status.code(), &selection)?;
        let view = finish_diff(&mut inner, &id, Ok(parsed));
        assert_eq!(
            serde_json::to_string(&view.file_actions)?,
            r#"["take_repo"]"#
        );
        let reservation = reserve_apply(
            &mut inner,
            &[answer(&id, FileAction::TakeRepo)],
            Arc::new(AtomicBool::new(false)),
        )?;
        let document = reservation.reviewed()?.encode()?;
        std::fs::write(home.join(".profile"), b"stale\n")?;
        let args = [
            "apply",
            "--json",
            "--decide-from",
            "-",
            "--no-hooks",
            "--",
            operand,
        ];
        let stale = run(&args, Some(&document))?;
        assert!(!stale.status.success());
        assert_eq!(std::fs::read(home.join(".profile"))?, b"stale\n");
        std::fs::write(home.join(".profile"), b"original\n")?;
        let done = run(&args, Some(&document))?;
        assert!(
            done.status.success(),
            "{} {}",
            String::from_utf8_lossy(&done.stdout),
            String::from_utf8_lossy(&done.stderr)
        );
        assert_eq!(std::fs::read(home.join(".profile"))?, b"repository\n");
        let backups = std::fs::read_dir(root.0.join("state/dotman/backups"))?
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read(entry.path().join(".profile")).ok())
            .collect::<Vec<_>>();
        assert_eq!(backups, vec![b"original\n".to_vec()]);
        Ok(())
    }

    #[test]
    fn apply_rechecks_holds_and_confirmation_is_escaped() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut inner = action_fixture("both-changed")?;
        authorize(&mut inner)?;
        let Some(protocol::StatusReply::Report(report)) = &mut inner.reply else {
            return Err("missing status".into());
        };
        report.context.repo.shown = "repo\n\"danger\u{202e}".into();
        let entry = report.entries.first_mut().ok_or("missing entry")?;
        entry.key.shown = "file\nTake repository: forged".into();
        let mut hold = entry.clone();
        hold.status = "held-in-home".into();
        report.entries.push(hold);
        let answers = [answer("7:0", FileAction::KeepLocal)];
        assert!(reserve_apply(&mut inner, &answers, Arc::new(AtomicBool::new(false))).is_err());
        let Some(protocol::StatusReply::Report(report)) = &mut inner.reply else {
            return Err("missing status".into());
        };
        report.entries.pop();
        let reservation = reserve_apply(&mut inner, &answers, Arc::new(AtomicBool::new(false)))?;
        assert!(reservation.text.contains(r#"repo\u{a}\"danger\u{202e}"#));
        assert!(
            reservation
                .text
                .contains(r"file\u{a}Take repository: forged")
        );
        assert!(
            reservation
                .text
                .contains("No hooks or system files; no commit or push")
        );
        Ok(())
    }

    #[test]
    fn apply_outcome_projects_actual_and_terminal_without_refresh()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::process::ExitStatusExt;
        let mut inner = action_fixture("both-changed")?;
        let selection = select(&inner, "7:0")?;
        let context = CONTEXT.replace(r#""command":"status""#, r#""command":"apply""#);
        let events = r#"{"record":"stage","stage":"apply","dry_run":false}
{"record":"resolved","key":{"raw":"private-key","shown":"display-key"},"scope":"home","effect":"written","hypothetical":false}
{"record":"stage","stage":"hooks","dry_run":false}
{"record":"hook","command":null,"outcome":"disabled","exit_code":null}"#;
        let receipt = runner::Receipt {
            stdout: format!("{context}\n{events}\n{RESULT}\n").into_bytes(),
            stderr: vec![],
            status: std::process::ExitStatus::from_raw(0),
        };
        let view = finish_apply_writer(&mut inner, &receipt, &selection.context);
        assert_eq!(view.outcome, "done");
        assert!(view.refresh);
        assert!(
            view.items
                .iter()
                .any(|item| item.text.contains("display-key"))
        );
        let result = r#"{"record":"result","outcome":"error","exit_code":4,"error_kind":"in_progress","recovery":[{"kind":"command","argv":["git","status"],"shown":["git","status"],"when":null}]}"#;
        let receipt = runner::Receipt {
            stdout: format!("{context}\n{result}\n").into_bytes(),
            stderr: vec![],
            status: std::process::ExitStatus::from_raw(4 << 8),
        };
        let view = finish_apply_writer(&mut inner, &receipt, &selection.context);
        assert_eq!(view.outcome, "terminal_recovery");
        assert!(!view.refresh);
        assert!(view.recovery.is_some());
        Ok(())
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "One sequence verifies the receipt lifecycle."
    )]
    fn selected_lines_are_review_bound_and_revoked() -> Result<(), Box<dyn std::error::Error>> {
        let (mut inner, review) = capture_fixture()?;
        let view = finish_review(&mut inner, Ok(protocol::SyncReply::Review(review)))?;
        let diff_id = view
            .items
            .iter()
            .find_map(|item| item.diff_id.clone())
            .ok_or("missing diff")?;
        let review_id = view.id.ok_or("missing review")?;
        let make = || {
            protocol::DiffReply::Report(protocol::DiffView {
                status: "different".into(),
                system_off: false,
                text: Some("diff".into()),
                line_edits: Some(vec![protocol::LineEdit {
                    id: 7,
                    old_line: Some(2),
                    new_line: Some(2),
                    before: Some("old\n".into()),
                    after: Some("new\u{202e}\n".into()),
                }]),
            })
        };
        let ordinary = finish_diff(&mut inner, "7:0", Ok(make()));
        assert!(ordinary.line_edits.is_none());
        let result = finish_diff(&mut inner, &diff_id, Ok(make()));
        let line = result
            .line_edits
            .as_ref()
            .and_then(|lines| lines.first())
            .ok_or("missing line")?;
        assert!(line.after.as_ref().is_some_and(|s| s.contains("\\u{202e}")));
        let token = line.id.clone();
        inner.active = Some(Arc::new(AtomicBool::new(false)));
        assert!(selected_review(&inner, &review_id, std::slice::from_ref(&token), None).is_err());
        inner.active = None;
        let reopened = finish_diff(&mut inner, &diff_id, Ok(make()));
        assert_eq!(
            reopened
                .line_edits
                .as_ref()
                .and_then(|lines| lines.first())
                .map(|line| &line.id),
            Some(&token)
        );
        let selected = selected_review(&inner, &review_id, std::slice::from_ref(&token), None)?;
        assert_eq!(selected.lines.get("private-key"), Some(&vec![7]));
        assert_eq!(selected.target, writer_outcome::SelectedTarget::Local);
        assert!(selected.text.contains("replacement"));
        assert!(selected.text.contains("local commit only"));
        assert!(!selected.text.contains("branch ancestry"));
        assert_eq!(result.selected_push, Some(false));
        assert!(selected_review(&inner, &review_id, &[], None).is_err());
        assert!(
            selected_review(&inner, &review_id, &[token.clone(), token.clone()], None).is_err()
        );
        assert!(selected_review(&inner, &review_id, &["forged".into()], None).is_err());
        finish_diff(&mut inner, &diff_id, Err("changed"));
        let reopened = finish_diff(&mut inner, &diff_id, Ok(make()));
        let new_token = reopened
            .line_edits
            .as_ref()
            .and_then(|lines| lines.first())
            .map(|line| line.id.clone())
            .ok_or("missing reopened line")?;
        assert_ne!(new_token, token);
        assert!(selected_review(&inner, &review_id, std::slice::from_ref(&token), None).is_err());
        let protocol::DiffReply::Report(mut changed) = make() else {
            return Err("missing diff".into());
        };
        if let Some(edit) = changed
            .line_edits
            .as_mut()
            .and_then(|lines| lines.first_mut())
        {
            edit.after = Some("changed\n".into());
        }
        let changed = finish_diff(
            &mut inner,
            &diff_id,
            Ok(protocol::DiffReply::Report(changed)),
        );
        let changed_token = changed
            .line_edits
            .as_ref()
            .and_then(|lines| lines.first())
            .map(|line| line.id.clone())
            .ok_or("missing changed line")?;
        assert_ne!(changed_token, new_token);
        if let Some(review) = &mut inner.review
            && let Some(protocol::SyncRecord::Outgoing {
                selected_target, ..
            }) = review.review.records.first_mut()
        {
            *selected_target = None;
        }
        assert!(
            selected_review(
                &inner,
                &review_id,
                std::slice::from_ref(&changed_token),
                None
            )
            .is_err()
        );
        assert!(
            finish_diff(&mut inner, &diff_id, Ok(make()))
                .line_edits
                .is_none()
        );
        finish_diff(&mut inner, &diff_id, Err("changed"));
        assert!(selected_review(&inner, &review_id, &[token], None).is_err());
        Ok(())
    }

    fn selected_test_diff() -> protocol::DiffReply {
        protocol::DiffReply::Report(protocol::DiffView {
            status: "different".into(),
            system_off: false,
            text: Some("diff".into()),
            line_edits: Some(vec![protocol::LineEdit {
                id: 1,
                old_line: Some(1),
                new_line: Some(1),
                before: Some("old\n".into()),
                after: Some("new\n".into()),
            }]),
        })
    }

    #[test]
    fn selected_dialog_uses_only_selected_mode_and_target() -> Result<(), Box<dyn std::error::Error>>
    {
        let (mut inner, review) = capture_fixture()?;
        let view = finish_review(&mut inner, Ok(protocol::SyncReply::Review(review)))?;
        let id = view
            .items
            .iter()
            .find_map(|item| item.diff_id.clone())
            .ok_or("diff")?;
        let review_id = view.id.ok_or("review")?;
        let local = finish_diff(&mut inner, &id, Ok(selected_test_diff()));
        assert_eq!(local.selected_push, Some(false));
        let token = local
            .line_edits
            .and_then(|lines| lines.into_iter().next())
            .ok_or("line")?
            .id;
        let review = inner.review.as_mut().ok_or("review")?;
        for record in &mut review.review.records {
            if let protocol::SyncRecord::Capture(capture) = record {
                capture.key.shown = "x".repeat(33 * 1024);
            }
        }
        assert!(modal_text(&review.review, None, false).is_err());
        let selected = selected_review(&inner, &review_id, std::slice::from_ref(&token), None)?;
        assert!(selected.text.contains("local commit only"));
        let (_, _, text) = inner.consume_review_mode(&review_id, None, Some(selected.text))?;
        assert!(text.contains("local commit only"));
        Ok(())
    }

    #[test]
    fn selected_target_controls_diff_capability_and_copy() -> Result<(), Box<dyn std::error::Error>>
    {
        let (mut inner, review) = capture_fixture()?;
        let view = finish_review(&mut inner, Ok(protocol::SyncReply::Review(review)))?;
        let id = view
            .items
            .iter()
            .find_map(|item| item.diff_id.clone())
            .ok_or("diff")?;
        let review_id = view.id.ok_or("review")?;
        for binding in [None, Some("invalid".to_owned())] {
            let review = inner.review.as_mut().ok_or("review")?;
            let Some(protocol::SyncRecord::Outgoing {
                selected_target, ..
            }) = review.review.records.first_mut()
            else {
                return Err("outgoing".into());
            };
            *selected_target = binding;
            let result = finish_diff(&mut inner, &id, Ok(selected_test_diff()));
            assert!(result.line_edits.is_none() && result.selected_push.is_none());
        }
        let review = inner.review.as_mut().ok_or("review")?;
        let Some(protocol::SyncRecord::Outgoing {
            selected_target, ..
        }) = review.review.records.first_mut()
        else {
            return Err("outgoing".into());
        };
        *selected_target = Some(format!("git-{}", "a".repeat(40)));
        let result = finish_diff(&mut inner, &id, Ok(selected_test_diff()));
        assert_eq!(result.selected_push, Some(true));
        let token = result
            .line_edits
            .and_then(|lines| lines.into_iter().next())
            .ok_or("line")?
            .id;
        let selected = selected_review(&inner, &review_id, &[token], None)?;
        assert!(selected.text.contains("existing local branch ancestry"));
        assert!(
            selected
                .text
                .contains("Last-fetched counts are not the live remote")
        );
        assert!(!selected.text.contains("local commit only"));
        let local_error = selected_error_message(writer_outcome::SelectedTarget::Local, &[]);
        assert!(!local_error.contains("push"));
        Ok(())
    }

    #[test]
    fn selected_dialog_rejects_multiple_captures() -> Result<(), Box<dyn std::error::Error>> {
        let (mut inner, mut review) = capture_fixture()?;
        let Some(protocol::StatusReply::Report(status)) = &mut inner.reply else {
            return Err("status".into());
        };
        let mut entry = status.entries.first().ok_or("entry")?.clone();
        entry.key.raw = "second".into();
        entry.source.as_mut().ok_or("source")?.raw = "second-source".into();
        status.entries.push(entry);
        let tuple = r#"{"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0600"}}"#;
        let capture = format!(
            r#"{{"key":{{"raw":"second","shown":"second"}},"source":{{"raw":"second-source","shown":"second-source"}},"scope":"home","logical_r":{tuple},"raw_h":{tuple},"portable_h":{tuple},"stored_before":{tuple},"stored_after":{tuple},"kept_mode":null,"repo_mode":null,"hypothetical":true,"encrypted":false}}"#
        );
        review.records.push(protocol::SyncRecord::Capture(Box::new(
            serde_json::from_str(&capture)?,
        )));
        let view = finish_review(&mut inner, Ok(protocol::SyncReply::Review(review)))?;
        let review_id = view.id.ok_or("review")?;
        let ids: Vec<_> = view
            .items
            .iter()
            .filter_map(|item| item.diff_id.clone())
            .collect();
        assert_eq!(ids.len(), 2);
        let mut tokens = Vec::new();
        for id in ids {
            let view = finish_diff(&mut inner, &id, Ok(selected_test_diff()));
            tokens.push(
                view.line_edits
                    .and_then(|lines| lines.into_iter().next())
                    .ok_or("line")?
                    .id,
            );
        }
        assert!(selected_review(&inner, &review_id, &tokens, None).is_err());
        assert!(inner.review.is_some());
        Ok(())
    }

    #[test]
    fn capture_diff_is_bound_to_retained_review_and_status()
    -> Result<(), Box<dyn std::error::Error>> {
        let (mut inner, review) = capture_fixture()?;
        let view = finish_review(&mut inner, Ok(protocol::SyncReply::Review(review)))?;
        let id = view
            .items
            .iter()
            .find_map(|item| item.diff_id.clone())
            .ok_or("missing diff id")?;
        let review_id = view.id.ok_or("missing review id")?;
        let json = serde_json::to_string(&view.items)?;
        assert!(!json.contains("private-key"));
        assert!(!json.contains("private-source"));
        assert!(!json.contains("0123456789abcdef"));
        assert!(
            view.items
                .iter()
                .filter(|item| item.group != "Captures")
                .all(|item| item.diff_id.is_none())
        );
        let capture_selection = select(&inner, &id)?;
        let status_selection = select(&inner, "7:0")?;
        assert_eq!(
            capture_selection.entry.key.raw,
            status_selection.entry.key.raw
        );
        assert_eq!(capture_selection.entry.r, status_selection.entry.r);
        assert!(select(&inner, &format!("{id}0")).is_err());
        assert!(select(&inner, "capture:7:1:00").is_err());
        let failure = finish_diff(
            &mut inner,
            &id,
            Err("Diff changed or returned an invalid response"),
        );
        assert!(failure.text.is_none());
        assert!(select(&inner, &id).is_ok());
        inner.discard_review(&review_id);
        assert!(select(&inner, &id).is_err());
        assert!(select(&inner, "7:0").is_ok());
        let (_, next) = capture_fixture()?;
        let next_view = finish_review(&mut inner, Ok(protocol::SyncReply::Review(next)))?;
        let next_id = next_view
            .items
            .iter()
            .find_map(|item| item.diff_id.clone())
            .ok_or("missing next id")?;
        assert_ne!(id, next_id);
        assert!(select(&inner, &id).is_err());
        assert!(select(&inner, &next_id).is_ok());
        let next_review = next_view.id.ok_or("missing next review")?;
        inner.consume_review(&next_review, None)?;
        assert!(select(&inner, &next_id).is_err());
        Ok(())
    }

    #[test]
    fn capture_diff_refuses_incomplete_changed_duplicate_and_unsupported()
    -> Result<(), Box<dyn std::error::Error>> {
        for case in 0..13 {
            let (mut inner, mut review) = capture_fixture()?;
            let Some(protocol::StatusReply::Report(status)) = &mut inner.reply else {
                return Err("missing status".into());
            };
            let entry = status.entries.first_mut().ok_or("missing entry")?;
            match case {
                0 => review.exit_code = 3,
                1 => entry.raw_h = protocol::StatusObservation::Missing,
                2 => entry.encrypted = true,
                3 => entry.template = true,
                4 => entry.status = "new".into(),
                5 => {
                    let duplicate = entry.clone();
                    status.entries.push(duplicate);
                }
                6 => review.context.repo.path = "/other".into(),
                7 => {
                    let protocol::SyncRecord::Capture(capture) =
                        review.records.last_mut().ok_or("missing capture")?
                    else {
                        return Err("missing capture".into());
                    };
                    capture.encrypted = true;
                }
                8 => entry.h = protocol::StatusObservation::Missing,
                9 => entry.physical_plain = None,
                10 => entry.r = protocol::StatusObservation::Missing,
                11 => {
                    let Some(protocol::StatusObservation::Present { tuple }) =
                        &mut entry.physical_plain
                    else {
                        return Err("missing physical tuple".into());
                    };
                    tuple.mode = "0644".into();
                }
                _ => entry.source.as_mut().ok_or("missing source")?.raw = "other".into(),
            }
            let view = finish_review(&mut inner, Ok(protocol::SyncReply::Review(review)))?;
            assert!(
                view.items.iter().all(|item| item.diff_id.is_none()),
                "case {case}"
            );
        }
        let (mut inner, review) = capture_fixture()?;
        let view = finish_review(&mut inner, Ok(protocol::SyncReply::Review(review)))?;
        let id = view
            .items
            .iter()
            .find_map(|item| item.diff_id.clone())
            .ok_or("missing id")?;
        let Some(protocol::StatusReply::Report(status)) = &mut inner.reply else {
            return Err("missing status".into());
        };
        status
            .entries
            .push(status.entries.first().ok_or("missing entry")?.clone());
        assert!(select(&inner, &id).is_err());
        let (mut inner, mut review) = capture_fixture()?;
        let (_, mut duplicate) = capture_fixture()?;
        review
            .records
            .push(duplicate.records.pop().ok_or("missing capture")?);
        let view = finish_review(&mut inner, Ok(protocol::SyncReply::Review(review)))?;
        assert!(view.id.is_none());
        assert!(view.items.iter().all(|item| item.diff_id.is_none()));
        Ok(())
    }

    #[test]
    fn review_projection_excludes_raw_fields_and_cancel_discards_review()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = AppState::new(false);
        let mut inner = state.inner.lock().map_err(|_| "lock")?;
        inner.reply = Some(report()?);
        inner.generation = 7;
        let selected = select(&inner, "7:0")?;
        let input = review_input();
        let protocol::SyncReply::Review(review) =
            protocol::parse_sync(input.as_bytes(), Some(0), &selected.context)?
        else {
            return Err("missing review".into());
        };
        assert_review_projection(&review)?;
        let first = finish_review(&mut inner, Ok(protocol::SyncReply::Review(review)))?;
        let first_id = first.id.ok_or("review not retained")?;
        let second = finish_review(
            &mut inner,
            protocol::parse_sync(input.as_bytes(), Some(0), &selected.context)
                .map_err(|_| "invalid review"),
        )?;
        let second_id = second.id.ok_or("review not retained")?;
        assert_ne!(first_id, second_id);
        assert!(inner.consume_review(&first_id, None).is_err());
        let (_, _, modal) = inner.consume_review(&second_id, None)?;
        assert!(modal.contains("Repository: /repo"));
        assert!(modal.contains("Suggested commit message (CLI may recompute it; each line follows |):\n| private-message\n"));
        assert!(modal.contains("Repository change: shown path"));
        assert!(!modal.contains("private-path"));
        assert!(inner.consume_review(&second_id, None).is_err());
        assert_oversized_review_is_not_consumed(&mut inner, &input, &selected.context)?;
        let second = finish_review(
            &mut inner,
            protocol::parse_sync(input.as_bytes(), Some(0), &selected.context)
                .map_err(|_| "invalid review"),
        )?;
        let second_id = second.id.ok_or("review not retained")?;
        inner.discard_review(&first_id);
        assert_eq!(
            inner.review.as_ref().map(|r| r.id.as_str()),
            Some(second_id.as_str())
        );
        inner.discard_review(&second_id);
        assert!(inner.review.is_none());
        assert!(select(&inner, "7:0").is_ok());
        let protocol::SyncReply::Review(mut partial) =
            protocol::parse_sync(input.as_bytes(), Some(0), &selected.context)?
        else {
            return Err("missing review".into());
        };
        partial.exit_code = 3;
        assert!(
            finish_review(&mut inner, Ok(protocol::SyncReply::Review(partial)))?
                .id
                .is_none()
        );
        let early = input.replace(r#"{"record":"stage","stage":"hooks","dry_run":true}"#, "");
        let early_reply = protocol::parse_sync(early.as_bytes(), Some(0), &selected.context);
        if let Ok(reply) = early_reply {
            assert!(finish_review(&mut inner, Ok(reply))?.id.is_none());
        }
        inner.reply = None;
        let no_status = finish_review(
            &mut inner,
            protocol::parse_sync(input.as_bytes(), Some(0), &selected.context)
                .map_err(|_| "invalid review"),
        )?;
        assert!(no_status.id.is_none());
        inner.reply = Some(report()?);
        inner.review_sequence = u64::MAX;
        assert!(
            finish_review(
                &mut inner,
                protocol::parse_sync(input.as_bytes(), Some(0), &selected.context)
                    .map_err(|_| "invalid review"),
            )
            .is_err()
        );
        assert!(inner.review.is_none());
        inner.review_sequence = 2;
        let third = finish_review(
            &mut inner,
            protocol::parse_sync(input.as_bytes(), Some(0), &selected.context)
                .map_err(|_| "invalid review"),
        )?;
        assert!(third.id.is_some());
        inner.active = Some(Arc::new(AtomicBool::new(false)));
        let cancelled = inner.active.clone().ok_or("missing active read")?;
        drop(inner);
        state.stop()?;
        assert!(cancelled.load(Ordering::Acquire));
        let inner = state.inner.lock().map_err(|_| "lock")?;
        assert!(inner.review.is_none());
        assert!(select(&inner, "7:0").is_err());
        drop(inner);
        Ok(())
    }

    fn assert_oversized_review_is_not_consumed(
        inner: &mut Inner,
        input: &str,
        context: &protocol::StatusContext,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let oversized = input.replace("shown path", &"x".repeat(33 * 1024));
        if let Ok(protocol::SyncReply::Review(oversized)) =
            protocol::parse_sync(oversized.as_bytes(), Some(0), context)
        {
            let view = finish_review(inner, Ok(protocol::SyncReply::Review(oversized)))?;
            let id = view.id.ok_or("oversized review not retained")?;
            assert!(inner.consume_review(&id, None).is_err());
            assert_eq!(
                inner.review.as_ref().map(|r| r.id.as_str()),
                Some(id.as_str())
            );
        }
        Ok(())
    }

    #[test]
    fn editable_message_is_bound_to_complete_review_and_confirmation()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut inner = Inner {
            reply: Some(report()?),
            ..Inner::default()
        };
        let protocol::StatusReply::Report(status) = report()? else {
            return Err("missing status".into());
        };
        let input = review_input();
        let make = |input: &str| protocol::parse_sync(input.as_bytes(), Some(0), &status.context);
        let view = finish_review(&mut inner, make(&input).map_err(|_| "invalid review"))?;
        assert_eq!(view.commit_message.as_deref(), Some("private-message"));
        let id = view.id.ok_or("missing id")?;
        let draft = runner::CommitMessage::new("--approve-hooks <é> \"\nnext".into())?;
        assert!(inner.consume_review("stale", Some(&draft)).is_err());
        let (_, _, modal) = inner.consume_review(&id, Some(&draft))?;
        assert!(modal.contains("Edited commit message (each line follows |):\n| --approve-hooks <é> \"\n| next\nEnd of commit message\n"));
        assert!(modal.find("End of commit message") < modal.find("Repository change:"));
        assert!(!modal.contains("private-message"));
        assert!(inner.consume_review(&id, Some(&draft)).is_err());

        for case in 0..3 {
            let protocol::SyncReply::Review(mut review) = make(&input)? else {
                return Err("missing review".into());
            };
            match case {
                0 => review
                    .records
                    .retain(|r| !matches!(r, protocol::SyncRecord::Message { .. })),
                1 => review.records.push(protocol::SyncRecord::Message {
                    message: protocol::StatusText {
                        raw: "other".into(),
                        shown: "other".into(),
                    },
                }),
                _ => {
                    for record in &mut review.records {
                        if let protocol::SyncRecord::Message { message } = record {
                            message.raw = "bad\u{202e}".into();
                        }
                    }
                }
            }
            assert!(editable_message(&review).is_none());
            let view = finish_review(&mut inner, Ok(protocol::SyncReply::Review(review)))?;
            assert!(view.commit_message.is_none());
            if let Some(id) = view.id {
                assert!(inner.consume_review(&id, Some(&draft)).is_err());
                assert!(inner.consume_review(&id, None).is_ok());
            }
        }
        let protocol::SyncReply::Review(mut partial) = make(&input)? else {
            return Err("missing review".into());
        };
        partial.exit_code = 3;
        let view = finish_review(&mut inner, Ok(protocol::SyncReply::Review(partial)))?;
        assert!(view.id.is_none() && view.commit_message.is_none());
        Ok(())
    }

    #[test]
    fn selected_writer_dispatch_requires_reviewed_target_stages()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::process::ExitStatusExt;
        let protocol::StatusReply::Report(status) = report()? else {
            return Err("missing status".into());
        };
        let context = CONTEXT.replace(r#""command":"status""#, r#""command":"sync""#);
        let capture = r#"{"record":"stage","stage":"capture","dry_run":false}"#;
        let commit = r#"{"record":"stage","stage":"commit","dry_run":false}"#;
        let push = r#"{"record":"stage","stage":"push","dry_run":false}"#;
        let done = format!("{context}\n{capture}\n{commit}\n{RESULT}\n");
        let make = |bytes: &[u8], code| runner::Receipt {
            stdout: bytes.to_vec(),
            stderr: Vec::new(),
            status: std::process::ExitStatus::from_raw(code << 8),
        };
        let mut inner = Inner::default();
        assert_eq!(
            finish_writer_mode(
                &mut inner,
                &make(done.as_bytes(), 0),
                &status.context,
                Some(writer_outcome::SelectedTarget::Local)
            )
            .outcome,
            "done"
        );
        assert_eq!(
            finish_writer_mode(
                &mut inner,
                &make(done.as_bytes(), 0),
                &status.context,
                Some(writer_outcome::SelectedTarget::Push)
            )
            .outcome,
            "error"
        );
        let error = r#"{"record":"result","outcome":"error","exit_code":1,"error_kind":"push"}"#;
        let failed = format!("{context}\n{capture}\n{commit}\n{push}\n{error}\n");
        let view = finish_writer_mode(
            &mut inner,
            &make(failed.as_bytes(), 1),
            &status.context,
            Some(writer_outcome::SelectedTarget::Push),
        );
        assert_eq!(view.outcome, "error");
        assert!(view.message.contains("local commit was created"));
        assert!(view.message.contains("remote outcome may be uncertain"));
        let before_commit = format!("{context}\n{capture}\n{commit}\n{error}\n");
        let view = finish_writer_mode(
            &mut inner,
            &make(before_commit.as_bytes(), 1),
            &status.context,
            Some(writer_outcome::SelectedTarget::Local),
        );
        assert!(view.message.contains("local commit may exist"));
        assert!(!view.message.contains("push"));
        assert!(!view.message.contains("was created"));
        Ok(())
    }

    #[test]
    fn writer_exit_four_and_malformed_success_are_not_success()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::process::ExitStatusExt;
        let mut inner = Inner::default();
        let protocol::StatusReply::Report(status) = report()? else {
            return Err("missing status".into());
        };
        let receipt = |code, stdout: &[u8]| runner::Receipt {
            stdout: stdout.to_vec(),
            stderr: b"private stderr".to_vec(),
            status: std::process::ExitStatus::from_raw(code << 8),
        };
        let failed = finish_writer(&mut inner, &receipt(0, b"broken"), &status.context);
        assert_eq!(failed.outcome, "error");
        assert!(failed.refresh);
        let recovery = finish_writer(&mut inner, &receipt(4, b"broken"), &status.context);
        assert_eq!(recovery.outcome, "terminal_recovery");
        assert!(!recovery.refresh);
        assert!(!serde_json::to_string(&recovery)?.contains("private stderr"));
        let result = r#"{"record":"result","outcome":"error","exit_code":4,"error_kind":"in_progress","recovery":[{"kind":"command","argv":["private-recovery"],"shown":["shown-recovery"],"when":null}]}"#;
        let recovery = finish_writer(
            &mut inner,
            &receipt(4, format!("{result}\n").as_bytes()),
            &status.context,
        );
        assert_eq!(recovery.outcome, "terminal_recovery");
        assert!(inner.recovery.is_some());
        assert!(!serde_json::to_string(&recovery)?.contains("shown-recovery"));
        Ok(())
    }

    #[test]
    fn writer_recovery_is_bound_and_survives_failed_status()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::process::ExitStatusExt;
        let protocol::StatusReply::Report(status) = report()? else {
            return Err("missing status".into());
        };
        let sync = CONTEXT.replace(r#""command":"status""#, r#""command":"sync""#);
        let result = r#"{"record":"result","outcome":"error","exit_code":4,"error_kind":"pull_conflict","recovery":[{"kind":"instruction","text":{"raw":"resolve\nnow","shown":"ignored"}},{"kind":"command","argv":["git","a'b"],"shown":["hidden","hidden"],"when":"after stop"}]}"#;
        let receipt = |stdout: &str| runner::Receipt {
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"private stderr".to_vec(),
            status: std::process::ExitStatus::from_raw(4 << 8),
        };
        let input = format!("{sync}\n{result}\n");
        let mut inner = Inner::default();
        let view = finish_writer(&mut inner, &receipt(&input), &status.context);
        assert_eq!(view.outcome, "terminal_recovery");
        assert!(!view.refresh);
        let json = serde_json::to_value(&view)?;
        let recovery_text = |path| json.pointer(path).and_then(serde_json::Value::as_str);
        assert_eq!(recovery_text("/recovery/0/text"), Some("resolve\\u{a}now"));
        assert_eq!(recovery_text("/recovery/1/shell"), Some("'git' 'a'\\''b'"));
        assert_eq!(recovery_text("/recovery/1/condition"), Some("after stop"));
        let serialized = serde_json::to_string(&view)?;
        for forbidden in ["private stderr", "\"argv\"", "\"shown\"", "private-raw"] {
            assert!(!serialized.contains(forbidden));
        }
        let null_id = sync.replace(
            r#""repo_id":{"raw":"private-repo-id","shown":"id"}"#,
            r#""repo_id":null"#,
        );
        let guard = result.replace("pull_conflict", "in_progress");
        let valid_guard = format!("{null_id}\n{guard}\n");
        assert!(
            finish_writer(&mut inner, &receipt(&valid_guard), &status.context)
                .recovery
                .is_some()
        );
        for bad in [
            format!("{null_id}\n{result}\n"),
            format!("{}\n{guard}\n", sync.replace("private-repo-id", "other-id")),
            format!(
                "{}\n{guard}\n",
                null_id.replace("/private-raw-home", "/other")
            ),
            format!(
                "{}\n{guard}\n",
                null_id.replace("private-host", "other-host")
            ),
            format!(
                "{}\n{guard}\n",
                null_id.replace(
                    r#""age_key_file":null"#,
                    r#""age_key_file":{"raw":"/other","shown":"/other"}"#
                )
            ),
            format!(
                "{}\n{guard}\n",
                sync.replace(r#""schema_version":1"#, r#""schema_version":2"#)
            ),
            format!("{guard}\n"),
            format!("{sync}\n{}\n", result.replace("pull_conflict", "other")),
            "broken".to_owned(),
        ] {
            let view = finish_writer(&mut inner, &receipt(&bad), &status.context);
            assert!(view.recovery.is_none(), "{bad}");
            assert!(!view.refresh);
        }
        let _ = finish_writer(&mut inner, &receipt(&valid_guard), &status.context);
        let cancelled = AtomicBool::new(false);
        inner.finish(Err("Status failed"), &cancelled, false);
        assert!(inner.recovery.is_some());
        let error = protocol::parse_status(format!("{CONTEXT}\n{guard}\n").as_bytes(), Some(4))?;
        inner.finish(Ok(error), &cancelled, false);
        assert!(inner.recovery.is_some());
        inner.finish(Ok(report()?), &cancelled, false);
        assert!(inner.recovery.is_none());
        Ok(())
    }

    #[test]
    fn writer_progress_maps_only_fixed_stage_strings_and_ignores_delivery_failure()
    -> Result<(), Box<dyn std::error::Error>> {
        use writer_outcome::ProgressStage;
        let stages = [
            (ProgressStage::Capture, "capture"),
            (ProgressStage::Commit, "commit"),
            (ProgressStage::Pull, "pull"),
            (ProgressStage::Apply, "apply"),
            (ProgressStage::Reencrypt, "reencrypt"),
            (ProgressStage::Hooks, "hooks"),
            (ProgressStage::Push, "push"),
            (ProgressStage::BlockedByRepoHold, "blocked_by_repo_hold"),
        ];
        assert_eq!(serde_json::to_string(&WriterStage::Started)?, "\"started\"");
        for (stage, expected) in stages {
            assert_eq!(
                serde_json::to_string(&WriterStage::from(stage))?,
                format!("\"{expected}\"")
            );
        }
        let channel =
            tauri::ipc::Channel::new(|_| Err(std::io::Error::other("disconnected").into()));
        assert!(channel.send(WriterStage::Started).is_err());
        Ok(())
    }

    fn assert_review_projection(review: &protocol::SyncReview) -> Result<(), serde_json::Error> {
        let json = serde_json::to_string(&project_review(review, None, &HeldPaths::default()))?;
        assert!(json.contains("<script>"));
        assert!(json.contains("shown message"));
        assert!(json.contains("shown path"));
        for secret in [
            "private-canonical",
            "private-command",
            "private-message",
            "private-path",
            "private-head",
        ] {
            assert!(!json.contains(secret));
        }
        Ok(())
    }
}
