use super::{AppState, HeldPaths, Inner, WriterView, run_status, wait_apply, writer_view};
use dotman_gui::{held, held_encrypt, held_forget, protocol, runner};
use gtk::prelude::*;
use serde::Deserialize;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

pub(super) fn add_eligible(entry: &protocol::StatusEntry) -> bool {
    entry.scope == "home"
        && entry.status == "held"
        && !entry.system
        && !entry.template
        && !entry.encrypted
        && entry.finding.is_none()
        && matches!(
            entry.hold,
            Some(protocol::Hold::Large | protocol::Hold::Binary)
        )
}

pub(super) fn plain_eligible(entry: &protocol::StatusEntry) -> bool {
    entry.scope == "home"
        && entry.status == "held"
        && !entry.system
        && !entry.template
        && !entry.encrypted
        && entry.finding.is_none()
        && matches!(entry.hold, Some(protocol::Hold::SecretName))
        && std::path::Path::new(&entry.key.raw)
            .file_name()
            .is_some_and(|name| name == ".env")
}

fn encryption_occurrence(entry: &protocol::StatusEntry) -> Option<held_encrypt::Occurrence> {
    if entry.scope != "home"
        || entry.status != "held"
        || entry.system
        || entry.template
        || entry.encrypted
    {
        return None;
    }
    let (reason, finding) = match (entry.hold, &entry.finding) {
        (Some(protocol::Hold::SecretName), None) if plain_eligible(entry) => ("secret_name", None),
        (Some(protocol::Hold::Content), Some(finding))
            if matches!(finding.place, protocol::FindingPlace::Content) && finding.line > 0 =>
        {
            (
                "content",
                Some(held_encrypt::Finding {
                    rule: finding.rule.clone(),
                    place: "content".into(),
                    line: finding.line,
                }),
            )
        }
        _ => return None,
    };
    Some(held_encrypt::Occurrence {
        scope: "new".into(),
        source: None,
        reason: reason.into(),
        finding,
    })
}

fn managed_occurrence(entry: &protocol::StatusEntry) -> Option<held_encrypt::Occurrence> {
    let finding = entry.finding.as_ref()?;
    if !matches!(finding.place, protocol::FindingPlace::Content) || finding.line == 0 {
        return None;
    }
    let (scope, source) = match (entry.scope.as_str(), entry.status.as_str()) {
        ("home", "held-in-home") => ("home", entry.source.as_ref()?.raw.clone()),
        ("repo_source", "held-in-repo") => ("repo", entry.path.as_ref()?.path.to_str()?.to_owned()),
        _ => return None,
    };
    Some(held_encrypt::Occurrence {
        scope: scope.into(),
        source: Some(source),
        reason: "content".into(),
        finding: Some(held_encrypt::Finding {
            rule: finding.rule.clone(),
            place: "content".into(),
            line: finding.line,
        }),
    })
}

fn encrypt_target(
    report: &protocol::StatusReport,
    entry: &protocol::StatusEntry,
) -> Option<(String, protocol::StatusEntry, held_encrypt::Occurrence)> {
    if let Some(occurrence) = encryption_occurrence(entry) {
        return Some((entry.key.raw.clone(), entry.clone(), occurrence));
    }
    let key = forget_key(report, entry)?;
    let occurrence = managed_occurrence(entry)?;
    let mut homes = report.entries.iter().filter(|item| {
        item.scope == "home"
            && item.key.raw == key
            && if occurrence.scope == "home" {
                item.status == "held-in-home"
                    && forget_key(report, item).as_deref() == Some(key.as_str())
            } else {
                matches!(
                    item.status.as_str(),
                    "clean" | "changed-locally" | "held-in-home"
                ) && !item.system
                    && !item.encrypted
                    && !item.template
                    && item.physical_plain.as_ref() == Some(&item.raw_h)
                    && matches!(item.raw_h, protocol::StatusObservation::Present { .. })
            }
    });
    let home = homes.next()?;
    if homes.next().is_some()
        || home.identity_changed
        || (occurrence.scope == "home" && !same_finding(home, entry))
        || home.source.as_ref().map(|source| source.raw.as_str()) != occurrence.source.as_deref()
    {
        return None;
    }
    Some((key, home.clone(), occurrence))
}

pub(super) fn actions(
    entry: &protocol::StatusEntry,
    paths: &HeldPaths<'_>,
    generation: u64,
    report: &protocol::StatusReport,
) -> Vec<&'static str> {
    if generation == u64::MAX || paths.matches(entry, true) {
        return Vec::new();
    }
    let mut actions = Vec::new();
    if add_eligible(entry) {
        actions.push("add");
    }
    if plain_eligible(entry) {
        actions.push("plain");
    }
    if encrypt_target(report, entry).is_some() {
        actions.push("encrypt");
    }
    if forget_key(report, entry).is_some() {
        actions.push("forget");
    }
    actions
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Add,
    Plain,
    Encrypt,
    Forget,
}

fn eligible(entry: &protocol::StatusEntry, action: Action) -> bool {
    match action {
        Action::Add => add_eligible(entry),
        Action::Plain => plain_eligible(entry),
        Action::Encrypt | Action::Forget => true,
    }
}

fn forget_key(report: &protocol::StatusReport, entry: &protocol::StatusEntry) -> Option<String> {
    if entry.template || entry.encrypted || entry.system {
        return None;
    }
    let key = match (entry.scope.as_str(), entry.status.as_str()) {
        ("home", "held-in-home") => entry.key.raw.as_str(),
        ("repo_source", "held-in-repo") => {
            let source = entry.path.as_ref()?.path.to_str()?;
            let mut matches = report.entries.iter().filter(|item| {
                item.scope == "home"
                    && item.key.raw == source
                    && item
                        .source
                        .as_ref()
                        .is_some_and(|selected| selected.raw == source)
                    && !item.template
                    && !item.encrypted
                    && !item.system
            });
            let item = matches.next()?;
            if matches.next().is_some() {
                return None;
            }
            if item.physical_plain.as_ref() != Some(&item.raw_h)
                || !matches!(item.raw_h, protocol::StatusObservation::Present { .. })
            {
                return None;
            }
            item.key.raw.as_str()
        }
        _ => return None,
    };
    held_forget::ForgetTarget::from_status(&report.context, key, entry).ok()?;
    Some(key.to_owned())
}

fn select(
    inner: &Inner,
    id: &str,
    action: Action,
) -> Result<
    (
        protocol::StatusContext,
        protocol::StatusEntry,
        String,
        protocol::StatusEntry,
    ),
    &'static str,
> {
    if inner.active.is_some() || inner.recovery.is_some() || inner.generation == u64::MAX {
        return Err("Another operation is running");
    }
    let (generation, index) = id.split_once(':').ok_or("Invalid selection")?;
    let n: usize = index.parse().map_err(|_| "Invalid selection")?;
    if generation != inner.generation.to_string() || index != n.to_string() {
        return Err("Status selection is stale");
    }
    let Some(protocol::StatusReply::Report(report)) = &inner.reply else {
        return Err("Status is unavailable");
    };
    let entry = report.entries.get(n).ok_or("Invalid selection")?;
    if !eligible(entry, action)
        || HeldPaths::new(report).matches(entry, true)
        || report.context.repo_id.is_none()
    {
        return Err("This occurrence cannot be added");
    }
    let (key, bound) = match action {
        Action::Forget => (
            forget_key(report, entry).ok_or("This occurrence cannot be forgotten")?,
            entry.clone(),
        ),
        Action::Encrypt => {
            let (key, bound, _) =
                encrypt_target(report, entry).ok_or("This occurrence cannot be encrypted")?;
            (key, bound)
        }
        _ => (entry.key.raw.clone(), entry.clone()),
    };
    Ok((report.context.clone(), entry.clone(), key, bound))
}

fn reserve(
    inner: &mut Inner,
    id: &str,
    action: Action,
    cancelled: Arc<AtomicBool>,
) -> Result<
    (
        protocol::StatusContext,
        protocol::StatusEntry,
        String,
        protocol::StatusEntry,
    ),
    &'static str,
> {
    let selection = select(inner, id, action)?;
    inner.reply = None;
    inner.review = None;
    inner.diff_receipts.clear();
    inner.generation = inner.generation.saturating_add(1);
    inner.active = Some(cancelled);
    Ok(selection)
}

fn same_context(a: &protocol::StatusContext, b: &protocol::StatusContext) -> bool {
    a.home.path == b.home.path
        && a.repo.path == b.repo.path
        && a.config.path == b.config.path
        && a.repo_config.path == b.repo_config.path
        && a.state.path == b.state.path
        && a.identity.path == b.identity.path
        && a.age_key_file == b.age_key_file
        && a.repo_id == b.repo_id
        && a.machine == b.machine
}

fn still_current(
    context: &protocol::StatusContext,
    entry: &protocol::StatusEntry,
    cancelled: &AtomicBool,
    action: Action,
    key: &str,
    bound: &protocol::StatusEntry,
) -> bool {
    let Ok(protocol::StatusReply::Report(report)) = run_status(cancelled) else {
        return false;
    };
    let mut candidates = report.entries.iter().filter(|candidate| {
        candidate.key.raw == entry.key.raw
            && candidate.scope == entry.scope
            && candidate.path == entry.path
            && eligible(candidate, action)
            && (matches!(action, Action::Forget)
                && forget_key(&report, candidate).as_deref() == Some(key)
                && same_finding(candidate, entry)
                && candidate.source == entry.source
                || matches!(action, Action::Encrypt)
                    && encrypt_target(&report, candidate).is_some_and(
                        |(current_key, current_bound, occurrence)| {
                            current_key == key
                                && Some(occurrence)
                                    == encryption_occurrence(entry)
                                        .or_else(|| managed_occurrence(entry))
                                && current_bound.source == bound.source
                                && current_bound
                                    .alternatives
                                    .iter()
                                    .map(|s| &s.raw)
                                    .eq(bound.alternatives.iter().map(|s| &s.raw))
                                && current_bound.raw_h == bound.raw_h
                                && current_bound.source_tuple == bound.source_tuple
                                && same_finding(&current_bound, bound)
                        },
                    )
                || matches!(
                    (candidate.hold, entry.hold),
                    (Some(protocol::Hold::Large), Some(protocol::Hold::Large))
                        | (Some(protocol::Hold::Binary), Some(protocol::Hold::Binary))
                        | (
                            Some(protocol::Hold::SecretName),
                            Some(protocol::Hold::SecretName)
                        )
                ))
    });
    let Some(candidate) = candidates.next() else {
        return false;
    };
    same_context(context, &report.context)
        && candidates.next().is_none()
        && !HeldPaths::new(&report).matches(candidate, true)
        && [
            (&entry.r, &candidate.r),
            (&entry.h, &candidate.h),
            (&entry.raw_h, &candidate.raw_h),
            (&entry.s, &candidate.s),
            (&entry.source_tuple, &candidate.source_tuple),
        ]
        .into_iter()
        .all(|(old, new)| {
            matches!(old, protocol::StatusObservation::Unavailable { .. }) || old == new
        })
        && entry.physical_plain.as_ref().is_none_or(|old| {
            matches!(old, protocol::StatusObservation::Unavailable { .. })
                || candidate.physical_plain.as_ref() == Some(old)
        })
}

fn same_finding(a: &protocol::StatusEntry, b: &protocol::StatusEntry) -> bool {
    match (&a.finding, &b.finding) {
        (Some(a), Some(b)) => {
            a.rule == b.rule
                && a.line == b.line
                && matches!(
                    (a.place, b.place),
                    (
                        protocol::FindingPlace::Content,
                        protocol::FindingPlace::Content
                    )
                )
        }
        (None, None) => true,
        _ => false,
    }
}

fn safe_path(path: &std::path::Path) -> Result<String, &'static str> {
    let bytes = path.as_os_str().as_bytes();
    if !path.is_absolute() || bytes.len() > 4096 {
        return Err("Path cannot be shown safely");
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        let mut shown = String::from("Unix path bytes (hex): ");
        for byte in bytes {
            write!(&mut shown, "{byte:02x}").map_err(|_| "Path cannot be shown safely")?;
        }
        return Ok(shown);
    };
    let mut shown = String::from("\"");
    for c in text.chars() {
        if runner::unsafe_display_char(c) {
            shown.extend(c.escape_unicode());
        } else if matches!(c, '"' | '\\') {
            shown.push('\\');
            shown.push(c);
        } else {
            shown.push(c);
        }
    }
    shown.push('"');
    Ok(shown)
}

fn approved(yes: bool, cancelled: &AtomicBool) -> bool {
    yes && !cancelled.load(Ordering::Acquire)
}

fn information(
    window: &tauri::WebviewWindow,
    text: String,
    cancelled: Arc<AtomicBool>,
) -> Result<(), &'static str> {
    if window.label() != "main" {
        return Err("Cannot show recovery information");
    }
    let (sender, receiver) = mpsc::channel();
    window
        .with_webview(move |platform| {
            let Some(parent) = platform
                .inner()
                .toplevel()
                .and_then(|widget| widget.downcast::<gtk::Window>().ok())
            else {
                return;
            };
            let dialog = gtk::MessageDialog::new(
                Some(&parent),
                gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
                gtk::MessageType::Info,
                gtk::ButtonsType::Ok,
                &text,
            );
            dialog.set_title("Held-file recovery");
            dialog.connect_response(|dialog, _| dialog.close());
            dialog.connect_destroy(move |_| {
                let _ = sender.send(());
            });
            let weak = dialog.downgrade();
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                let Some(dialog) = weak.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                if cancelled.load(Ordering::Acquire) {
                    dialog.close();
                    return gtk::glib::ControlFlow::Break;
                }
                gtk::glib::ControlFlow::Continue
            });
            dialog.show_all();
        })
        .map_err(|_| "Cannot show recovery information")?;
    receiver
        .recv()
        .map_err(|_| "Cannot show recovery information")
}

fn uncertain_recovery() -> String {
    "The operation may have changed files or state. Recovery locations could not be verified. Do not retry or restore automatically; inspect status and seek terminal recovery guidance.".into()
}

const fn recovery_writer_view(refresh: bool) -> WriterView {
    writer_view(
        "attention",
        "The action may have changed files or state. Recovery locations may be unverified; check status and do not retry automatically.",
        refresh,
    )
}

fn recovery_path(
    parent: &std::path::Path,
    run: &str,
    leaf: Option<&str>,
) -> Result<String, &'static str> {
    let mut path = parent.join(run);
    if let Some(name) = leaf {
        path.push(name);
    }
    safe_path(&path)
}

fn encryption_recovery_text(
    context: &protocol::StatusContext,
    review: &held_encrypt::HeldEncryption,
    result: &held_encrypt::Execution,
) -> Result<Option<String>, &'static str> {
    if matches!(result, held_encrypt::Execution::Complete { .. }) {
        return Ok(None);
    }
    let parent = context
        .state
        .path
        .parent()
        .ok_or("State parent unavailable")?
        .join("backups");
    let reviewed = review.managed_effects();
    if reviewed
        .as_ref()
        .is_some_and(|effects| std::path::Path::new(effects.recovery_parent) != parent)
    {
        return Err("Recovery parent differs");
    }
    let (label, effects, recovery) = match result {
        held_encrypt::Execution::Complete { .. } => return Ok(None),
        held_encrypt::Execution::Partial(effects) => (
            "Encryption did not finish.",
            Some(effects),
            effects
                .recovery
                .as_ref()
                .map(held_encrypt::RecoveryHandle::as_str),
        ),
    };
    Ok(Some(encryption_notice_text(
        &parent,
        reviewed.is_some(),
        label,
        effects,
        recovery,
    )?))
}

fn encryption_notice_text(
    parent: &std::path::Path,
    managed: bool,
    label: &str,
    effects: Option<&held_encrypt::PartialEffects>,
    recovery: Option<&str>,
) -> Result<String, &'static str> {
    let location = match recovery {
        Some(handle) if managed && held_forget::safe_handle(handle, "") => format!(
            "CLI-reported recovery location for unencrypted data; inspect before restoring; contents and identity are not verified.\n{}",
            recovery_path(parent, handle, Some("source"))?
        ),
        Some(_) => return Err("Invalid recovery handle"),
        _ => "The private original's location could not be verified.".into(),
    };
    let effects = effects.map_or_else(String::new, |effects| format!(
        "\nRecipient record created: {}\nCiphertext created: {}\nPrivate backups directory created: {}\nState needs review: {}\nRecovery unavailable: {}",
        effects.record_created, effects.ciphertext_created, effects.backup_dir_created, effects.state_needs_review, effects.recovery_unavailable
    ));
    Ok(format!(
        "{label}{effects}\n\n{location}\n\nDo not retry or restore automatically. Inspect status and seek terminal recovery guidance."
    ))
}

fn forget_recovery_text(
    context: &protocol::StatusContext,
    result: &held_forget::Execution,
) -> Result<String, &'static str> {
    let parent = context
        .state
        .path
        .parent()
        .ok_or("State parent unavailable")?
        .join("backups");
    let (partial, run, quarantines) = match result {
        held_forget::Execution::Forgotten { backup_run } => (false, backup_run, &[][..]),
        held_forget::Execution::Partial {
            backup_run,
            quarantines,
        } => (true, backup_run, quarantines.as_slice()),
    };
    if !held_forget::safe_handle(run, "")
        || quarantines
            .iter()
            .any(|name| !held_forget::safe_handle(name, ".dotman-forget-"))
    {
        return Err("Invalid recovery handle");
    }
    let backup = recovery_path(&parent, run, None)?;
    let mut locations = Vec::new();
    for name in quarantines {
        locations.push(recovery_path(
            &context.repo.path.join(".git"),
            name,
            Some("source"),
        )?);
    }
    let quarantine = if locations.is_empty() {
        "No repository quarantine location was reported.".to_owned()
    } else {
        format!(
            "Reported quarantine locations; some may already have been removed:\n{}",
            locations.join("\n")
        )
    };
    Ok(format!(
        "{}\n\nVerified private backup run: {backup}\n{quarantine}\n\nDo not retry or restore automatically. Inspect status and seek terminal recovery guidance.",
        if partial {
            "Forget did not finish; other changes may have occurred."
        } else {
            "Forget completed; Home was preserved."
        }
    ))
}

enum Reviewed {
    Plain(Box<held::HeldReview>),
    Encrypt(Box<held_encrypt::HeldEncryption>),
    Forget(Box<held_forget::ForgetReview>),
}

fn preflight(
    context: &protocol::StatusContext,
    entry: &protocol::StatusEntry,
    key: &str,
    bound: &protocol::StatusEntry,
    action: Action,
    cancelled: &AtomicBool,
) -> Result<Reviewed, &'static str> {
    if matches!(action, Action::Forget) {
        let target = held_forget::ForgetTarget::from_status(context, key, entry)?;
        let request = target.review_request()?;
        let mut run =
            runner::start_remedy(&context.home.path, request).map_err(|_| "Cannot start remedy")?;
        let receipt = wait_apply(&mut run, cancelled).map_err(|_| "Cannot verify review")?;
        return Ok(Reviewed::Forget(Box::new(
            target.parse_review(&receipt.stdout, receipt.status.code())?,
        )));
    }
    let reason = match entry.hold {
        Some(protocol::Hold::Large) => "large",
        Some(protocol::Hold::Binary) => "binary",
        Some(protocol::Hold::SecretName) => "secret_name",
        Some(protocol::Hold::Content) => "content",
        _ if matches!(action, Action::Encrypt) && managed_occurrence(entry).is_some() => "content",
        _ => return Err("Unsupported hold"),
    };
    let request = match action {
        Action::Add => {
            serde_json::json!({"schema_version":1,"operation":"review","key":entry.key.raw})
        }
        Action::Plain => {
            serde_json::json!({"schema_version":1,"operation":"review_plain","key":entry.key.raw,"occurrence":{"scope":"new","source":null,"reason":"secret_name","finding":null}})
        }
        Action::Encrypt => {
            serde_json::json!({"schema_version":1,"operation":"review_encrypt","key":key,"occurrence":encryption_occurrence(entry).or_else(|| managed_occurrence(entry)).ok_or("Unsupported hold")?})
        }
        Action::Forget => return Err("Unsupported hold"),
    };
    let bytes = serde_json::to_vec(&request).map_err(|_| "Invalid request")?;
    let mut run =
        runner::start_remedy(&context.home.path, bytes).map_err(|_| "Cannot start remedy")?;
    let receipt = wait_apply(&mut run, cancelled).map_err(|_| "Cannot verify review")?;
    let repo_id = &context
        .repo_id
        .as_ref()
        .ok_or("Missing repository identity")?
        .raw;
    let review = match action {
        Action::Add => Reviewed::Plain(Box::new(held::review(
            &receipt.stdout,
            receipt.status.code(),
            &entry.key.raw,
            repo_id,
            reason,
        )?)),
        Action::Plain => Reviewed::Plain(Box::new(held::review_plain(
            &receipt.stdout,
            receipt.status.code(),
            &entry.key.raw,
            repo_id,
        )?)),
        Action::Encrypt => {
            let occurrence = encryption_occurrence(entry)
                .or_else(|| managed_occurrence(entry))
                .ok_or("Unsupported hold")?;
            let result = if occurrence.scope == "new" {
                held_encrypt::HeldEncryption::parse(
                    &receipt.stdout,
                    receipt.status.code(),
                    key,
                    repo_id,
                    &occurrence,
                )
            } else {
                held_encrypt::HeldEncryption::parse_managed(
                    &receipt.stdout,
                    receipt.status.code(),
                    context,
                    bound,
                    &occurrence,
                )
            };
            Reviewed::Encrypt(Box::new(result?))
        }
        Action::Forget => return Err("Unsupported hold"),
    };
    let matches = match &review {
        Reviewed::Plain(review) => review.matches_home(&entry.raw_h),
        Reviewed::Encrypt(review) => review.matches_home(&bound.raw_h),
        Reviewed::Forget(_) => false,
    };
    if !matches {
        return Err("Home observation changed");
    }
    Ok(review)
}

fn modal(
    context: &protocol::StatusContext,
    entry: &protocol::StatusEntry,
    review: &Reviewed,
) -> Result<String, &'static str> {
    let source = safe_path(&context.home.path.join(&entry.key.raw))?;
    let state = safe_path(&context.state.path)?;
    let settings = safe_path(&context.repo_config.path)?;
    match review {
        Reviewed::Plain(review) => {
            let target = safe_path(&context.repo.path.join(&entry.key.raw))?;
            let mode_change = if review.prepare_mode() {
                format!(
                    "\nA mode entry will be added to {settings}; it may remain if publication fails."
                )
            } else {
                String::new()
            };
            Ok(format!(
                "Add this held file?\n\nSource: {source}\nRepository target: {target}\nState: {state}\nMode: {:04o}{mode_change}\nRecipients: none (unencrypted)\n\nThe file is copied unencrypted into the repository and recorded in local state. It may be published by a later sync. Home is unchanged; no backup is created. Existing history is not removed. No hooks, commit, or sync will run.\n\nContinue?",
                review.mode()
            ))
        }
        Reviewed::Encrypt(review) => {
            let target = safe_path(&context.repo.path.join(review.destination()))?;
            let record = safe_path(&context.repo.path.join(".dotman/age-recipients"))?;
            let record_effect = if review.creates_record() {
                "will be created"
            } else {
                "already exists and will be retained"
            };
            if let Some(effects) = review.managed_effects() {
                let selected = safe_path(&context.repo.path.join(effects.selected))?;
                let recovery = safe_path(std::path::Path::new(effects.recovery_parent))?;
                let sources = effects
                    .sources
                    .into_iter()
                    .map(|(path, mode)| {
                        Ok(format!(
                            "{} (mode {mode:04o})",
                            safe_path(&context.repo.path.join(path))?
                        ))
                    })
                    .collect::<Result<Vec<_>, &'static str>>()?;
                let directory = if effects.creates_directory {
                    "The private backups directory will be created at mode 0700."
                } else {
                    "The existing private backups directory will be used."
                };
                Ok(format!(
                    "Encrypt this managed file?\n\nHome: {source}\nSelected repository source: {selected}\nReviewed sources:\n{}\nEncrypted repository target: {target}\nPrivate recovery parent: {recovery}\nRecipient record: {record} ({record_effect})\nState: {state}\nHome mode: {:04o}; encrypted file applies at 0600.\nRecipients:\n{}\n\n{directory} The previous repository plaintext inode is retained UNENCRYPTED in private recovery outside the repository. Home remains unchanged. Recorded state is replaced; mode settings are unchanged. Existing Git history is unaffected and may retain plaintext. This does not purge history. No hooks, commit, or sync will run.\n\nContinue?",
                    sources.join("\n"),
                    review.mode(),
                    review.recipients().join("\n")
                ))
            } else {
                Ok(format!(
                    "Encrypt this held file?\n\nHome source: {source}\nEncrypted repository target: {target}\nRecipient record: {record} ({record_effect})\nState: {state}\nHome mode: {:04o}; encrypted file applies at 0600.\nRecipients:\n{}\n\nHome is unchanged; no backup is created. Plaintext may remain in existing history or elsewhere; this does not purge it. No hooks, commit, or sync will run.\n\nContinue?",
                    review.mode(),
                    review.recipients().join("\n")
                ))
            }
        }
        Reviewed::Forget(review) => {
            let confirmation = review.confirmation();
            let backup = safe_path(
                &context
                    .state
                    .path
                    .parent()
                    .ok_or("State parent unavailable")?
                    .join("backups"),
            )?;
            let mut sources = Vec::new();
            for source in &confirmation.sources {
                sources.push(format!(
                    "{} (mode {})",
                    safe_path(&context.repo.path.join(&source.source))?,
                    source.mode
                ));
            }
            Ok(format!(
                "{}\n\nHome: {source}\nRepository sources:\n{}\nPrivate backups under: {backup}\nState: {state}\nSettings: {settings}\n\n{}\n\nContinue?",
                confirmation.question,
                sources.join("\n"),
                confirmation.consequences.join("\n")
            ))
        }
    }
}

#[tauri::command]
pub async fn run_held(
    id: String,
    action: Action,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<WriterView, String> {
    if id.len() > 64 {
        return Err("Invalid selection".into());
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let (context, entry, key, bound) = {
        let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
        reserve(&mut inner, &id, action, cancelled.clone())?
    };
    let shared = state.inner.clone();
    let worker_flag = cancelled.clone();
    let uncertain = Arc::new(AtomicBool::new(false));
    let worker_uncertain = uncertain.clone();
    let worker = tauri::async_runtime::spawn_blocking(move || {
        let attention = || writer_view("attention", "The held-file review could not be verified; refresh status before trying again.", true);
        if !still_current(&context, &entry, &worker_flag, action, &key, &bound) { return attention(); }
        let Ok(review) = preflight(&context, &entry, &key, &bound, action, &worker_flag) else { return attention(); };
        let Ok(text) = modal(&context, &entry, &review) else { return attention(); };
        if matches!(action, Action::Plain) && !crate::confirm::ask(&window, "Store unencrypted?", "This name is classified as sensitive. Storing it unencrypted may publish its contents in a future sync. Do you want to continue to the exact-file review?".into(), worker_flag.clone()).unwrap_or(false) {
            return writer_view("declined", "Plaintext storage was not approved.", true);
        }
        let title = match action { Action::Encrypt => "Encrypt held file?", Action::Forget => "Forget held file?", _ => "Add held file?" };
        if !approved(crate::confirm::ask(&window, title, text, worker_flag.clone()).unwrap_or(false), &worker_flag) {
            return writer_view("declined", "Held-file action was not approved.", true);
        }
        if !still_current(&context, &entry, &worker_flag, action, &key, &bound) { return attention(); }
        let request = match &review {
            Reviewed::Plain(review) => review.execute_request().map_err(|_| "Invalid request"),
            Reviewed::Encrypt(review) => review.execute_request().map_err(|_| "Invalid request"),
            Reviewed::Forget(review) => review.execute_request(),
        };
        let Ok(request) = request else { return attention(); };
        let Ok(guard) = shared.lock() else { return attention(); };
        if worker_flag.load(Ordering::Acquire) { return attention(); }
        worker_uncertain.store(true, Ordering::Release);
        let Ok(mut run) = runner::start_remedy(&context.home.path, request) else { worker_uncertain.store(false, Ordering::Release); return attention(); };
        drop(guard);
        let Ok(receipt) = wait_apply(&mut run, &worker_flag) else {
            let _ = information(&window, uncertain_recovery(), worker_flag.clone());
            return recovery_writer_view(false);
        };
        worker_uncertain.store(false, Ordering::Release);
        let (success, notice) = match &review {
            Reviewed::Plain(_) => match held::executed(&receipt.stdout, receipt.status.code(), &entry.key.raw, matches!(action, Action::Plain)) {
                Ok(()) => (true, None),
                Err(_) => (false, Some(uncertain_recovery())),
            },
            Reviewed::Encrypt(review) => review
                .execution(&receipt.stdout, receipt.status.code())
                .map_or_else(
                    |_| (false, Some(uncertain_recovery())),
                    |result| {
                        let success = matches!(&result, held_encrypt::Execution::Complete { .. });
                        let notice = encryption_recovery_text(&context, review, &result)
                            .unwrap_or_else(|_| Some(uncertain_recovery()));
                        (success, notice)
                    },
                ),
            Reviewed::Forget(review) => review
                .parse_execution(&receipt.stdout, receipt.status.code())
                .map_or_else(
                    |_| (false, Some(uncertain_recovery())),
                    |result| {
                        let success = matches!(&result, held_forget::Execution::Forgotten { .. });
                        let notice = (!success).then(|| {
                            forget_recovery_text(&context, &result)
                                .unwrap_or_else(|_| uncertain_recovery())
                        });
                        (success, notice)
                    },
                ),
        };
        if let Some(text) = notice { let _ = information(&window, text, worker_flag.clone()); }
        if !success {
            return recovery_writer_view(true);
        }
        writer_view("done", "Held-file action completed.", true)
    }).await;
    let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
    if !uncertain.load(Ordering::Acquire) {
        inner.active = None;
    }
    drop(inner);
    worker.map_err(|_| {
        "Held-file operation ended unexpectedly; check status before trying again.".into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(hold: &str, finding: &serde_json::Value) -> protocol::StatusEntry {
        let unknown = json!({"state":"unavailable","reason":"not_observed"});
        serde_json::from_value(json!({
            "status":"held","key":{"raw":"data","shown":"data"},"scope":"home",
            "path":null,"source":null,"recorded_source":null,"encrypted":false,"template":false,
            "system":false,"row":null,"row_number":null,"category":null,
            "r":unknown,"source_tuple":unknown,"h":unknown,"raw_h":unknown,"s":unknown,
            "identity_changed":false,"update_state":false,"hold":hold,"finding":finding
        }))
        .expect("test entry")
    }

    #[test]
    fn only_unredacted_new_large_or_binary_without_finding_is_eligible() {
        assert!(add_eligible(&entry("large", &serde_json::Value::Null)));
        assert!(add_eligible(&entry("binary", &serde_json::Value::Null)));
        for hold in [
            "secret_name",
            "secret_binary",
            "content",
            "special",
            "unreadable",
            "unapproved",
            "collision",
        ] {
            assert!(!add_eligible(&entry(hold, &serde_json::Value::Null)));
        }
        assert!(!add_eligible(&entry(
            "binary",
            &json!({"rule":"private-key","place":"content","line":1})
        )));
        let mut managed = entry("large", &serde_json::Value::Null);
        managed.status = "held-in-home".into();
        assert!(!add_eligible(&managed));
    }

    #[test]
    fn confirmation_escapes_invisible_characters() {
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(
            safe_path(std::path::Path::new("/tmp/a\u{202e}b")),
            Ok("\"/tmp/a\\u{202e}b\"".into())
        );
        assert!(safe_path(std::path::Path::new("relative")).is_err());
        assert_eq!(
            safe_path(std::path::Path::new(std::ffi::OsStr::from_bytes(b"/x\xff"))),
            Ok("Unix path bytes (hex): 2f78ff".into())
        );
    }

    #[test]
    fn plaintext_requires_literal_env_name_and_no_finding() {
        let mut named = entry("secret_name", &serde_json::Value::Null);
        named.key.raw = ".config/app/.env".into();
        assert!(plain_eligible(&named));
        named.finding = Some(protocol::Finding {
            rule: "private-key".into(),
            line: 1,
            place: protocol::FindingPlace::Content,
        });
        assert!(!plain_eligible(&named));
        let mut other = entry("secret_name", &serde_json::Value::Null);
        other.key.raw = ".config/app/secret".into();
        assert!(!plain_eligible(&other));
        let mut binary = entry("secret_binary", &serde_json::Value::Null);
        binary.key.raw = ".config/app/.env".into();
        assert!(!plain_eligible(&binary));
    }

    #[test]
    fn encryption_selection_is_generation_bound_and_one_use() {
        let Ok(protocol::StatusReply::Report(mut report)) = super::super::tests::report() else {
            unreachable!("test status");
        };
        report.entries.truncate(1);
        let item = report.entries.first_mut().expect("test entry");
        item.key.raw = "file".into();
        item.status = "held".into();
        item.hold = Some(protocol::Hold::Content);
        item.finding = Some(protocol::Finding {
            rule: "github-token".into(),
            place: protocol::FindingPlace::Content,
            line: 1,
        });
        let mut inner = Inner {
            generation: 7,
            reply: Some(protocol::StatusReply::Report(report)),
            ..Inner::default()
        };
        assert!(select(&inner, "6:0", Action::Encrypt).is_err());
        assert!(select(&inner, "7:0", Action::Plain).is_err());
        assert!(
            reserve(
                &mut inner,
                "7:0",
                Action::Encrypt,
                Arc::new(AtomicBool::new(false))
            )
            .is_ok()
        );
        assert!(
            reserve(
                &mut inner,
                "7:0",
                Action::Encrypt,
                Arc::new(AtomicBool::new(false))
            )
            .is_err()
        );
    }

    #[test]
    fn denied_or_cancelled_confirmation_cannot_approve() {
        let cancelled = AtomicBool::new(false);
        assert!(!approved(false, &cancelled));
        assert!(approved(true, &cancelled));
        cancelled.store(true, Ordering::Release);
        assert!(!approved(true, &cancelled));
    }

    #[test]
    fn native_partial_notice_distinguishes_reported_locations_from_verified_data()
    -> Result<(), Box<dyn std::error::Error>> {
        let protocol::StatusReply::Report(report) = super::super::tests::report()? else {
            return Err("missing status".into());
        };
        let effects = held_encrypt::PartialEffects {
            record_created: true,
            ciphertext_created: true,
            state_needs_review: true,
            backup_dir_created: false,
            recovery_unavailable: false,
            recovery: None,
        };
        let parent = std::path::Path::new("/private state/backups");
        let text = encryption_notice_text(
            parent,
            true,
            "Encryption did not finish.",
            Some(&effects),
            Some("run-123"),
        )?;
        assert!(text.contains("Recipient record created: true"));
        assert!(text.contains("Ciphertext created: true"));
        assert!(text.contains("CLI-reported recovery location for unencrypted data; inspect before restoring; contents and identity are not verified."));
        assert!(!text.contains("Verified private UNENCRYPTED original"));
        assert!(text.contains("\"/private state/backups/run-123/source\""));
        assert!(text.contains("Do not retry or restore automatically"));
        assert!(
            encryption_notice_text(parent, true, "partial", Some(&effects), Some("../bad"))
                .is_err()
        );
        let missing = encryption_notice_text(parent, true, "partial", Some(&effects), None)?;
        assert!(missing.contains("location could not be verified"));

        let partial = held_forget::Execution::Partial {
            backup_run: "old-run".into(),
            quarantines: vec![".dotman-forget-a".into(), ".dotman-forget-b".into()],
        };
        let text = forget_recovery_text(&report.context, &partial)?;
        assert!(text.contains("Reported quarantine locations; some may already have been removed"));
        assert!(!text.contains("Verified repository quarantine"));
        assert!(text.contains("\"/backups/old-run\""));
        assert!(text.contains("\"/private-raw-repo/.git/.dotman-forget-a/source\""));
        assert!(text.contains("\"/private-raw-repo/.git/.dotman-forget-b/source\""));
        let bad = held_forget::Execution::Partial {
            backup_run: "../secret".into(),
            quarantines: vec![],
        };
        assert!(forget_recovery_text(&report.context, &bad).is_err());
        let generic = serde_json::to_string(&recovery_writer_view(true))?;
        assert!(
            !generic.contains("old-run")
                && !generic.contains(".dotman-forget")
                && !generic.contains("private-raw-repo")
        );
        Ok(())
    }

    #[test]
    fn forget_requires_bound_source_and_equal_repo_home() {
        let Ok(protocol::StatusReply::Report(mut report)) = super::super::tests::report() else {
            unreachable!("test status");
        };
        report.entries.truncate(1);
        let home = report.entries.first_mut().expect("test entry");
        home.key.raw = "file".into();
        home.source = Some(protocol::StatusText {
            raw: "file".into(),
            shown: "file".into(),
        });
        home.alternatives = vec![home.source.clone().expect("source")];
        home.source_tuple = home.r.clone();
        home.raw_h = home.r.clone();
        home.physical_plain = Some(home.r.clone());
        home.status = "held-in-home".into();
        home.finding = Some(protocol::Finding {
            rule: "github-token".into(),
            place: protocol::FindingPlace::Content,
            line: 1,
        });
        assert_eq!(
            forget_key(&report, report.entries.first().expect("home")),
            Some("file".into())
        );
        let home_target = encrypt_target(&report, report.entries.first().expect("home"));
        assert!(home_target.is_some());
        assert_eq!(home_target.expect("target").2.scope, "home");
        let mut repo = report.entries.first().expect("home").clone();
        repo.status = "held-in-repo".into();
        repo.scope = "repo_source".into();
        repo.path = Some(protocol::StatusPath {
            path: "file".into(),
            shown: "file".into(),
        });
        assert_eq!(forget_key(&report, &repo), Some("file".into()));
        assert_eq!(
            encrypt_target(&report, &repo).expect("repo target").2.scope,
            "repo"
        );
        let home = report.entries.first_mut().expect("home");
        home.status = "clean".into();
        home.finding = None;
        assert_eq!(
            encrypt_target(&report, &repo)
                .expect("clean repo target")
                .2
                .scope,
            "repo"
        );
        report.entries.first_mut().expect("home").physical_plain =
            Some(protocol::StatusObservation::Missing);
        assert_eq!(forget_key(&report, &repo), None);
        assert!(encrypt_target(&report, &repo).is_none());
        repo.path.as_mut().expect("path").path = "other".into();
        assert_eq!(forget_key(&report, &repo), None);
    }
}
