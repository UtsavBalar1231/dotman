use super::{AppState, protocol, runner, wait_apply};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Serialize, Deserialize)]
pub struct Destination {
    branch: Option<String>,
    fetch: Vec<Option<String>>,
    push: Vec<Option<String>>,
    separate_push: bool,
    editable: bool,
    token: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    fetch: String,
    push: Option<String>,
}

fn read(
    context: &protocol::StatusContext,
    change: Option<&Change>,
    expected: Option<&str>,
    flag: &AtomicBool,
) -> Result<Destination, String> {
    let id = context
        .repo_id
        .as_ref()
        .ok_or("Repository identity unavailable.")?;
    let request = serde_json::json!({
        "schema_version": 1, "operation": "sync_destination", "repo_id": id.raw,
        "change": change, "expected": expected
    });
    let bytes = serde_json::to_vec(&request).map_err(|_| "Could not encode destination.")?;
    let mut run =
        runner::start_remedy(&context.home.path, bytes).map_err(|_| "Could not start Dotman.")?;
    let receipt = wait_apply(&mut run, flag)
        .map_err(|_| "Destination operation was interrupted. Reload the destination.")?;
    if !receipt.status.success() {
        let message = if expected.is_some() {
            "Could not save this destination. Reload and check for a concurrent Git edit."
        } else if change.is_some() {
            "Enter a credential-free HTTPS or SSH URL, or an absolute local path. Git URL rewrites must resolve to the address entered."
        } else {
            "The repository destination could not be read."
        };
        return Err(message.into());
    }
    let (record, _) = dotman_gui::held::records(&receipt.stdout, receipt.status.code())?;
    serde_json::from_slice(record).map_err(|_| "Could not verify the destination.".into())
}

#[tauri::command]
pub async fn sync_destination(
    change: Option<Change>,
    expected: Option<String>,
    dark: Option<bool>,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<Destination, String> {
    if change.is_some() != expected.is_some() {
        return Err("Reload the destination first.".into());
    }
    let flag = Arc::new(AtomicBool::new(false));
    let context = {
        let mut inner = state.inner.lock().map_err(|_| "Status unavailable.")?;
        if inner.active.is_some() || inner.recovery.is_some() {
            return Err("Wait for the current operation.".into());
        }
        let Some(protocol::StatusReply::Report(report)) = &inner.reply else {
            return Err("Refresh repository status first.".into());
        };
        let context = report.context.clone();
        inner.active = Some(flag.clone());
        drop(inner);
        context
    };
    let editing = change.is_some();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let before = read(&context, change.as_ref(), None, &flag)?;
        let Some(change) = change else { return Ok(before); };
        if !before.editable || expected.as_deref() != Some(before.token.as_str()) {
            return Err("The remote changed. Reload its current settings.".into());
        }
        let prompt = crate::confirm::SyncPrompt {
            title: "Change sync destination?", action: "Save destination",
            scope: "Updates origin in the local Git configuration. The next sync uses these addresses.",
            destructive: false, dark,
        };
        let text = format!("Repository\n{}\n\nFetch from\n{}\n\nPush to\n{}", context.repo.shown,
            change.fetch, change.push.as_ref().unwrap_or(&change.fetch));
        if !crate::confirm::ask_sync(&window, prompt, text, flag.clone())? || flag.load(Ordering::Acquire) {
            return Err("Destination change cancelled.".into());
        }
        let after = read(&context, Some(&change), expected.as_deref(), &flag)?;
        if after.fetch.as_slice() != [Some(change.fetch.clone())]
            || after.push.as_slice() != [Some(change.push.unwrap_or(change.fetch))] {
            return Err("The saved destination could not be verified. Reload the current addresses.".into());
        }
        Ok(after)
    }).await;
    let mut inner = state.inner.lock().map_err(|_| "Status unavailable.")?;
    inner.active = None;
    if editing && matches!(&result, Ok(Ok(_))) {
        inner.review = None;
        inner.tracking = None;
        inner.diff_receipts.clear();
        inner.source_doc = None;
    }
    drop(inner);
    result.map_err(|_| "Destination operation could not be verified.")?
}

#[tauri::command]
pub fn open_github_repository(repository: &str) -> Result<(), &'static str> {
    let (owner, name) = repository
        .split_once('/')
        .ok_or("Invalid GitHub repository.")?;
    if ![owner, name].iter().all(|part| {
        !part.is_empty()
            && part.len() <= 100
            && !matches!(*part, "." | "..")
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    }) {
        return Err("Invalid GitHub repository.");
    }
    gtk::gio::AppInfo::launch_default_for_uri(
        &format!("https://github.com/{owner}/{name}"),
        None::<&gtk::gio::AppLaunchContext>,
    )
    .map_err(|_| "Could not open the default browser.")
}
