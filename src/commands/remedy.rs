//! Review and capture one held plain file.

use crate::cli::RemedyArgs;
use crate::commands::add::held_encrypt;
use crate::commands::add::{self, Hold};
use crate::commands::forget::reviewed as held_forget;
use crate::commands::review::Stored;
use crate::context::Context;
use crate::engine::{Tuple, recorded_mode, safe_mode};
use crate::secrets;
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::path::Path;

mod host_variant;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    schema_version: u32,
    operation: Operation,
    key: String,
    snapshot: Option<serde_json::Value>,
    occurrence: Option<Occurrence>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Occurrence {
    scope: OccurrenceScope,
    source: Option<String>,
    reason: String,
    finding: Option<FindingClaim>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OccurrenceScope {
    New,
    Home,
    Repo,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FindingClaim {
    rule: String,
    place: String,
    line: usize,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Review,
    Execute,
    ReviewPlain,
    ExecutePlain,
    ReviewEncrypt,
    ExecuteEncrypt,
    ReviewForget,
    ExecuteForget,
    ReviewHostVariant,
    ExecuteHostVariant,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    action: PlainAction,
    occurrence: Option<Occurrence>,
    key: String,
    repo_id: String,
    reason: String,
    hash: String,
    content_oid: String,
    mode: u32,
    prepare_mode: bool,
    config_oid: String,
    config_mode: u32,
    context: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PlainAction {
    Held,
    AllowPlain,
}

struct Candidate {
    snapshot: Snapshot,
    #[cfg(target_os = "linux")]
    bytes: Vec<u8>,
    executable: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EncryptSnapshot {
    key: String,
    occurrence: Occurrence,
    review: EncryptReview,
    managed_binding: Option<ForgetSnapshot>,
    context: String,
    content_oid: String,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "review",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum EncryptReview {
    New(held_encrypt::NewReview),
    Managed(held_encrypt::ManagedReview),
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ForgetSnapshot {
    key: String,
    review: held_forget::ForgetReview,
    occurrence: Occurrence,
    source_oids: Vec<String>,
    home_oid: Option<String>,
}

#[derive(Debug)]
struct Partial(&'static str);

impl std::fmt::Display for Partial {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for Partial {}

#[derive(Debug)]
struct EncryptFailure {
    record_created: bool,
    ciphertext_created: bool,
    state_needs_review: bool,
    recovery: EncryptionRecovery,
}

#[derive(Debug)]
enum EncryptionRecovery {
    None {
        directory_created: bool,
    },
    Run {
        name: String,
        directory_created: bool,
    },
    Unavailable {
        directory_created: bool,
    },
}

impl std::fmt::Display for EncryptFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("encryption partially completed; refresh status")
    }
}

impl std::error::Error for EncryptFailure {}

/// Reviews or captures an unmanaged held regular leaf.
///
/// # Errors
///
/// Returns an error for invalid, stale, or unsupported requests.
pub fn run(_args: &RemedyArgs) -> Result<()> {
    let mut input = Vec::new();
    io::stdin().take(65_537).read_to_end(&mut input)?;
    ensure!(input.len() <= 65_536, "request is too large");
    let request: Request =
        serde_json::from_slice(&input).map_err(|_| anyhow::anyhow!("invalid remedy request"))?;
    ensure!(request.schema_version == 1, "unsupported schema version");
    validate_request(&request).map_err(|_| anyhow::anyhow!("invalid remedy request"))?;
    let ctx = Context::resolve("remedy")?;
    let lock = ctx.lock(true)?;
    let result = (|| match request.operation {
        Operation::Review => review_plain(&ctx, &request, PlainAction::Held, lock.is_held()),
        Operation::Execute => execute_plain(&ctx, request, PlainAction::Held, lock.is_held()),
        Operation::ReviewPlain => {
            review_plain(&ctx, &request, PlainAction::AllowPlain, lock.is_held())
        }
        Operation::ExecutePlain => {
            execute_plain(&ctx, request, PlainAction::AllowPlain, lock.is_held())
        }
        Operation::ReviewEncrypt => review_encryption(&ctx, &request, lock.is_held()),
        Operation::ExecuteEncrypt => {
            ensure!(lock.is_held(), "cannot execute without the repository lock");
            let expected: EncryptSnapshot =
                serde_json::from_value(request.snapshot.context("execute requires a snapshot")?)?;
            ensure!(expected.key == request.key, "snapshot key differs");
            ensure!(
                request.occurrence.as_ref() == Some(&expected.occurrence),
                "occurrence differs"
            );
            execute_encrypt(&ctx, &expected)
        }
        Operation::ReviewForget => {
            ensure!(
                request.snapshot.is_none(),
                "review does not accept a snapshot"
            );
            let occurrence = request.occurrence.as_ref().context("missing occurrence")?;
            let snapshot = forget_snapshot(&ctx, &request.key, occurrence)?;
            let executable = lock.is_held() && cfg!(target_os = "linux");
            let source_count = snapshot.review.sources.len();
            emit(&serde_json::json!({
                "schema_version": 1, "record": "remedy_review",
                "operation": "review_forget", "executable": executable,
                "occurrence": snapshot.occurrence, "source_count": source_count,
                "sources": snapshot.review.sources.iter().map(|source| serde_json::json!({
                    "source": source.path, "kind": source.kind, "mode": format!("{:04o}", source.mode)
                })).collect::<Vec<_>>(),
                "backup": "private_copy_of_each_source", "home_preserved": true,
                "metadata": ["source_modes", "recorded_file", "capture_ignore_if_needed"],
                "snapshot": executable.then_some(snapshot),
            }))
        }
        Operation::ExecuteForget => {
            ensure!(lock.is_held(), "cannot execute without the repository lock");
            let expected: ForgetSnapshot =
                serde_json::from_value(request.snapshot.context("execute requires a snapshot")?)?;
            ensure!(expected.key == request.key, "snapshot key differs");
            ensure!(
                request.occurrence.as_ref() == Some(&expected.occurrence),
                "occurrence differs"
            );
            execute_forget(&ctx, &expected)
        }
        Operation::ReviewHostVariant => host_variant::review(&ctx, &request, lock.is_held()),
        Operation::ExecuteHostVariant => host_variant::execute(&ctx, request, lock.is_held()),
    })();
    if let Err(error) = result {
        return report_failure(&error);
    }
    Ok(())
}

fn report_failure(error: &anyhow::Error) -> Result<()> {
    if error.is::<host_variant::UnsupportedRecipient>() {
        emit(
            &serde_json::json!({"schema_version":1,"record":"remedy_error","code":"terminal_only","message":"recipient type requires terminal review"}),
        )?;
        anyhow::bail!("recipient type requires terminal review");
    }
    if error.is::<crate::commands::add::host_variant::TerminalOnly>() {
        emit(
            &serde_json::json!({"schema_version":1,"record":"remedy_error","code":"terminal_only","message":"mode settings require terminal review"}),
        )?;
        anyhow::bail!("mode settings require terminal review");
    }
    if let Some(partial) = error.downcast_ref::<EncryptFailure>() {
        let (recovery, directory_created, unavailable) = match &partial.recovery {
            EncryptionRecovery::Run {
                name,
                directory_created,
            } => (
                Some(serde_json::json!({
                    "state_backup_run": name, "file": "source"
                })),
                *directory_created,
                false,
            ),
            EncryptionRecovery::None { directory_created } => (None, *directory_created, false),
            EncryptionRecovery::Unavailable { directory_created } => {
                (None, *directory_created, true)
            }
        };
        emit(&serde_json::json!({
            "schema_version": 1, "record": "remedy_error", "code": "partial",
            "message": "encryption partially completed; refresh status",
            "effects": {"record_created": partial.record_created,
                "ciphertext_created": partial.ciphertext_created,
                "state_needs_review": partial.state_needs_review,
                "backup_dir_created": directory_created, "recovery_unavailable": unavailable},
            "private_recovery": recovery,
        }))?;
        anyhow::bail!("encryption partially completed; refresh status");
    }
    if let Some(partial) = error.downcast_ref::<held_forget::ForgetPartial>() {
        let receipt = &partial.receipt;
        ensure!(
            safe_recovery_handle(&receipt.backup_run, "")
                && receipt
                    .quarantines
                    .iter()
                    .all(|name| safe_recovery_handle(name, ".dotman-forget-")),
            "invalid recovery receipt"
        );
        emit(&serde_json::json!({
            "schema_version": 1, "record": "remedy_error", "code": "partial",
            "message": "forget did not finish; inspect the private recovery locations",
            "private_recovery": {"state_backup_run": receipt.backup_run,
                "repo_git_quarantines": receipt.quarantines},
        }))?;
        anyhow::bail!("forget did not finish; inspect the private recovery locations");
    }
    let (code, message) = error.downcast_ref::<Partial>().map_or(
        (
            "refused",
            "remedy refused; review again and check file and permissions",
        ),
        |partial| ("partial", partial.0),
    );
    emit(&serde_json::json!({
        "schema_version": 1, "record": "remedy_error", "code": code, "message": message,
    }))?;
    anyhow::bail!("{message}");
}

fn review_encryption(ctx: &Context, request: &Request, locked: bool) -> Result<()> {
    ensure!(
        request.snapshot.is_none(),
        "review does not accept a snapshot"
    );
    let occurrence = request.occurrence.as_ref().context("missing occurrence")?;
    let snapshot = encrypt_snapshot(ctx, &request.key, occurrence)?;
    let executable = locked && parent_identity(ctx, &request.key).is_some();
    let recipients = secrets::recipient_set(&ctx.repo_config()?.age.recipients);
    let (destination, consequences) = match &snapshot.review {
        EncryptReview::New(_) => (
            crate::variant::with_age(&request.key)?,
            serde_json::Value::Null,
        ),
        EncryptReview::Managed(review) => (
            crate::variant::with_age(&review.selected)?,
            serde_json::json!({
                "selected_source": review.selected,
                "recovery_parent": review.recovery_parent,
                "recovery_consequence": review.recovery_consequence,
                "mode_consequence": review.mode_consequence,
                "creates_recipient_record": review.creates_recipient_record,
                "creates_backup_directory": review.creates_backup_directory,
                "state_change": "replace_recorded_entry", "mode_change": "none",
                "sources": snapshot.managed_binding.as_ref().map(|binding| &binding.review.sources),
            }),
        ),
    };
    emit(&serde_json::json!({
        "schema_version": 1, "record": "remedy_review",
        "operation": "review_encrypt", "executable": executable,
        "key": request.key, "occurrence": snapshot.occurrence,
        "recipients": recipients, "destination": destination,
        "plaintext_history_may_remain": true, "managed_consequences": consequences,
        "snapshot": executable.then_some(snapshot),
    }))
}

fn safe_recovery_handle(name: &str, prefix: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 128
        && name.starts_with(prefix)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_request(request: &Request) -> Result<()> {
    match request.operation {
        Operation::Review => {
            ensure!(
                request.snapshot.is_none() && request.occurrence.is_none(),
                "legacy review does not take a snapshot or occurrence"
            );
        }
        Operation::ReviewPlain | Operation::ReviewEncrypt => {
            ensure!(
                request.snapshot.is_none() && request.occurrence.is_some(),
                "review needs an occurrence"
            );
        }
        Operation::ReviewForget => {
            ensure!(
                request.snapshot.is_none() && request.occurrence.is_some(),
                "forget review needs an occurrence"
            );
        }
        Operation::Execute | Operation::ExecutePlain => {
            let snapshot: Snapshot =
                serde_json::from_value(request.snapshot.clone().context("missing snapshot")?)?;
            let action = if matches!(request.operation, Operation::Execute) {
                PlainAction::Held
            } else {
                PlainAction::AllowPlain
            };
            ensure!(
                snapshot.key == request.key
                    && snapshot.action == action
                    && ((action == PlainAction::Held && request.occurrence.is_none())
                        || request.occurrence.as_ref() == snapshot.occurrence.as_ref()),
                "snapshot mismatch"
            );
        }
        Operation::ExecuteEncrypt => {
            let snapshot: EncryptSnapshot =
                serde_json::from_value(request.snapshot.clone().context("missing snapshot")?)?;
            ensure!(
                snapshot.key == request.key
                    && request.occurrence.as_ref() == Some(&snapshot.occurrence),
                "snapshot mismatch"
            );
        }
        Operation::ExecuteForget => {
            let snapshot: ForgetSnapshot =
                serde_json::from_value(request.snapshot.clone().context("missing snapshot")?)?;
            ensure!(
                snapshot.key == request.key
                    && request.occurrence.as_ref() == Some(&snapshot.occurrence),
                "snapshot mismatch"
            );
        }
        Operation::ReviewHostVariant | Operation::ExecuteHostVariant => {
            host_variant::validate(request)?;
        }
    }
    Ok(())
}

fn review_plain(ctx: &Context, request: &Request, action: PlainAction, locked: bool) -> Result<()> {
    ensure!(
        request.snapshot.is_none(),
        "review does not accept a snapshot"
    );
    let candidate = candidate(ctx, &request.key, action)?;
    ensure!(
        action == PlainAction::Held
            || request.occurrence.as_ref() == candidate.snapshot.occurrence.as_ref(),
        "occurrence changed"
    );
    let executable = candidate.executable && locked;
    let record = if action == PlainAction::Held {
        serde_json::json!({
            "schema_version": 1, "record": "remedy_review",
            "operation": "review", "executable": executable,
            "reason": candidate.snapshot.reason, "repo_id": candidate.snapshot.repo_id,
            "key": request.key, "home": {"kind": "file", "hash": candidate.snapshot.hash,
                "mode": format!("{:04o}", candidate.snapshot.mode)},
            "snapshot": executable.then_some(candidate.snapshot),
        })
    } else {
        serde_json::json!({
            "schema_version": 1, "record": "remedy_review",
            "operation": "review_plain", "executable": executable,
            "reason": candidate.snapshot.reason, "key": request.key,
            "occurrence": candidate.snapshot.occurrence,
            "plaintext_may_be_published": true, "prepare_mode": candidate.snapshot.prepare_mode,
            "snapshot": executable.then_some(candidate.snapshot),
        })
    };
    emit(&record)
}

fn execute_plain(ctx: &Context, request: Request, action: PlainAction, locked: bool) -> Result<()> {
    ensure!(locked, "cannot execute without the repository lock");
    let expected: Snapshot =
        serde_json::from_value(request.snapshot.context("execute requires a snapshot")?)?;
    ensure!(
        expected.key == request.key && expected.action == action,
        "snapshot action or key differs"
    );
    execute(ctx, &expected)
}

fn emit(record: &serde_json::Value) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, record)?;
    writeln!(out)?;
    Ok(())
}

fn fingerprint<T: Serialize>(ctx: &Context, value: &T) -> Result<String> {
    ctx.git.hash_object(&serde_json::to_vec(value)?, false)
}

fn finding_claim(finding: secrets::Finding) -> FindingClaim {
    let place = match finding.place {
        secrets::Place::Name => "name",
        secrets::Place::Content => "content",
        secrets::Place::Target => "target",
    };
    FindingClaim {
        rule: finding.rule.to_owned(),
        place: place.to_owned(),
        line: finding.line,
    }
}

fn new_occurrence(ctx: &Context, key: &str) -> Result<Occurrence> {
    let config = ctx.repo_config()?;
    let state = ctx.state(&ctx.repo_id()?)?;
    let sources = ctx.sources()?;
    let mut known: BTreeSet<String> = sources.keys().map(str::to_owned).collect();
    known.extend(state.files.keys().cloned());
    let scan = add::scan_dirs(ctx, &config, &known, &state)?;
    let (reason, finding) = match scan.held.get(key) {
        Some(Hold::Large) => ("large", None),
        Some(Hold::Binary) => ("binary", None),
        Some(Hold::SecretName) => ("secret_name", None),
        Some(Hold::SecretBinary) => ("secret_binary", None),
        Some(Hold::Content(finding)) => ("content", Some(finding_claim(*finding))),
        _ => anyhow::bail!("file is not an eligible held leaf"),
    };
    Ok(Occurrence {
        scope: OccurrenceScope::New,
        source: None,
        reason: reason.to_owned(),
        finding,
    })
}

fn candidate(ctx: &Context, key: &str, action: PlainAction) -> Result<Candidate> {
    ctx.check_key(key, true)?;
    ensure!(
        ctx.keys().alias(&ctx.paths.home, key, true).is_none(),
        "file aliases a protected path"
    );
    let repo_id = ctx.repo_id()?;
    let config = ctx.repo_config()?;
    let state = ctx.state(&repo_id)?;
    let sources = ctx.sources()?;
    let git = ctx.git.staging_review(&[], &[])?;
    let indexed = crate::variant::Sources::group(
        git.head
            .keys()
            .chain(git.index.keys())
            .filter_map(|path| path.to_str()),
    );
    ensure!(
        indexed.sources_of(key).next().is_none(),
        "file is already managed"
    );
    let mut known: BTreeSet<String> = sources.keys().map(str::to_owned).collect();
    known.extend(state.files.keys().cloned());
    let scan = add::scan_dirs(ctx, &config, &known, &state)?;
    let reason = match (action, scan.held.get(key)) {
        (PlainAction::Held, Some(Hold::Large)) => "large",
        (PlainAction::Held, Some(Hold::Binary)) => "binary",
        (PlainAction::AllowPlain, Some(Hold::SecretName)) => "secret_name",
        _ => anyhow::bail!("file is not an eligible held leaf"),
    };
    let (bytes, mode, home_parent) = read_home(ctx, key)?;
    ensure!(
        secrets::held(&bytes, key, &[]).is_none(),
        "file is held by the content guard"
    );
    ensure!(
        action != PlainAction::AllowPlain || !secrets::looks_binary(&bytes),
        "binary secret-named files cannot be stored in plain text"
    );
    ensure!(
        matches!(
            Stored::read(&ctx.paths.repo, Path::new(key))?,
            Stored::Missing
        ),
        "destination is not absent"
    );
    let repo_parent = parent_identity(ctx, key);
    let (ready, prepare_mode) = mode_plan(ctx, &config, key, mode, action)?;
    let executable = repo_parent.is_some() && safe_mode(mode) == mode && (ready || prepare_mode);
    ensure!(
        crate::config::Config::load(&ctx.paths.config_file)? == ctx.machine,
        "machine config changed; review again"
    );
    let paths = (
        &ctx.paths.home,
        &ctx.paths.home_real,
        &ctx.paths.repo,
        &ctx.paths.config_file,
        &ctx.paths.state_dir,
    );
    let (config_oid, config_mode) = config_fingerprint(ctx)?;
    let context = fingerprint(
        ctx,
        &(
            &paths,
            &ctx.machine,
            ctx.identity(),
            &config,
            &state,
            home_parent,
            repo_parent,
            &config_oid,
            config_mode,
        ),
    )?;
    Ok(Candidate {
        snapshot: Snapshot {
            action,
            occurrence: if action == PlainAction::AllowPlain {
                Some(new_occurrence(ctx, key)?)
            } else {
                None
            },
            key: key.to_owned(),
            repo_id,
            reason: reason.to_owned(),
            hash: Tuple::of_file(&bytes, mode).hash,
            content_oid: ctx.git.hash_object(&bytes, false)?,
            mode,
            prepare_mode,
            config_oid,
            config_mode,
            context,
        },
        #[cfg(target_os = "linux")]
        bytes,
        executable,
    })
}

fn config_fingerprint(ctx: &Context) -> Result<(String, u32)> {
    let Stored::File { bytes, mode } = Stored::read(&ctx.paths.repo, Path::new(".dotman.toml"))?
    else {
        anyhow::bail!("repository config is not a regular file");
    };
    Ok((ctx.git.hash_object(&bytes, false)?, mode))
}

fn mode_plan(
    ctx: &Context,
    config: &crate::repo_config::RepoConfig,
    key: &str,
    mode: u32,
    action: PlainAction,
) -> Result<(bool, bool)> {
    let mut projected = config.clone();
    let (changed, remove) = add::record_ahead(
        &mut projected,
        &mut BTreeMap::new(),
        &ctx.paths.home,
        key,
        key,
        Some(mode),
        false,
    )?;
    let ready = recorded_mode(&config.modes, key)?.unwrap_or(0o644) == mode && !changed && !remove;
    let mut mode_only = config.clone();
    mode_only.modes.insert(key.to_owned(), "0600".to_owned());
    let prepare = action == PlainAction::AllowPlain
        && mode == 0o600
        && !config.modes.contains_key(key)
        && projected == mode_only
        && !remove;
    Ok((ready, prepare))
}

fn encrypt_snapshot(ctx: &Context, key: &str, occurrence: &Occurrence) -> Result<EncryptSnapshot> {
    ensure!(
        crate::config::Config::load(&ctx.paths.config_file)? == ctx.machine,
        "machine config changed; review again"
    );
    let (bytes, home_mode, home_parent) = read_home(ctx, key)?;
    let (review, managed_binding) = match occurrence.scope {
        OccurrenceScope::New => {
            ensure!(
                new_occurrence(ctx, key)? == *occurrence,
                "occurrence changed"
            );
            (
                EncryptReview::New(held_encrypt::review_new(ctx, key)?),
                None,
            )
        }
        OccurrenceScope::Home | OccurrenceScope::Repo => {
            let binding = forget_snapshot(ctx, key, occurrence)?;
            let review = held_encrypt::review_managed(ctx, key)?;
            ensure!(
                occurrence.source.as_deref() == Some(review.selected.as_str()),
                "selected source differs"
            );
            ensure!(
                home_mode == 0o600 || home_mode == 0o644,
                "managed conversion supports Home modes 0600 and 0644"
            );
            ensure!(
                !ctx.repo_config()?.modes.contains_key(&review.selected),
                "source mode needs separate metadata review"
            );
            let Stored::File {
                bytes: source,
                mode: source_mode,
            } = Stored::read(&ctx.paths.repo, Path::new(&review.selected))?
            else {
                anyhow::bail!("selected source is not regular")
            };
            if source != bytes {
                ensure!(
                    occurrence.scope == OccurrenceScope::Home,
                    "repository and Home differ"
                );
                ensure!(!secrets::looks_binary(&source), "source cannot be scanned");
                ensure!(secrets::held(&source, key, &[]).is_none(), "source is held");
                let git = ctx.git.staging_review(&[], &[])?;
                let source_oid = ctx.git.hash_object(&source, false)?;
                let path = Path::new(&review.selected);
                let git_mode = if source_mode & 0o100 == 0 {
                    "100644"
                } else {
                    "100755"
                };
                ensure!(
                    git.head
                        .get(path)
                        .is_some_and(|entry| entry.oid == source_oid && entry.mode == git_mode)
                        && git
                            .index
                            .get(path)
                            .is_some_and(|entry| entry.oid == source_oid && entry.mode == git_mode),
                    "source is not the unchanged tracked base"
                );
            }
            (EncryptReview::Managed(review), Some(binding))
        }
    };
    let paths = (
        &ctx.paths.home,
        &ctx.paths.home_real,
        &ctx.paths.repo,
        &ctx.paths.config_file,
        &ctx.paths.state_dir,
    );
    let context = fingerprint(
        ctx,
        &(
            &paths,
            &ctx.machine,
            ctx.identity(),
            home_parent,
            parent_identity(ctx, key),
        ),
    )?;
    Ok(EncryptSnapshot {
        key: key.to_owned(),
        occurrence: occurrence.clone(),
        review,
        managed_binding,
        context,
        content_oid: ctx.git.hash_object(&bytes, false)?,
    })
}

fn forget_snapshot(ctx: &Context, key: &str, occurrence: &Occurrence) -> Result<ForgetSnapshot> {
    use crate::commands::apply::{Classified, classify_key};
    use crate::engine::status::{Variants, Worktree};

    ensure!(
        secrets::held_name(key).is_none() && ctx.keys().alias(&ctx.paths.home, key, true).is_none(),
        "invalid or protected selector"
    );
    let review = held_forget::review_forget(ctx, key)?;
    ensure!(
        review
            .sources
            .iter()
            .all(|source| secrets::held_name(&source.path).is_none()),
        "credential in selector"
    );
    let state = bounded_state(ctx, &review.repo_id)?;
    let sources =
        crate::variant::Sources::group(review.sources.iter().map(|source| source.path.as_str()));
    let config = bounded_repo_config(ctx)?;
    let worktree = Worktree::new(&ctx.paths.repo, &config.modes);
    let variants = Variants::new(
        worktree,
        &sources,
        ctx.identity(),
        ctx.template_vars(),
        ctx.age(),
    );
    let source = occurrence
        .source
        .as_deref()
        .context("occurrence needs a source")?;
    ensure!(
        secrets::held_name(source).is_none()
            && review.sources.iter().any(|item| item.path == source),
        "invalid source"
    );
    let index = ctx.git.index_entries(&[std::path::PathBuf::from(source)])?;
    let tracked = index
        .iter()
        .any(|(mode, _, path)| path == Path::new(source) && (mode == "100644" || mode == "100755"));
    ensure!(
        state.files.contains_key(key) || tracked,
        "file is not managed"
    );
    let (source_oids, selected_stored) = forget_source_oids(ctx, &review.sources, source)?;
    let finding = match occurrence.scope {
        OccurrenceScope::New => anyhow::bail!("forget requires a managed occurrence"),
        OccurrenceScope::Home => {
            let Classified::Item(item) = classify_key(ctx, &variants, &state, key)? else {
                anyhow::bail!("file is not held in Home");
            };
            let (selected, _, _, _) = item.source_metadata().context("no selected source")?;
            ensure!(selected == source, "selected source differs");
            item.capture_hold(ctx).context("file is not held in Home")?
        }
        OccurrenceScope::Repo => {
            ensure!(tracked, "source is not tracked");
            let statuses = ctx.git.status_for(Path::new(source))?;
            ensure!(
                statuses.iter().any(|entry| {
                    entry.path == Path::new(source)
                        && matches!(
                            entry.kind,
                            crate::git::StatusKind::Ordinary | crate::git::StatusKind::Renamed
                        )
                        && !entry.xy.contains('D')
                }),
                "source is not a changed tracked file"
            );
            crate::commands::sync::holds_stored(
                Path::new(source),
                &selected_stored,
                &ctx.machine.guard.allow,
            )
            .context("source is not held in the repo")?
        }
    };
    ensure!(
        occurrence.reason == "content" && occurrence.finding == Some(finding_claim(finding)),
        "finding changed"
    );
    let home_oid = match Stored::read(&ctx.paths.home, Path::new(key))? {
        Stored::File { bytes, .. } => Some(ctx.git.hash_object(&bytes, false)?),
        Stored::Missing => None,
        _ => anyhow::bail!("forget supports regular home files only"),
    };
    ensure!(
        held_forget::review_forget(ctx, key)? == review,
        "review changed"
    );
    Ok(ForgetSnapshot {
        key: key.to_owned(),
        review,
        occurrence: occurrence.clone(),
        source_oids,
        home_oid,
    })
}

fn forget_source_oids(
    ctx: &Context,
    sources: &[held_forget::Source],
    selected: &str,
) -> Result<(Vec<String>, Stored)> {
    let mut oids = Vec::new();
    let mut selected_stored = None;
    for source in sources {
        let stored = read_repo_source(ctx, &source.path)?;
        let Stored::File { bytes, mode } = &stored else {
            anyhow::bail!("forget supports regular sources only")
        };
        let tuple = Tuple::of_file(bytes, *mode);
        ensure!(
            tuple.kind == source.kind && tuple.hash == source.hash && tuple.mode == source.mode,
            "source changed"
        );
        oids.push(ctx.git.hash_object(bytes, false)?);
        if source.path == selected {
            selected_stored = Some(stored);
        }
    }
    Ok((oids, selected_stored.context("selected source is missing")?))
}

#[cfg(target_os = "linux")]
fn read_repo_source(ctx: &Context, source: &str) -> Result<Stored> {
    use std::fs::File;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::fs::MetadataExt;
    let (directory, name) = parent(&ctx.paths.repo, source, true)?;
    let pinned = File::from(crate::sys::openat_path(directory.as_fd(), &name)?);
    ensure!(pinned.metadata()?.is_file(), "source is not regular");
    let mut file = File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))?;
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.mode() == after.mode(),
        "source changed"
    );
    Ok(Stored::File {
        bytes,
        mode: after.mode() & 0o7777,
    })
}

#[cfg(not(target_os = "linux"))]
fn read_repo_source(_ctx: &Context, _source: &str) -> Result<Stored> {
    anyhow::bail!("reviewed forget requires Linux")
}

#[cfg(target_os = "linux")]
fn bounded_repo_config(ctx: &Context) -> Result<crate::repo_config::RepoConfig> {
    use std::fs::File;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::fs::MetadataExt;

    const LIMIT: u64 = 16 * 1024 * 1024;
    let (directory, name) = parent(&ctx.paths.repo, ".dotman.toml", true)?;
    let pinned = match crate::sys::openat_path(directory.as_fd(), &name) {
        Ok(fd) => File::from(fd),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(crate::repo_config::RepoConfig::default());
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(
        pinned.metadata()?.is_file(),
        "repository config is not a regular file"
    );
    let file = File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))?;
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    file.take(LIMIT.saturating_add(1)).read_to_end(&mut bytes)?;
    ensure!(
        u64::try_from(bytes.len())? <= LIMIT,
        "repository config is too large"
    );
    let after = pinned.metadata()?;
    ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.mode() == after.mode(),
        "repository config changed"
    );
    crate::repo_config::RepoConfig::parse(std::str::from_utf8(&bytes)?)
}

#[cfg(not(target_os = "linux"))]
fn bounded_repo_config(_ctx: &Context) -> Result<crate::repo_config::RepoConfig> {
    anyhow::bail!("reviewed forget requires Linux")
}

#[cfg(target_os = "linux")]
fn bounded_state(ctx: &Context, repo_id: &str) -> Result<crate::state::State> {
    use std::fs::File;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::fs::MetadataExt;

    const LIMIT: u64 = 8 * 1024 * 1024;
    let path = ctx.paths.state_file();
    let relative = path
        .strip_prefix("/")?
        .to_str()
        .context("unsupported state path")?;
    let (directory, name) = parent(Path::new("/"), relative, false)?;
    let pinned = match crate::sys::openat_path(directory.as_fd(), &name) {
        Ok(fd) => File::from(fd),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(crate::state::State {
                repo_id: repo_id.to_owned(),
                ..Default::default()
            });
        }
        Err(error) => return Err(error.into()),
    };
    ensure!(pinned.metadata()?.is_file(), "state is not a regular file");
    let file = File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))?;
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    file.take(LIMIT.saturating_add(1)).read_to_end(&mut bytes)?;
    ensure!(u64::try_from(bytes.len())? <= LIMIT, "state is too large");
    let after = pinned.metadata()?;
    ensure!(
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.mode() == after.mode(),
        "state changed"
    );
    let state = crate::state::State::parse_bytes(&path, &bytes)?;
    ensure!(
        state.repo_id == repo_id,
        "state belongs to another repository"
    );
    Ok(state)
}

#[cfg(not(target_os = "linux"))]
fn bounded_state(_ctx: &Context, _repo_id: &str) -> Result<crate::state::State> {
    anyhow::bail!("reviewed forget requires Linux")
}

#[cfg(target_os = "linux")]
fn execute_forget(ctx: &Context, expected: &ForgetSnapshot) -> Result<()> {
    let actual = forget_snapshot(ctx, &expected.key, &expected.occurrence)?;
    ensure!(
        actual.review == expected.review
            && actual.occurrence == expected.occurrence
            && actual.source_oids == expected.source_oids
            && actual.home_oid == expected.home_oid,
        "review changed; review again"
    );
    let receipt = held_forget::execute_forget(ctx, &expected.review).context(Partial(
        "forget may have created private backups or changed the repo; inspect backups and refresh status",
    ))?;
    ensure!(
        safe_recovery_handle(&receipt.backup_run, "")
            && receipt
                .quarantines
                .iter()
                .all(|name| safe_recovery_handle(name, ".dotman-forget-")),
        Partial("forget completed but its recovery receipt is invalid; inspect backups")
    );
    emit(
        &serde_json::json!({"schema_version": 1, "record": "remedy_result",
        "operation": "execute_forget", "result": "forgotten",
        "private_recovery": {"state_backup_run": receipt.backup_run,
            "repo_git_quarantines": receipt.quarantines}}),
    )
}

#[cfg(not(target_os = "linux"))]
fn execute_forget(_ctx: &Context, _expected: &ForgetSnapshot) -> Result<()> {
    anyhow::bail!("reviewed forget is available only on Linux")
}

fn execute_encrypt(ctx: &Context, expected: &EncryptSnapshot) -> Result<()> {
    let actual = encrypt_snapshot(ctx, &expected.key, &expected.occurrence)?;
    ensure!(
        actual.review == expected.review
            && actual.managed_binding == expected.managed_binding
            && actual.context == expected.context
            && actual.content_oid == expected.content_oid,
        "review changed; review again"
    );
    let (entry, recovery_run, record_created, backup_dir_created) = match &expected.review {
        EncryptReview::New(review) => {
            let record_created = matches!(
                Stored::read(&ctx.paths.repo, Path::new(secrets::RECORD))?,
                Stored::Missing
            );
            let entry = held_encrypt::execute_new(ctx, review)
                .map_err(|error| encryption_failure(ctx, None, error))?;
            (entry, None, record_created, false)
        }
        EncryptReview::Managed(review) => {
            let result = held_encrypt::execute_managed(ctx, review)
                .map_err(|error| encryption_failure(ctx, Some(review), error))?;
            let handle =
                recovery_handle(ctx, review, &result.backup).map_err(|_| EncryptFailure {
                    record_created: review.creates_recipient_record,
                    ciphertext_created: true,
                    state_needs_review: true,
                    recovery: EncryptionRecovery::Unavailable {
                        directory_created: review.creates_backup_directory,
                    },
                })?;
            (
                result.entry,
                Some(handle),
                review.creates_recipient_record,
                review.creates_backup_directory,
            )
        }
    };
    let partial = || EncryptFailure {
        record_created,
        ciphertext_created: true,
        state_needs_review: true,
        recovery: recovery_run.clone().map_or(
            EncryptionRecovery::None {
                directory_created: backup_dir_created,
            },
            |name| EncryptionRecovery::Run {
                name,
                directory_created: backup_dir_created,
            },
        ),
    };
    let repo_id = ctx.repo_id().context(Partial(
        "encrypted file was installed but repository identity could not be read; refresh status",
    )).map_err(|_| partial())?;
    let mut state = bounded_state(ctx, &repo_id)
        .context(Partial(
            "encrypted file was installed but state could not be read; refresh status",
        ))
        .map_err(|_| partial())?;
    state.files.insert(expected.key.clone(), entry);
    if state.identity.is_none() {
        state.identity = Some(ctx.identity().clone());
    }
    state
        .save(&ctx.paths.state_file(), false)
        .map_err(|_| partial())?;
    emit(
        &serde_json::json!({"schema_version": 1, "record": "remedy_result",
        "operation": "execute_encrypt", "result": "encrypted", "key": expected.key,
        "backup_dir_created": backup_dir_created,
        "private_recovery": recovery_run.map(|run| serde_json::json!({
            "state_backup_run": run, "file": "source"
        }))}),
    )
}

fn recovery_handle(
    ctx: &Context,
    review: &held_encrypt::ManagedReview,
    path: &Path,
) -> Result<String> {
    #[cfg(target_os = "linux")]
    use std::os::fd::AsFd;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::MetadataExt;
    let parent = ctx.paths.state_dir.join("backups");
    ensure!(
        Path::new(&review.recovery_parent) == parent,
        "recovery parent differs"
    );
    let mut parts = path.strip_prefix(parent)?.components();
    let Some(std::path::Component::Normal(run)) = parts.next() else {
        anyhow::bail!("invalid recovery handle");
    };
    ensure!(
        parts.next() == Some(std::path::Component::Normal(std::ffi::OsStr::new("source")))
            && parts.next().is_none(),
        "invalid recovery handle"
    );
    let run = run.to_str().context("invalid recovery handle")?;
    ensure!(
        safe_recovery_handle(run, "recovered-"),
        "invalid recovery handle"
    );
    #[cfg(target_os = "linux")]
    {
        let (backups, _) = self::parent(&ctx.paths.state_dir, &format!("backups/{run}"), true)?;
        let (directory, source) =
            self::parent(&ctx.paths.state_dir, &format!("backups/{run}/source"), true)?;
        for directory in [&backups, &directory] {
            let metadata = directory.metadata()?;
            ensure!(
                metadata.uid() == crate::sys::geteuid() && metadata.mode() & 0o7777 == 0o700,
                "recovery directory is not private"
            );
        }
        let file = std::fs::File::from(crate::sys::openat_path(directory.as_fd(), &source)?);
        ensure!(file.metadata()?.is_file(), "recovery file is not regular");
    }
    Ok(run.to_owned())
}

fn encryption_failure(
    ctx: &Context,
    review: Option<&held_encrypt::ManagedReview>,
    error: anyhow::Error,
) -> anyhow::Error {
    let Some(partial) = error.downcast_ref::<held_encrypt::EncryptPartial>() else {
        return error;
    };
    let recovery = match (&partial.recovery, review) {
        (Some(path), Some(review)) => recovery_handle(ctx, review, path).map_or(
            EncryptionRecovery::Unavailable {
                directory_created: partial.backup_dir_created,
            },
            |name| EncryptionRecovery::Run {
                name,
                directory_created: partial.backup_dir_created,
            },
        ),
        (Some(_), None) => EncryptionRecovery::Unavailable {
            directory_created: partial.backup_dir_created,
        },
        (None, _) => EncryptionRecovery::None {
            directory_created: partial.backup_dir_created,
        },
    };
    EncryptFailure {
        record_created: partial.record_created,
        ciphertext_created: partial.ciphertext_created,
        state_needs_review: partial.state_needs_review,
        recovery,
    }
    .into()
}

#[cfg(target_os = "linux")]
fn parent(
    root: &Path,
    key: &str,
    nofollow_root: bool,
) -> Result<(std::fs::File, std::ffi::CString)> {
    use std::ffi::CString;
    use std::fs::{File, OpenOptions};
    use std::os::fd::AsFd;
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options.read(true).custom_flags(
        libc::O_DIRECTORY | libc::O_CLOEXEC | if nofollow_root { libc::O_NOFOLLOW } else { 0 },
    );
    let mut directory = options.open(root)?;
    let mut parts = Path::new(key).components().peekable();
    while let Some(part) = parts.next() {
        let name = CString::new(part.as_os_str().as_encoded_bytes())?;
        if parts.peek().is_none() {
            return Ok((directory, name));
        }
        directory = File::from(crate::sys::openat_dir(directory.as_fd(), &name)?);
    }
    anyhow::bail!("missing destination name")
}

#[cfg(target_os = "linux")]
fn identity(file: &std::fs::File) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    Ok((meta.dev(), meta.ino()))
}

#[cfg(target_os = "linux")]
fn parent_identity(ctx: &Context, key: &str) -> Option<(u64, u64)> {
    parent(&ctx.paths.repo, key, true)
        .and_then(|(directory, _)| identity(&directory))
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn parent_identity(_ctx: &Context, _key: &str) -> Option<(u64, u64)> {
    None
}

#[cfg(target_os = "linux")]
fn read_home(ctx: &Context, key: &str) -> Result<(Vec<u8>, u32, (u64, u64))> {
    use std::fs::File;
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::fs::MetadataExt;

    let (directory, name) = parent(&ctx.paths.home, key, false)?;
    let parent_identity = identity(&directory)?;
    let pinned = File::from(crate::sys::openat_path(directory.as_fd(), &name)?);
    ensure!(pinned.metadata()?.is_file(), "file is not a regular leaf");
    let mut file = File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd()))?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "file changed while reading");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    ensure!(
        metadata.dev() == after.dev()
            && metadata.ino() == after.ino()
            && metadata.len() == after.len()
            && metadata.mtime() == after.mtime()
            && metadata.mtime_nsec() == after.mtime_nsec()
            && metadata.mode() == after.mode(),
        "file changed while reading"
    );
    Ok((bytes, after.mode() & 0o7777, parent_identity))
}

#[cfg(not(target_os = "linux"))]
fn read_home(ctx: &Context, key: &str) -> Result<(Vec<u8>, u32, (u64, u64))> {
    match Stored::read(&ctx.paths.home, Path::new(key))? {
        Stored::File { bytes, mode } => Ok((bytes, mode, (0, 0))),
        _ => anyhow::bail!("file is not an eligible held regular leaf"),
    }
}

#[cfg(target_os = "linux")]
fn execute(ctx: &Context, expected: &Snapshot) -> Result<()> {
    use crate::sys;
    use std::ffi::CString;
    use std::fs::Permissions;
    use std::os::fd::AsFd;
    use std::os::unix::fs::PermissionsExt;

    let initial = candidate(ctx, &expected.key, expected.action)?;
    ensure!(
        initial.executable && initial.snapshot == *expected,
        "review changed; review again"
    );
    let (directory, leaf) = parent(&ctx.paths.repo, &expected.key, true)?;
    ensure!(
        sys::kind_at(directory.as_fd(), &leaf)?.is_none(),
        "destination is occupied"
    );
    let temp = CString::new(format!(".dotman-tmp-remedy-{}", std::process::id()))?;
    let mut file = sys::create_excl(directory.as_fd(), &temp, 0o600)?;
    let mut prepared = false;
    let result = (|| -> Result<()> {
        file.write_all(&initial.bytes)?;
        file.set_permissions(Permissions::from_mode(expected.mode))?;
        file.sync_all()?;
        let latest = candidate(ctx, &expected.key, expected.action)?;
        ensure!(
            latest.executable && latest.snapshot == *expected && latest.bytes == initial.bytes,
            "review changed; review again"
        );
        ensure!(
            parent_identity(ctx, &expected.key) == Some(identity(&directory)?),
            "destination directory changed; review again"
        );
        if expected.prepare_mode {
            prepared = true;
            prepare_private_mode(ctx, expected)?;
        }
        sys::rename_noreplace(directory.as_fd(), &temp, directory.as_fd(), &leaf)?;
        sys::fsync_dir(directory.as_fd()).context(Partial(
            "file was installed but directory sync failed; refresh status",
        ))?;
        let mut state = ctx.state(&expected.repo_id).context(Partial(
            "file was installed but state could not be read; refresh status",
        ))?;
        state.files.insert(
            expected.key.clone(),
            Tuple::of_file(&initial.bytes, expected.mode).entry(&expected.key),
        );
        if state.identity.is_none() {
            state.identity = Some(ctx.identity().clone());
        }
        state.save(&ctx.paths.state_file(), false).context(Partial(
            "file was installed but state could not be saved; refresh status",
        ))?;
        emit(
            &serde_json::json!({"schema_version": 1, "record": "remedy_result",
            "operation": if expected.action == PlainAction::Held { "execute" } else { "execute_plain" },
            "result": "captured", "key": expected.key}),
        )
    })();
    if result.is_err() {
        match sys::unlinkat(directory.as_fd(), &temp) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("cannot remove remedy temporary file"),
        }
    }
    if prepared
        && result
            .as_ref()
            .err()
            .is_some_and(|error| error.downcast_ref::<Partial>().is_none())
    {
        return result.context(Partial(
            "the reviewed 0600 mode setting may have been saved; refresh status",
        ));
    }
    result
}

#[cfg(target_os = "linux")]
fn prepare_private_mode(ctx: &Context, expected: &Snapshot) -> Result<()> {
    use std::os::fd::AsRawFd;
    let (root, _) = parent(&ctx.paths.repo, ".dotman.toml", true)?;
    let pinned = std::path::PathBuf::from(format!("/proc/self/fd/{}", root.as_raw_fd()));
    let Stored::File { bytes, mode } = Stored::read(&pinned, Path::new(".dotman.toml"))? else {
        anyhow::bail!("repository config is not a regular file");
    };
    ensure!(
        mode == expected.config_mode && ctx.git.hash_object(&bytes, false)? == expected.config_oid,
        "repository config changed; review again"
    );
    let text = std::str::from_utf8(&bytes)?;
    let mut document = text.parse::<toml_edit::DocumentMut>()?;
    if !document.as_table().contains_key("modes") {
        document
            .as_table_mut()
            .insert("modes", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    let modes = document
        .as_table_mut()
        .get_mut("modes")
        .and_then(toml_edit::Item::as_table_like_mut)
        .context("modes is not a table")?;
    ensure!(
        modes.get(&expected.key).is_none(),
        "mode setting changed; review again"
    );
    modes.insert(&expected.key, toml_edit::value("0600"));
    crate::paths::write_atomic(
        &pinned.join(".dotman.toml"),
        document.to_string().as_bytes(),
        mode & 0o777,
    )
}

#[cfg(not(target_os = "linux"))]
fn execute(_ctx: &Context, _expected: &Snapshot) -> Result<()> {
    anyhow::bail!("reviewed capture is available only on Linux")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{Env, Paths};

    #[test]
    fn encryption_partial_preserves_flags_and_rejects_unbound_recovery() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let home = temp.path().join("home");
        std::fs::create_dir(&home)?;
        let env = Env::from_fn(|name| (name == "HOME").then(|| home.clone().into_os_string()));
        let paths = Paths::resolve(&env, crate::sys::geteuid())?;
        let ctx = Context::new(paths, crate::config::Config::default(), "remedy");
        let error = anyhow::Error::new(held_encrypt::EncryptPartial {
            record_created: true,
            ciphertext_created: false,
            state_needs_review: true,
            backup_dir_created: true,
            recovery: Some(temp.path().join("unbound")),
        });
        let mapped = encryption_failure(&ctx, None, error);
        let partial = mapped
            .downcast_ref::<EncryptFailure>()
            .context("missing partial")?;
        assert!(partial.record_created);
        assert!(!partial.ciphertext_created);
        assert!(partial.state_needs_review);
        assert!(matches!(
            &partial.recovery,
            EncryptionRecovery::Unavailable {
                directory_created: true
            }
        ));
        assert!(!mapped.to_string().contains("unbound"));
        assert!(safe_recovery_handle("recovered-Ab12", "recovered-"));
        assert!(!safe_recovery_handle(
            "recovered-Ab12/../source",
            "recovered-"
        ));
        let second = anyhow::Error::new(held_encrypt::EncryptPartial {
            record_created: false,
            ciphertext_created: true,
            state_needs_review: false,
            backup_dir_created: false,
            recovery: None,
        });
        let mapped = encryption_failure(&ctx, None, second);
        let partial = mapped
            .downcast_ref::<EncryptFailure>()
            .context("missing partial")?;
        assert!(!partial.record_created);
        assert!(partial.ciphertext_created);
        assert!(!partial.state_needs_review);
        assert!(matches!(
            &partial.recovery,
            EncryptionRecovery::None {
                directory_created: false
            }
        ));
        Ok(())
    }
}
