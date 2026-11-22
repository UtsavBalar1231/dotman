use super::{AppState, Inner, WriterView, run_status, wait_apply, writer_view};
use dotman_gui::{protocol, runner};
use gtk::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

fn observation(path: &Path) -> Result<Option<Observation>, &'static str> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("Cannot inspect identity context"),
    };
    if !meta.is_file() {
        return Err("Expected a regular file");
    }
    Ok(Some(Observation {
        dev: meta.dev(),
        ino: meta.ino(),
        size: meta.size(),
        mtime: meta.mtime(),
        mtime_nsec: meta.mtime_nsec(),
        ctime: meta.ctime(),
        ctime_nsec: meta.ctime_nsec(),
    }))
}

fn path_value(path: &Path) -> Value {
    json!({"bytes": path.as_os_str().as_bytes()})
}

fn display_path(path: &Path) -> Result<String, &'static str> {
    let bytes = path.as_os_str().as_bytes();
    if !path.is_absolute() || bytes.is_empty() || bytes.len() > 4096 || bytes.contains(&0) {
        return Err("Path cannot be shown safely");
    }
    let mut shown = String::new();
    if let Ok(text) = std::str::from_utf8(bytes) {
        shown.push('"');
        for character in text.chars() {
            if runner::unsafe_display_char(character) {
                shown.extend(character.escape_unicode());
            } else if matches!(character, '"' | '\\') {
                shown.push('\\');
                shown.push(character);
            } else {
                shown.push(character);
            }
            if shown.len() > 16_384 {
                return Err("Path cannot be shown safely");
            }
        }
        shown.push('"');
    } else {
        shown.push_str("Unix path bytes (hex): ");
        for byte in bytes {
            write!(&mut shown, "{byte:02x}").map_err(|_| "Path cannot be shown safely")?;
        }
    }
    Ok(shown)
}

fn install_modal(intent: &Intent) -> Result<String, &'static str> {
    let identity = display_path(&intent.context.identity.path)?;
    let source = intent
        .source
        .as_ref()
        .map(|(path, _)| display_path(path))
        .transpose()?;
    let operation = if source.is_some() { "Import" } else { "Create" };
    let source = source.map_or_else(String::new, |path| format!("\nSource: {path}"));
    Ok(format!(
        "{operation} an age identity at:\n{identity}{source}\n\nThe destination must be absent. Repository recipients will not change unless separately approved.\n\nContinue?"
    ))
}

fn grant_modal(intent: &Intent, recipients: &[String]) -> Result<String, &'static str> {
    let repo = display_path(&intent.context.repo.path)?;
    let config = display_path(&intent.context.repo.path.join(".dotman.toml"))?;
    let recovery = display_path(&recovery_parent(intent)?)?;
    Ok(format!(
        "Add this public recipient to repository settings?\n\nRepository: {repo}\nSettings: {config}\nRecipient: {}\n\nRecovery parent: {recovery}\nA settings change retains a reviewed-byte copy and the previous settings inode in a private recovery directory. Both may contain previous settings. The installed identity remains if you decline or this step fails.",
        recipients.join("\n")
    ))
}

fn grant_after_approval<T>(approved: bool, run: impl FnOnce() -> T) -> Option<T> {
    approved.then(run)
}

struct Intent {
    context: protocol::StatusContext,
    config: Option<Observation>,
    parent_dev: u64,
    parent_ino: u64,
    source: Option<(PathBuf, Observation)>,
}

impl Intent {
    fn new(
        context: protocol::StatusContext,
        source: Option<PathBuf>,
    ) -> Result<Self, &'static str> {
        if context.repo_id.as_ref().is_none_or(|id| id.raw.is_empty())
            || context.repo_config.path.as_os_str()
                != context.repo.path.join(".dotman.toml").as_os_str()
        {
            return Err("Repository identity is unavailable");
        }
        let parent = context
            .identity
            .path
            .parent()
            .ok_or("Identity parent is unavailable")?;
        let meta = fs::symlink_metadata(parent).map_err(|_| "Identity parent is unavailable")?;
        if !meta.is_dir()
            || meta.mode() & 0o077 != 0
            || observation(&context.identity.path)?.is_some()
        {
            return Err("Identity destination is not absent or its parent is not private");
        }
        let config = observation(&context.config.path)?;
        let source = source
            .map(|path| {
                let stat = observation(&path)?.ok_or("Selected identity is unavailable")?;
                Ok::<_, &'static str>((path, stat))
            })
            .transpose()?;
        Ok(Self {
            context,
            config,
            parent_dev: meta.dev(),
            parent_ino: meta.ino(),
            source,
        })
    }

    fn recheck(&self) -> Result<(), &'static str> {
        let current = Self::new(
            self.context.clone(),
            self.source.as_ref().map(|(path, _)| path.clone()),
        )?;
        if current.config != self.config
            || current.parent_dev != self.parent_dev
            || current.parent_ino != self.parent_ino
            || current.source != self.source
        {
            return Err("Identity review changed");
        }
        Ok(())
    }

    fn envelope(&self, operation: &Value) -> Result<Vec<u8>, &'static str> {
        let id = self
            .context
            .repo_id
            .as_ref()
            .ok_or("Repository identity is unavailable")?;
        serde_json::to_vec(&json!({
            "schema_version": 1, "repo_id": id.raw,
            "home": path_value(&self.context.home.path),
            "repo": path_value(&self.context.repo.path),
            "config": path_value(&self.context.config.path),
            "identity": path_value(&self.context.identity.path),
            "config_observation": self.config,
            "parent_dev": self.parent_dev, "parent_ino": self.parent_ino,
            "operation": operation
        }))
        .map_err(|_| "Cannot encode identity request")
    }

    fn install(&self) -> Result<Vec<u8>, &'static str> {
        let operation = match &self.source {
            Some((path, observation)) => {
                json!({"kind":"import","source":path_value(path), "source_observation":observation})
            }
            None => json!({"kind":"create"}),
        };
        self.envelope(&operation)
    }

    fn preflight(&self) -> Result<Vec<u8>, &'static str> {
        self.envelope(&json!({"kind":"grant_preflight"}))
    }

    fn grant(&self, token: &GrantPreflight) -> Result<Vec<u8>, &'static str> {
        self.envelope(&json!({
            "kind": "grant",
            "identity_observation": token.identity_observation,
            "repo_config_observation": token.repo_config_observation,
            "repo_config_digest": token.repo_config_digest,
            "repo_dev": token.repo_dev,
            "repo_ino": token.repo_ino,
            "recovery_parent": path_value(&token.recovery_parent.path()?),
            "recovery_parent_dev": token.recovery_parent_dev,
            "recovery_parent_ino": token.recovery_parent_ino,
            "recipients": token.recipients
        }))
    }
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum WirePath {
    Raw { raw: String },
    Bytes { bytes: Vec<u8> },
}

impl WirePath {
    fn matches(&self, path: &Path) -> bool {
        match self {
            Self::Raw { raw } => raw.as_bytes() == path.as_os_str().as_bytes(),
            Self::Bytes { bytes } => bytes.as_slice() == path.as_os_str().as_bytes(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantPreflight {
    record: String,
    schema_version: u32,
    repo_id: String,
    home: WirePath,
    repo: WirePath,
    config: WirePath,
    identity: WirePath,
    #[serde(deserialize_with = "required_nullable")]
    config_observation: Option<Observation>,
    parent_dev: u64,
    parent_ino: u64,
    repo_dev: u64,
    repo_ino: u64,
    identity_observation: Observation,
    repo_config_observation: Observation,
    repo_config_digest: String,
    recipients: Vec<String>,
    recipient_present: bool,
    backup_consequence: String,
    recovery_parent: ReportedPath,
    recovery_parent_dev: u64,
    recovery_parent_ino: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportedPath {
    raw: Option<String>,
    bytes: Option<Vec<u8>>,
    shown: String,
}

impl ReportedPath {
    fn path(&self) -> Result<PathBuf, &'static str> {
        let bytes = match (&self.raw, &self.bytes) {
            (Some(raw), None) => raw.as_bytes().to_vec(),
            (None, Some(bytes)) => bytes.clone(),
            _ => return Err("Invalid recovery path"),
        };
        if bytes.is_empty() || bytes.len() > 4096 || bytes.contains(&0) || self.shown.len() > 16_384
        {
            return Err("Invalid recovery path");
        }
        Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
}

const BACKUP_CONSEQUENCE: &str = "A settings change retains a reviewed-byte copy and the displaced settings inode in a private recovery directory under this state directory.";

fn recovery_parent(intent: &Intent) -> Result<PathBuf, &'static str> {
    if intent.context.state.path.file_name() != Some(std::ffi::OsStr::new("state.toml")) {
        return Err("Invalid state path");
    }
    let parent = intent
        .context
        .state
        .path
        .parent()
        .ok_or("Invalid state path")?;
    display_path(parent)?;
    Ok(parent.to_path_buf())
}

fn required_nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    Option::deserialize(deserializer)
}

fn parse_preflight(
    bytes: &[u8],
    status: Option<i32>,
    intent: &Intent,
    recipients: &[String],
) -> Result<GrantPreflight, &'static str> {
    if status != Some(0) || bytes.len() > 4096 {
        return Err("Recipient review could not be verified");
    }
    let token: GrantPreflight =
        serde_json::from_slice(bytes).map_err(|_| "Invalid recipient review")?;
    let repo =
        fs::symlink_metadata(&intent.context.repo.path).map_err(|_| "Repository is unavailable")?;
    let parent = fs::symlink_metadata(
        intent
            .context
            .identity
            .path
            .parent()
            .ok_or("Identity parent is unavailable")?,
    )
    .map_err(|_| "Identity parent is unavailable")?;
    let recovery = fs::symlink_metadata(recovery_parent(intent)?)
        .map_err(|_| "Recovery parent is unavailable")?;
    if token.record != "grant_preflight"
        || token.schema_version != 1
        || intent
            .context
            .repo_id
            .as_ref()
            .is_none_or(|id| token.repo_id != id.raw)
        || !token.home.matches(&intent.context.home.path)
        || !token.repo.matches(&intent.context.repo.path)
        || !token.config.matches(&intent.context.config.path)
        || !token.identity.matches(&intent.context.identity.path)
        || token.config_observation != intent.config
        || observation(&intent.context.config.path)? != token.config_observation
        || (token.parent_dev, token.parent_ino) != (intent.parent_dev, intent.parent_ino)
        || !parent.is_dir()
        || (parent.dev(), parent.ino()) != (token.parent_dev, token.parent_ino)
        || !repo.is_dir()
        || (repo.dev(), repo.ino()) != (token.repo_dev, token.repo_ino)
        || observation(&intent.context.identity.path)?.as_ref() != Some(&token.identity_observation)
        || observation(&intent.context.repo_config.path)?.as_ref()
            != Some(&token.repo_config_observation)
        || token.repo_config_digest.len() != 32
        || !token
            .repo_config_digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || token.recipients != recipients
        || token.recipients.len() != 1
        || token.backup_consequence != BACKUP_CONSEQUENCE
        || token.recovery_parent.path()?.as_os_str() != recovery_parent(intent)?.as_os_str()
        || !recovery.is_dir()
        || (recovery.dev(), recovery.ino())
            != (token.recovery_parent_dev, token.recovery_parent_ino)
    {
        return Err("Recipient review changed");
    }
    Ok(token)
}

fn postflight_matches(before: &GrantPreflight, after: &GrantPreflight, changed: bool) -> bool {
    after.recipient_present
        && after.identity_observation == before.identity_observation
        && (after.repo_dev, after.repo_ino) == (before.repo_dev, before.repo_ino)
        && (after.recovery_parent_dev, after.recovery_parent_ino)
            == (before.recovery_parent_dev, before.recovery_parent_ino)
        && if changed {
            after.repo_config_digest != before.repo_config_digest
        } else {
            after.repo_config_digest == before.repo_config_digest
                && after.repo_config_observation == before.repo_config_observation
        }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    record: String,
    recipients: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsRecovery {
    record: String,
    phase: String,
    directory: ReportedPath,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RecoveryPhase {
    Prepared,
    Published,
}

fn recovery_path(line: &[u8], parent: &Path) -> Option<(PathBuf, RecoveryPhase)> {
    let record: SettingsRecovery = serde_json::from_slice(line).ok()?;
    let phase = match record.phase.as_str() {
        "prepared" => RecoveryPhase::Prepared,
        "published" => RecoveryPhase::Published,
        _ => return None,
    };
    let path = record.directory.path().ok()?;
    let leaf = path.file_name()?.as_bytes();
    let suffix = leaf.strip_prefix(b"key-grant-")?;
    if record.record != "settings_recovery"
        || suffix.len() != 6
        || !suffix.iter().all(u8::is_ascii_alphanumeric)
        || path.parent()?.as_os_str() != parent.as_os_str()
        || parent.join(std::ffi::OsStr::from_bytes(leaf)).as_os_str() != path.as_os_str()
        || display_path(&path).is_err()
    {
        return None;
    }
    Some((path, phase))
}

fn grant_receipt(
    bytes: &[u8],
    status: Option<i32>,
    parent: &Path,
    recipients: &[String],
) -> (bool, Option<(PathBuf, RecoveryPhase)>) {
    if bytes.len() > 4096 {
        return (false, None);
    }
    let mut lines = bytes.split(|b| *b == b'\n');
    let Some(first) = lines.next() else {
        return (false, None);
    };
    let mut recovery =
        recovery_path(first, parent).filter(|(_, phase)| *phase == RecoveryPhase::Prepared);
    if !bytes.ends_with(b"\n") {
        return (false, recovery);
    }
    let result = if let Some((path, _)) = &recovery {
        let published = lines.next().and_then(|line| recovery_path(line, parent));
        if !published.as_ref().is_some_and(|(published, phase)| {
            published == path && *phase == RecoveryPhase::Published
        }) {
            return (false, recovery);
        }
        recovery = published;
        lines.next()
    } else {
        Some(first)
    };
    let valid = result.is_some_and(|line| {
        receipt(line, status, "recipients_added").is_ok_and(|found| found == recipients)
    }) && lines.next() == Some(&[][..])
        && lines.next().is_none();
    (valid, recovery)
}

fn receipt(bytes: &[u8], status: Option<i32>, record: &str) -> Result<Vec<String>, &'static str> {
    if status != Some(0) || bytes.len() > 4096 {
        return Err("Identity operation could not be verified");
    }
    let value: Receipt = serde_json::from_slice(bytes).map_err(|_| "Invalid identity receipt")?;
    if value.record != record
        || value.recipients.len() != 1
        || !value.recipients.iter().all(|s| {
            s.len() == 62
                && s.starts_with("age1")
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
    {
        return Err("Invalid identity receipt");
    }
    Ok(value.recipients)
}

fn reserve(
    inner: &mut Inner,
    id: &str,
    cancelled: Arc<AtomicBool>,
) -> Result<protocol::StatusContext, &'static str> {
    if inner.active.is_some()
        || inner.recovery.is_some()
        || inner.generation == u64::MAX
        || id != format!("key:{}", inner.generation)
    {
        return Err("Identity review is stale or another operation is running");
    }
    let Some(protocol::StatusReply::Report(report)) = &inner.reply else {
        return Err("Status is unavailable");
    };
    let context = report.context.clone();
    if context.repo_id.is_none() {
        return Err("Repository identity is unavailable");
    }
    inner.reply = None;
    inner.review = None;
    inner.diff_receipts.clear();
    inner.generation = inner.generation.saturating_add(1);
    inner.active = Some(cancelled);
    Ok(context)
}

fn choose(
    window: &tauri::WebviewWindow,
    cancelled: Arc<AtomicBool>,
) -> Result<Option<PathBuf>, &'static str> {
    if window.label() != "main" {
        return Err("Cannot open identity chooser");
    }
    let (sender, receiver) = mpsc::channel();
    window
        .with_webview(move |platform| {
            let Some(parent) = platform
                .inner()
                .toplevel()
                .and_then(|w| w.downcast::<gtk::Window>().ok())
            else {
                return;
            };
            crate::key_chooser::choose_identity(&parent, cancelled, move |path| {
                let _ = sender.send(path);
            });
        })
        .map_err(|_| "Cannot open identity chooser")?;
    receiver.recv().map_err(|_| "Cannot open identity chooser")
}

fn same_context(a: &protocol::StatusContext, b: &protocol::StatusContext) -> bool {
    a.home.path == b.home.path
        && a.repo.path == b.repo.path
        && a.config.path == b.config.path
        && a.identity.path == b.identity.path
        && a.repo_config.path == b.repo_config.path
        && a.repo_id == b.repo_id
}

fn current(context: &protocol::StatusContext, cancelled: &AtomicBool) -> bool {
    matches!(run_status(cancelled), Ok(protocol::StatusReply::Report(report)) if same_context(context, &report.context))
}

fn execute_raw(
    intent: &Intent,
    bytes: Vec<u8>,
    cancelled: &AtomicBool,
    uncertain: &AtomicBool,
    shared: &Mutex<Inner>,
) -> Result<runner::Receipt, &'static str> {
    let guard = shared.lock().map_err(|_| "Status state unavailable")?;
    if cancelled.load(Ordering::Acquire) {
        return Err("Identity operation cancelled");
    }
    uncertain.store(true, Ordering::Release);
    let mut run = runner::start_key(&intent.context.home.path, bytes)
        .map_err(|_| "Cannot start identity operation")?;
    drop(guard);
    let response = wait_apply(&mut run, cancelled).map_err(|_| {
        uncertain.store(true, Ordering::Release);
        "Identity operation ended without a usable result"
    })?;
    uncertain.store(false, Ordering::Release);
    Ok(response)
}

fn execute(
    intent: &Intent,
    bytes: Vec<u8>,
    cancelled: &AtomicBool,
    uncertain: &AtomicBool,
    shared: &Mutex<Inner>,
    record: &str,
) -> Result<Vec<String>, &'static str> {
    let response = execute_raw(intent, bytes, cancelled, uncertain, shared)?;
    receipt(&response.stdout, response.status.code(), record)
}

fn finish_grant(
    intent: &Intent,
    token: &GrantPreflight,
    response: &runner::Receipt,
    window: &tauri::WebviewWindow,
    cancelled: &Arc<AtomicBool>,
    uncertain: &AtomicBool,
    shared: &Mutex<Inner>,
) -> WriterView {
    let Ok(parent) = recovery_parent(intent) else {
        return writer_view(
            "attention",
            "Identity installed. Recipient result could not be verified; check status.",
            true,
        );
    };
    let (valid, recovery) = grant_receipt(
        &response.stdout,
        response.status.code(),
        &parent,
        &token.recipients,
    );
    let valid = valid && token.recipient_present == recovery.is_none();
    if let Some((path, _)) = &recovery {
        if let Ok(mut inner) = shared.lock() {
            inner.key_recovery.push(path.clone());
        } else {
            return writer_view(
                "attention",
                "Identity installed. Recovery information could not be retained; check the terminal.",
                false,
            );
        }
    }
    if !valid {
        if let Some((path, phase)) =
            recovery.and_then(|(path, phase)| display_path(&path).ok().map(|path| (path, phase)))
        {
            let detail = if phase == RecoveryPhase::Published {
                "The CLI reported publication: candidate was the displaced inode, but another process may still change it."
            } else {
                "The CLI reported preparation only: candidate may be unpublished or the displaced inode."
            };
            let text = format!(
                "Recovery directory: {path}\n\n{detail} Retain reviewed and candidate; inspect before restoring anything. No automatic retry or rollback was attempted."
            );
            let _ = crate::confirm::ask(
                window,
                "Settings recovery retained",
                "Acknowledge recovery",
                text,
                cancelled.clone(),
            );
        }
        return writer_view(
            "attention",
            "Identity installed. Recipient update could not be verified; recovery data may be retained. Check the terminal.",
            true,
        );
    }
    let Ok(request) = intent.preflight() else {
        return writer_view(
            "attention",
            "Identity installed. Recipient update completed but fresh review was unavailable.",
            true,
        );
    };
    let fresh = execute_raw(intent, request, cancelled, uncertain, shared)
        .ok()
        .and_then(|reply| {
            parse_preflight(
                &reply.stdout,
                reply.status.code(),
                intent,
                &token.recipients,
            )
            .ok()
        });
    let verified = fresh.is_some_and(|fresh| postflight_matches(token, &fresh, recovery.is_some()))
        && current(&intent.context, cancelled);
    if verified {
        writer_view(
            "done",
            "Identity installed; recipient update completed and settings refreshed.",
            true,
        )
    } else {
        writer_view(
            "attention",
            "Identity installed. Recipient update completed but fresh settings review could not be verified.",
            !uncertain.load(Ordering::Acquire),
        )
    }
}

#[tauri::command]
pub async fn run_key(
    id: String,
    action: String,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<WriterView, String> {
    if !matches!(action.as_str(), "create" | "import") || id.len() > 64 {
        return Err("Invalid identity action".into());
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let context = {
        let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
        reserve(&mut inner, &id, cancelled.clone())?
    };
    let worker_flag = cancelled.clone();
    let shared = state.inner.clone();
    let uncertain = Arc::new(AtomicBool::new(false));
    let worker_uncertain = uncertain.clone();
    let worker = tauri::async_runtime::spawn_blocking(move || {
        let source = if action == "import" {
            match choose(&window, worker_flag.clone()) {
                Ok(Some(path)) => Some(path),
                _ => return writer_view("cancelled", "Identity import cancelled.", true),
            }
        } else { None };
        let Ok(intent) = Intent::new(context, source) else {
            return writer_view("attention", "Identity destination or selection is unavailable; refresh and check the paths.", true);
        };
        let Ok(text) = install_modal(&intent) else {
            return writer_view("attention", "Identity paths cannot be shown safely; nothing was started.", true);
        };
        let approved = crate::confirm::ask(&window, "Install age identity?", "Install identity", text, worker_flag.clone()).unwrap_or(false);
        if !approved || worker_flag.load(Ordering::Acquire) {
            return writer_view("declined", "Identity installation was not approved.", true);
        }
        if intent.recheck().is_err() || !current(&intent.context, &worker_flag) {
            return writer_view("attention", "Identity review changed; nothing was started.", true);
        }
        let Ok(request) = intent.install() else { return writer_view("error", "Identity request could not be prepared.", true); };
        let Ok(recipients) = execute(&intent, request, &worker_flag, &worker_uncertain, &shared, "identity") else {
            return writer_view("attention", "Identity installation could not be verified; it may have succeeded. Check status before trying again.", !worker_uncertain.load(Ordering::Acquire));
        };
        if worker_flag.load(Ordering::Acquire) || !current(&intent.context, &worker_flag) {
            return writer_view("attention", "Identity installed. Recipient addition was not started; refresh to reconcile.", true);
        }
        let Ok(preflight) = intent.preflight() else {
            return writer_view("attention", "Identity installed. Recipient review is unavailable; refresh to reconcile.", true);
        };
        let Ok(response) = execute_raw(&intent, preflight, &worker_flag, &worker_uncertain, &shared) else {
            return writer_view("attention", "Identity installed. Recipient review could not be verified; check status to reconcile.", !worker_uncertain.load(Ordering::Acquire));
        };
        let Ok(token) = parse_preflight(&response.stdout, response.status.code(), &intent, &recipients) else {
            return writer_view("attention", "Identity installed. Recipient review could not be verified; refresh to reconcile.", true);
        };
        let Ok(text) = grant_modal(&intent, &token.recipients) else {
            return writer_view("attention", "Identity installed. Repository paths cannot be shown safely; recipient addition was not started.", true);
        };
        let approved = crate::confirm::ask(&window, "Add recipient?", "Add recipient", text, worker_flag.clone()).unwrap_or(false);
        let outcome = grant_after_approval(approved, || {
            if worker_flag.load(Ordering::Acquire) || !current(&intent.context, &worker_flag) {
                return Err("Recipient review changed");
            }
            let grant = intent.grant(&token)?;
            execute_raw(&intent, grant, &worker_flag, &worker_uncertain, &shared)
        });
        match outcome {
            None => writer_view("done", "Identity installed. Recipient addition was declined.", true),
            Some(Ok(response)) => finish_grant(&intent, &token, &response, &window, &worker_flag, &worker_uncertain, &shared),
            Some(_) => writer_view("attention", "Identity installed. Recipient addition could not be verified; check status to reconcile.", !worker_uncertain.load(Ordering::Acquire)),
        }
    }).await;
    let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
    if !uncertain.load(Ordering::Acquire) {
        inner.active = None;
    }
    drop(inner);
    worker.map_err(|_| "Identity operation ended unexpectedly; refresh to reconcile.".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> Result<Intent, serde_json::Error> {
        let context = serde_json::from_value(json!({
            "schema_version":1, "experimental":true, "command":"status",
            "home":{"raw":"/home/test","shown":"fake home"},
            "repo":{"raw":"/real/repo","shown":"fake repo"},
            "config":{"raw":"/real/config","shown":"fake config"},
            "repo_config":{"raw":"/real/repo/.dotman.toml","shown":"fake settings"},
            "state":{"raw":"/real/state/state.toml","shown":"fake state"},
            "identity":{"raw":"/real/identity","shown":"fake identity"},
            "age_key_file":null,
            "repo_id":{"raw":"repo-id","shown":"fake id"},
            "machine":{"host":{"raw":"h","shown":"h"},"user":{"raw":"u","shown":"u"},
                "os":{"raw":"linux","shown":"linux"},"distro":{"raw":"","shown":""},
                "like":{"raw":"","shown":""},"arch":{"raw":"x","shown":"x"},"profiles":[]}
        }))?;
        Ok(Intent {
            context,
            config: None,
            parent_dev: 1,
            parent_ino: 2,
            source: None,
        })
    }

    #[test]
    fn confirmation_uses_raw_paths_and_exact_recipient() -> Result<(), Box<dyn std::error::Error>> {
        let mut intent = intent()?;
        intent.source = Some((
            PathBuf::from("/real/source"),
            Observation {
                dev: 1,
                ino: 2,
                size: 3,
                mtime: 4,
                mtime_nsec: 0,
                ctime: 5,
                ctime_nsec: 0,
            },
        ));
        let install = install_modal(&intent)?;
        assert!(install.contains("/real/identity"));
        assert!(install.contains("/real/source"));
        assert!(!install.contains("fake identity"));
        let recipient = format!("age1{}", "q".repeat(58));
        let grant = grant_modal(&intent, std::slice::from_ref(&recipient))?;
        assert!(grant.contains("/real/repo/.dotman.toml"));
        assert!(grant.contains("/real/repo"));
        assert!(grant.contains(&recipient));
        assert!(!grant.contains("fake settings"));
        assert!(!grant.contains("fake repo"));
        assert!(display_path(Path::new("/path\nline"))?.contains("\\u{a}"));
        assert!(display_path(Path::new("relative")).is_err());
        assert!(display_path(Path::new(&format!("/{}", "a".repeat(4097)))).is_err());
        Ok(())
    }

    #[test]
    fn declined_grant_does_not_run_the_writer() {
        let called = std::cell::Cell::new(false);
        let result = grant_after_approval(false, || called.set(true));
        assert!(result.is_none());
        assert!(!called.get());
    }

    #[test]
    fn cancellation_before_start_does_not_spawn() -> Result<(), Box<dyn std::error::Error>> {
        let intent = intent()?;
        let cancelled = AtomicBool::new(true);
        let uncertain = AtomicBool::new(false);
        let shared = Mutex::new(Inner::default());
        assert!(
            execute(
                &intent,
                Vec::new(),
                &cancelled,
                &uncertain,
                &shared,
                "identity"
            )
            .is_err()
        );
        assert!(!uncertain.load(Ordering::Acquire));
        Ok(())
    }

    #[test]
    fn preflight_is_bound_to_native_context_and_observations()
    -> Result<(), Box<dyn std::error::Error>> {
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let root =
            std::env::temp_dir().join(format!("dotman-key-preflight-{}", std::process::id()));
        fs::create_dir(&root)?;
        let fixture = Fixture(root);
        let repo = fixture.0.join("repo");
        let parent = fixture.0.join("private");
        let state = fixture.0.join("state");
        fs::create_dir(&repo)?;
        fs::create_dir(&parent)?;
        fs::create_dir(&state)?;
        let identity = parent.join("identity");
        fs::write(&identity, b"synthetic")?;
        let settings = repo.join(".dotman.toml");
        fs::write(&settings, b"[age]\n")?;
        let mut intent = intent()?;
        intent.context.repo.path = repo.clone();
        intent.context.repo_config.path = settings.clone();
        intent.context.identity.path = identity.clone();
        intent.context.config.path = fixture.0.join("absent-config");
        intent.context.state.path = fixture.0.join("state/state.toml");
        let parent_meta = fs::metadata(&parent)?;
        let repo_meta = fs::metadata(&repo)?;
        let state_meta = fs::metadata(&state)?;
        intent.parent_dev = parent_meta.dev();
        intent.parent_ino = parent_meta.ino();
        let recipient = format!("age1{}", "q".repeat(58));
        let recipients = vec![recipient];
        let token = json!({
            "record":"grant_preflight", "schema_version":1, "repo_id":"repo-id",
            "home":path_value(&intent.context.home.path), "repo":path_value(&repo),
            "config":path_value(&intent.context.config.path), "identity":path_value(&identity),
            "config_observation":null, "parent_dev":parent_meta.dev(), "parent_ino":parent_meta.ino(),
            "repo_dev":repo_meta.dev(), "repo_ino":repo_meta.ino(),
            "identity_observation":observation(&identity)?, "repo_config_observation":observation(&settings)?,
            "repo_config_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "recipients":recipients,
            "recipient_present":false,
            "backup_consequence":BACKUP_CONSEQUENCE,
            "recovery_parent":{"raw":state,"shown":"untrusted"},
            "recovery_parent_dev":state_meta.dev(), "recovery_parent_ino":state_meta.ino()
        });
        let encoded = serde_json::to_vec(&token)?;
        let parsed = parse_preflight(&encoded, Some(0), &intent, &recipients)?;
        let grant: Value = serde_json::from_slice(&intent.grant(&parsed)?)?;
        assert_eq!(
            grant
                .get("operation")
                .and_then(|op| op.get("repo_config_digest")),
            token.get("repo_config_digest")
        );
        assert_eq!(
            grant
                .get("operation")
                .and_then(|op| op.get("recovery_parent_dev")),
            token.get("recovery_parent_dev")
        );
        assert_eq!(
            grant
                .get("operation")
                .and_then(|op| op.get("recovery_parent_ino")),
            token.get("recovery_parent_ino")
        );
        assert_eq!(
            grant
                .get("operation")
                .and_then(|op| op.get("recovery_parent")),
            Some(&path_value(&state))
        );
        verify_rejected_preflights(&token, &encoded, &intent, &recipients)?;
        Ok(())
    }

    fn verify_rejected_preflights(
        token: &Value,
        encoded: &[u8],
        intent: &Intent,
        recipients: &[String],
    ) -> Result<(), Box<dyn std::error::Error>> {
        for (field, replacement) in [
            ("schema_version", json!(2)),
            ("repo_id", json!("wrong")),
            ("home", json!({"raw":"/wrong"})),
            ("repo", json!({"raw":"/wrong"})),
            ("config", json!({"raw":"/wrong"})),
            ("identity", json!({"raw":"/wrong"})),
            ("parent_dev", json!(0)),
            ("parent_ino", json!(0)),
            ("repo_dev", json!(0)),
            ("repo_ino", json!(0)),
            ("config_observation", json!({})),
            ("identity_observation", json!({})),
            ("repo_config_observation", json!({})),
            ("repo_config_digest", json!("invalid")),
            ("backup_consequence", json!("different")),
            ("recipient_present", json!("yes")),
            ("recovery_parent_dev", json!(0)),
            ("recovery_parent_ino", json!(0)),
            (
                "recovery_parent",
                json!({"raw":"/wrong","shown":"untrusted"}),
            ),
            ("recipients", json!([])),
        ] {
            let mut altered = token.clone();
            *altered.get_mut(field).ok_or("missing field")? = replacement;
            assert!(
                parse_preflight(&serde_json::to_vec(&altered)?, Some(0), intent, recipients)
                    .is_err(),
                "{field}"
            );
        }
        let mut missing = token.clone();
        missing
            .as_object_mut()
            .ok_or("token object")?
            .remove("config_observation");
        assert!(
            parse_preflight(&serde_json::to_vec(&missing)?, Some(0), intent, recipients).is_err()
        );
        let mut extra: Value = serde_json::from_slice(encoded)?;
        extra
            .as_object_mut()
            .ok_or("token object")?
            .insert("unexpected".into(), json!(true));
        assert!(
            parse_preflight(&serde_json::to_vec(&extra)?, Some(0), intent, recipients).is_err()
        );
        Ok(())
    }

    #[test]
    fn declined_grant_preserves_cli_installed_identity() -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};

        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let Some(cli) = std::env::var_os("DOTMAN_TEST_CLI") else {
            return Ok(());
        };
        let root = std::env::temp_dir().join(format!("dotman-key-gui-test-{}", std::process::id()));
        fs::create_dir(&root)?;
        let fixture = Fixture(root);
        let home = fixture.0.join("home");
        let config = fixture.0.join("config");
        let state = fixture.0.join("state");
        let data = fixture.0.join("data");
        let repo = fixture.0.join("repo");
        for dir in [&home, &config, &state, &data] {
            fs::create_dir(dir)?;
        }
        let parent = config.join("dotman");
        fs::create_dir(&parent)?;
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700))?;
        let command = || {
            let mut command = Command::new(&cli);
            command
                .current_dir(&home)
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", &config)
                .env("XDG_STATE_HOME", &state)
                .env("XDG_DATA_HOME", &data)
                .env("DOTMAN_REPO_PATH", &repo)
                .env("DOTMAN_NON_INTERACTIVE", "1")
                .env_remove("DOTMAN_AGE_KEY")
                .env_remove("DOTMAN_AGE_KEY_FILE")
                .env_remove("DOTMAN_CONFIG_PATH");
            command
        };
        let setup = command()
            .args(["init", "--no-import", "--no-bootstrap"])
            .output()?;
        assert!(setup.status.success());
        let status = command().args(["status", "--json"]).output()?;
        let protocol::StatusReply::Report(report) =
            protocol::parse_status(&status.stdout, status.status.code())?
        else {
            return Err("status report unavailable".into());
        };
        let intent = Intent::new(report.context, None)?;
        let request = intent.install()?;
        let before = fs::read(repo.join(".dotman.toml"))?;
        let mut child = command()
            .args(["key", "reviewed"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .ok_or("missing stdin")?
            .write_all(&request)?;
        let installed = child.wait_with_output()?;
        let recipients = receipt(&installed.stdout, installed.status.code(), "identity")?;
        assert_eq!(recipients.len(), 1);
        let identity = parent.join("age-identity.txt");
        let metadata = fs::metadata(&identity)?;
        assert_eq!(metadata.mode() & 0o777, 0o600);
        let mut preflight = command()
            .args(["key", "reviewed"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        preflight
            .stdin
            .take()
            .ok_or("missing stdin")?
            .write_all(&intent.preflight()?)?;
        let review = preflight.wait_with_output()?;
        let token = verify_cli_preflight(&review, &intent, &recipients)?;
        let called = std::cell::Cell::new(false);
        assert!(grant_after_approval(false, || called.set(true)).is_none());
        assert!(!called.get());
        let after = fs::metadata(&identity)?;
        assert_eq!(
            (metadata.dev(), metadata.ino(), metadata.size()),
            (after.dev(), after.ino(), after.size())
        );
        assert_eq!(before, fs::read(repo.join(".dotman.toml"))?);
        verify_cli_grant(&command, &intent, &token, &recipients)?;
        let after = fs::metadata(&identity)?;
        assert_eq!(
            (metadata.dev(), metadata.ino(), metadata.size()),
            (after.dev(), after.ino(), after.size())
        );
        Ok(())
    }

    fn verify_cli_grant(
        command: &impl Fn() -> std::process::Command,
        intent: &Intent,
        token: &GrantPreflight,
        recipients: &[String],
    ) -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write;
        use std::process::Stdio;
        let mut grant = command()
            .args(["key", "reviewed"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        grant
            .stdin
            .take()
            .ok_or("missing stdin")?
            .write_all(&intent.grant(token)?)?;
        let result = grant.wait_with_output()?;
        let (success, recovery) = grant_receipt(
            &result.stdout,
            result.status.code(),
            &recovery_parent(intent)?,
            recipients,
        );
        assert!(success);
        let (recovery, phase) = recovery.ok_or("missing recovery directory")?;
        assert!(phase == RecoveryPhase::Published);
        assert!(recovery.join("reviewed").is_file());
        assert!(recovery.join("candidate").is_file());
        assert!(
            fs::read_to_string(intent.context.repo_config.path.as_path())?
                .contains(&recipients.join(""))
        );
        let mut fresh = command()
            .args(["key", "reviewed"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        fresh
            .stdin
            .take()
            .ok_or("missing stdin")?
            .write_all(&intent.preflight()?)?;
        let fresh = fresh.wait_with_output()?;
        let fresh = parse_preflight(&fresh.stdout, fresh.status.code(), intent, recipients)?;
        assert_ne!(fresh.repo_config_digest, token.repo_config_digest);
        assert!(fresh.recipient_present);
        assert!(postflight_matches(token, &fresh, true));
        let before_noop = observation(&intent.context.repo_config.path)?;
        let mut noop = command()
            .args(["key", "reviewed"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        noop.stdin
            .take()
            .ok_or("missing stdin")?
            .write_all(&intent.grant(&fresh)?)?;
        let noop = noop.wait_with_output()?;
        let (valid, recovery) = grant_receipt(
            &noop.stdout,
            noop.status.code(),
            &recovery_parent(intent)?,
            recipients,
        );
        assert!(valid && recovery.is_none());
        assert_eq!(before_noop, observation(&intent.context.repo_config.path)?);
        Ok(())
    }

    fn verify_cli_preflight(
        review: &std::process::Output,
        intent: &Intent,
        recipients: &[String],
    ) -> Result<GrantPreflight, Box<dyn std::error::Error>> {
        let token = parse_preflight(&review.stdout, review.status.code(), intent, recipients)?;
        let grant: Value = serde_json::from_slice(&intent.grant(&token)?)?;
        let operation = grant.get("operation").ok_or("missing operation")?;
        assert_eq!(
            operation.get("repo_config_digest").and_then(Value::as_str),
            Some(token.repo_config_digest.as_str())
        );
        assert_eq!(
            operation.get("repo_dev").and_then(Value::as_u64),
            Some(token.repo_dev)
        );
        assert_eq!(
            operation.get("repo_ino").and_then(Value::as_u64),
            Some(token.repo_ino)
        );
        let mut altered: Value = serde_json::from_slice(&review.stdout)?;
        *altered.get_mut("repo_id").ok_or("missing repo id")? = json!("different");
        assert!(
            parse_preflight(&serde_json::to_vec(&altered)?, Some(0), intent, recipients).is_err()
        );
        assert!(parse_preflight(&review.stdout, Some(1), intent, recipients).is_err());
        for (field, replacement) in [
            ("record", json!("identity")),
            ("schema_version", json!(2)),
            ("home", json!({"raw":"/wrong"})),
            ("repo", json!({"raw":"/wrong"})),
            ("config", json!({"raw":"/wrong"})),
            ("identity", json!({"raw":"/wrong"})),
            ("parent_dev", json!(0)),
            ("parent_ino", json!(0)),
            ("repo_dev", json!(0)),
            ("repo_ino", json!(0)),
            ("config_observation", json!({})),
            ("identity_observation", json!({})),
            ("repo_config_observation", json!({})),
            ("repo_config_digest", json!("not-a-digest")),
            ("recipients", json!([])),
        ] {
            let mut altered: Value = serde_json::from_slice(&review.stdout)?;
            *altered.get_mut(field).ok_or("missing preflight field")? = replacement;
            assert!(
                parse_preflight(&serde_json::to_vec(&altered)?, Some(0), intent, recipients)
                    .is_err(),
                "{field} was accepted"
            );
        }
        Ok(token)
    }

    #[test]
    fn receipt_requires_success_one_public_recipient_and_exact_record() {
        let recipient = format!("age1{}", "q".repeat(58));
        let data = json!({"record":"identity","recipients":[recipient]}).to_string();
        assert!(receipt(data.as_bytes(), Some(0), "identity").is_ok());
        assert!(receipt(data.as_bytes(), Some(1), "identity").is_err());
        assert!(receipt(data.as_bytes(), Some(0), "recipients_added").is_err());
        assert!(
            receipt(
                br#"{"record":"identity","recipients":[]}"#,
                Some(0),
                "identity"
            )
            .is_err()
        );
        assert!(
            receipt(
                br#"{"record":"identity","recipients":["AGE-SECRET-KEY-1"]}"#,
                Some(0),
                "identity"
            )
            .is_err()
        );
        assert!(
            receipt(
                br#"{"record":"identity","recipients":[],"extra":1}"#,
                Some(0),
                "identity"
            )
            .is_err()
        );
    }

    #[test]
    fn grant_receipts_bind_recovery_and_never_accept_nonzero_exit() {
        let parent = Path::new("/state");
        let recipients = vec![format!("age1{}", "q".repeat(58))];
        let result = json!({"record":"recipients_added","recipients":recipients}).to_string();
        let recovery = json!({"record":"settings_recovery","phase":"prepared","directory":{
            "raw":"/state/key-grant-Ab12cd","shown":"/untrusted"
        }})
        .to_string();
        let published = json!({"record":"settings_recovery","phase":"published","directory":{
            "raw":"/state/key-grant-Ab12cd","shown":"/untrusted"
        }})
        .to_string();
        let changed = format!("{recovery}\n{published}\n{result}\n");
        let (success, path) = grant_receipt(changed.as_bytes(), Some(0), parent, &recipients);
        assert!(success);
        assert_eq!(
            path.as_ref().map(|(path, _)| path.as_path()),
            Some(Path::new("/state/key-grant-Ab12cd"))
        );
        let (success, path) = grant_receipt(changed.as_bytes(), Some(1), parent, &recipients);
        assert!(!success);
        assert!(path.is_some());
        let failed = format!("{recovery}\npartial");
        let (success, path) = grant_receipt(failed.as_bytes(), Some(1), parent, &recipients);
        assert!(!success);
        assert!(path.is_some());
        assert!(
            grant_receipt(
                format!("{result}\n").as_bytes(),
                Some(0),
                parent,
                &recipients
            )
            .0
        );
        for bad in [
            "/other/key-grant-Ab12cd",
            "/state/key-grant-../bad",
            "/state/key-grant-abcdef/../escape",
            "/state/key-grant-abcdefghi",
        ] {
            let record = json!({"record":"settings_recovery","phase":"prepared","directory":{"raw":bad,"shown":"ok"}})
                .to_string();
            assert!(
                grant_receipt(
                    format!("{record}\n{result}\n").as_bytes(),
                    Some(0),
                    parent,
                    &recipients
                )
                .1
                .is_none()
            );
        }
    }

    #[test]
    fn postflight_requires_recipient_presence_and_preserves_noop_settings()
    -> Result<(), Box<dyn std::error::Error>> {
        let observation =
            json!({"dev":1,"ino":2,"size":3,"mtime":4,"mtime_nsec":0,"ctime":5,"ctime_nsec":0});
        let value = json!({
            "record":"grant_preflight","schema_version":1,"repo_id":"repo",
            "home":{"raw":"/home"},"repo":{"raw":"/repo"},"config":{"raw":"/config"},
            "identity":{"raw":"/identity"},"config_observation":null,"parent_dev":1,"parent_ino":2,
            "repo_dev":3,"repo_ino":4,"identity_observation":observation,
            "repo_config_observation":observation,"repo_config_digest":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "recipients":["age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq"],
            "recipient_present":true,"backup_consequence":BACKUP_CONSEQUENCE,
            "recovery_parent":{"raw":"/state","shown":"ignored"},"recovery_parent_dev":5,"recovery_parent_ino":6
        });
        let before: GrantPreflight = serde_json::from_value(value.clone())?;
        let mut after: GrantPreflight = serde_json::from_value(value)?;
        assert!(postflight_matches(&before, &after, false));
        after.recipient_present = false;
        assert!(!postflight_matches(&before, &after, false));
        after.recipient_present = true;
        after.repo_config_observation.ino = 99;
        assert!(!postflight_matches(&before, &after, false));
        after.repo_config_digest = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
        assert!(postflight_matches(&before, &after, true));
        after.recipient_present = false;
        assert!(!postflight_matches(&before, &after, true));
        Ok(())
    }
}
