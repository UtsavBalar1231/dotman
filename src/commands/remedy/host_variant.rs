use super::{Occurrence, OccurrenceScope, Operation, Partial, Request, emit};
use crate::commands::add::host_variant;
use crate::context::Context;
use crate::output::json::{PathValue, Text};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::os::unix::ffi::OsStrExt;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    occurrence: Occurrence,
    config_path: Vec<u8>,
    review: host_variant::Review,
}

#[derive(Debug)]
pub(super) struct UnsupportedRecipient;

impl std::fmt::Display for UnsupportedRecipient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("recipient type requires terminal review")
    }
}

impl std::error::Error for UnsupportedRecipient {}

fn public_recipients(review: &host_variant::Review) -> bool {
    if !review.encrypted {
        return review.recipients.is_empty();
    }
    !review.recipients.is_empty()
        && review.recipients.iter().all(|recipient| {
            recipient.len() == 62
                && recipient.starts_with("age1")
                && recipient
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

pub(super) fn validate(request: &Request) -> Result<()> {
    crate::paths::validate_key(&request.key)?;
    let occurrence = request
        .occurrence
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing occurrence"))?;
    ensure!(
        occurrence.scope == OccurrenceScope::Home
            && occurrence.reason == "host_variant"
            && occurrence.finding.is_none()
            && occurrence
                .source
                .as_ref()
                .is_some_and(|source| !source.is_empty()),
        "invalid occurrence"
    );
    crate::paths::validate_key(occurrence.source.as_deref().context("missing source")?)?;
    match request.operation {
        Operation::ReviewHostVariant => {
            ensure!(request.snapshot.is_none(), "review takes no snapshot");
        }
        Operation::ExecuteHostVariant => {
            let snapshot: Snapshot = serde_json::from_value(
                request
                    .snapshot
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("missing snapshot"))?,
            )?;
            ensure!(
                snapshot.review.key == request.key
                    && snapshot.occurrence == *occurrence
                    && snapshot.review.config_path.as_os_str().is_empty()
                    && !snapshot.config_path.is_empty()
                    && public_recipients(&snapshot.review)
                    && occurrence.source.as_deref() == Some(snapshot.review.selected.as_str()),
                "snapshot mismatch"
            );
        }
        _ => anyhow::bail!("invalid operation"),
    }
    Ok(())
}

pub(super) fn review(ctx: &Context, request: &Request, locked: bool) -> Result<()> {
    let occurrence = request.occurrence.clone().context("missing occurrence")?;
    let review = host_variant::review(ctx, &request.key, &ctx.identity().host)?;
    if !public_recipients(&review) {
        return Err(UnsupportedRecipient.into());
    }
    ensure!(
        occurrence.source.as_deref() == Some(review.selected.as_str()),
        "selected source changed"
    );
    let executable = locked && cfg!(target_os = "linux");
    let mut private = review.clone();
    private.config_path = std::path::PathBuf::new();
    let config_path = review.config_path.as_os_str().as_bytes().to_vec();
    emit(&serde_json::json!({
        "schema_version": 1, "record": "remedy_review", "operation": "review_host_variant",
        "executable": executable, "key": Text::new(&review.key), "selected": Text::new(&review.selected),
        "host": Text::new(&review.identity.host), "target": Text::new(&review.target),
        "repo_id": Text::new(&review.repo_id), "encrypted": review.encrypted,
        "recipients": review.recipients, "home_mode": format!("{:04o}", review.home_mode),
        "home": PathValue::new(&ctx.paths.home), "repo": PathValue::new(&ctx.paths.repo),
        "state": PathValue::new(&ctx.paths.state_file()),
        "config": PathValue::new(&ctx.paths.config_file),
        "repo_config": PathValue::new(&ctx.repo_config_file()),
        "identity": PathValue::new(&ctx.paths.age_identity),
        "age_key_file": ctx.paths.age_key_file.as_deref().map(PathValue::new),
        "snapshot": executable.then_some(Snapshot { occurrence, config_path, review: private }),
    }))
}

pub(super) fn execute(ctx: &Context, request: Request, locked: bool) -> Result<()> {
    ensure!(locked, "cannot execute without the repository lock");
    let mut expected: Snapshot =
        serde_json::from_value(request.snapshot.context("missing snapshot")?)?;
    ensure!(
        expected.review.key == request.key
            && request.occurrence.as_ref() == Some(&expected.occurrence),
        "snapshot mismatch"
    );
    ensure!(
        expected.review.config_path.as_os_str().is_empty()
            && expected.config_path == ctx.repo_config_file().as_os_str().as_bytes(),
        "repository config path changed"
    );
    expected.review.config_path = ctx.repo_config_file();
    let entry = host_variant::execute(ctx, &expected.review)?;
    let mut state = ctx.state(&expected.review.repo_id).context(Partial(
        "variant was published but state could not be read; refresh status",
    ))?;
    state.files.insert(request.key.clone(), entry);
    state.save(&ctx.paths.state_file(), false).context(Partial(
        "variant was published but state could not be saved; refresh status",
    ))?;
    emit(
        &serde_json::json!({"schema_version":1,"record":"remedy_result","operation":"execute_host_variant","result":"captured","key":Text::new(request.key)}),
    )
}
