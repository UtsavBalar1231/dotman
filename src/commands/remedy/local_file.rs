use super::{Occurrence, emit, ensure_unmanaged, new_occurrence};
use crate::{context::Context, state::State};
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    KeepLocal,
    ReviewAgain,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    schema_version: u32,
    operation: Operation,
    key: String,
    repo_id: String,
    repo: PathBuf,
    home: PathBuf,
    state: PathBuf,
    #[serde(default)]
    hide_name: bool,
}

pub(super) fn run(request: Request) -> Result<()> {
    ensure!(request.schema_version == 1, "unsupported schema version");
    let ctx = Context::resolve("remedy")?;
    let lock = ctx.lock_for_review(true)?;
    ensure!(
        lock.is_held()
            && ctx.repo_id()? == request.repo_id
            && ctx.paths.home == request.home
            && ctx.paths.repo == request.repo
            && ctx.paths.state_file() == request.state,
        "local capture context changed"
    );
    let mut state = ctx.state(&request.repo_id)?;
    let result = match request.operation {
        Operation::KeepLocal => {
            ctx.check_key(&request.key, true)?;
            ensure_unmanaged(&ctx, &request.key)?;
            let Occurrence {
                reason, finding, ..
            } = new_occurrence(&ctx, &request.key)?;
            let hidden = request.hide_name
                || matches!(reason.as_str(), "secret_name" | "secret_binary")
                || finding.is_some_and(|finding| finding.place == "name");
            let id = State::local_file_id(&request.key);
            state
                .kept_local
                .insert(id, if hidden { String::new() } else { request.key });
            "kept_local"
        }
        Operation::ReviewAgain => {
            ensure!(
                state.kept_local.remove(&request.key).is_some(),
                "local choice changed"
            );
            "review_again"
        }
    };
    state.save(&ctx.paths.state_file(), false)?;
    emit(&serde_json::json!({"schema_version":1,"record":"local_file_result","result":result}))
}
