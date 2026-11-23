use super::{AppState, protocol, runner, wait_apply};
use gtk::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    TrackFiles,
    TrackFolder,
    StopTracking,
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Preview {
    record: String,
    #[serde(skip_serializing)]
    token: String,
    files: Vec<String>,
    folders: Vec<String>,
    held: BTreeMap<String, String>,
    existing: Vec<String>,
}

pub(super) struct Pending {
    pub(super) id: String,
    context: protocol::StatusContext,
    action: Action,
    paths: Vec<String>,
    preview: Preview,
}

fn choose(
    window: &tauri::WebviewWindow,
    home: PathBuf,
    action: Action,
    flag: Arc<AtomicBool>,
) -> Result<Vec<PathBuf>, String> {
    let (sender, receiver) = mpsc::channel();
    window
        .with_webview(move |platform| {
            if let Some(parent) = platform
                .inner()
                .toplevel()
                .and_then(|w| w.downcast::<gtk::Window>().ok())
            {
                crate::folder_chooser::choose_paths(
                    &parent,
                    flag,
                    match action {
                        Action::TrackFolder => "Choose folder to track",
                        Action::TrackFiles => "Choose files to track",
                        Action::StopTracking => "Choose folder to stop tracking",
                    },
                    action != Action::TrackFiles,
                    Some(&home),
                    move |paths| {
                        let _ = sender.send(paths);
                    },
                );
            }
        })
        .map_err(|_| "Cannot open the file chooser.")?;
    receiver
        .recv()
        .map_err(|_| "The file chooser closed.".into())
}

fn call(
    context: &protocol::StatusContext,
    action: Action,
    paths: &[String],
    expected: Option<&str>,
    flag: &AtomicBool,
) -> Result<Preview, String> {
    let request = serde_json::json!({"schema_version":1,"operation":action,"paths":paths,"expected":expected,
        "repo_id":context.repo_id.as_ref().ok_or("Repository identity unavailable.")?.raw});
    let bytes = serde_json::to_vec(&request).map_err(|_| "Selection could not be encoded.")?;
    let mut run = runner::start_remedy(&context.home.path, bytes)
        .map_err(|_| "Cannot start the matching Dotman CLI.")?;
    let receipt = wait_apply(&mut run, flag)
        .map_err(|_| "Tracking was interrupted. Refresh to check which files are tracked.")?;
    if receipt.status.code() == Some(3) && expected.is_some() {
        return Err(
            "Files or tracking rules changed during review. Review the selection again.".into(),
        );
    }
    if !receipt.status.success() {
        if expected.is_some() {
            return Err(
                "The review could not be applied. Refresh and review the current files.".into(),
            );
        }
        return Err("Could not review this selection. Choose supported paths inside Home and try a smaller selection if needed.".into());
    }
    let (first, _) = dotman_gui::held::records(&receipt.stdout, receipt.status.code())?;
    let preview: Preview = serde_json::from_slice(first)
        .map_err(|_| "Incomplete tracking response. Refresh to check the repository.")?;
    let record = if expected.is_some() {
        "tracking_done"
    } else {
        "tracking_review"
    };
    if preview.record != record
        || preview.token.is_empty()
        || expected.is_some_and(|t| t != preview.token)
    {
        return Err("Tracking response did not match the request.".into());
    }
    Ok(preview)
}

#[tauri::command]
pub async fn preview_tracking(
    action: Action,
    key: Option<String>,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<Option<(String, Preview)>, String> {
    let flag = Arc::new(AtomicBool::new(false));
    let (context, id) = {
        let mut inner = state.inner.lock().map_err(|_| "Status unavailable.")?;
        if inner.active.is_some() || inner.recovery.is_some() {
            return Err("Another operation needs attention.".into());
        }
        let Some(protocol::StatusReply::Report(report)) = &inner.reply else {
            return Err("Refresh the file tree first.".into());
        };
        if let Some(key) = &key
            && (action != Action::StopTracking
                || key.is_empty()
                || !report.entries.iter().any(|entry| {
                    entry.scope == "home"
                        && (entry.key.raw == *key
                            || entry
                                .key
                                .raw
                                .strip_prefix(key)
                                .is_some_and(|rest| rest.starts_with('/')))
                }))
        {
            return Err("Select a current file or folder in the tree.".into());
        }
        let context = report.context.clone();
        inner.tracking = None;
        inner.review_sequence = inner
            .review_sequence
            .checked_add(1)
            .ok_or("Reopen the workspace.")?;
        let id = format!("tracking:{}:{}", inner.generation, inner.review_sequence);
        inner.active = Some(flag.clone());
        drop(inner);
        (context, id)
    };
    let flag_worker = flag.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let paths = if let Some(key) = key {
            vec![key]
        } else {
            let home = &context.home.path;
            let selected = choose(&window, home.clone(), action, flag_worker.clone())?;
            if selected.is_empty() {
                return Ok(None);
            }
            selected
                .iter()
                .map(|p| relative(home, p))
                .collect::<Result<Vec<_>, _>>()?
        };
        let preview = call(&context, action, &paths, None, &flag_worker)?;
        Ok(Some(Pending {
            id,
            context,
            action,
            paths,
            preview,
        }))
    })
    .await;
    let mut inner = state.inner.lock().map_err(|_| "Status unavailable.")?;
    inner.active = None;
    if flag.load(Ordering::Acquire) {
        return Ok(None);
    }
    let result: Result<Option<Pending>, String> =
        result.map_err(|_| "Tracking preview stopped.")?;
    inner.tracking = result?;
    Ok(inner
        .tracking
        .as_ref()
        .map(|p| (p.id.clone(), p.preview.clone())))
}

fn relative(home: &Path, path: &Path) -> Result<String, String> {
    let rel = path.strip_prefix(home).ok().or_else(|| {
        home.canonicalize()
            .ok()
            .and_then(|real| path.strip_prefix(real).ok())
    });
    rel.and_then(Path::to_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "Choose a file or folder inside Home.".into())
}

#[tauri::command]
pub async fn apply_tracking(
    id: String,
    dark: Option<bool>,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<bool, String> {
    let flag = Arc::new(AtomicBool::new(false));
    let pending = {
        let mut inner = state.inner.lock().map_err(|_| "Status unavailable.")?;
        if inner.active.is_some()
            || inner.recovery.is_some()
            || inner.reply.is_none()
            || inner.tracking.as_ref().is_none_or(|p| p.id != id)
        {
            return Err("Reopen the tracking review.".into());
        }
        let p = inner.tracking.take().ok_or("Reopen the tracking review.")?;
        inner.active = Some(flag.clone());
        p
    };
    let result = tauri::async_runtime::spawn_blocking(move || {
        let p = pending;
        let stop = p.action == Action::StopTracking;
        let prompt = crate::confirm::SyncPrompt {
            title: if stop {
                "Stop tracking?"
            } else {
                "Track this selection?"
            },
            action: if stop {
                "Stop tracking"
            } else {
                "Track selection"
            },
            scope: if stop {
                "Repository sources will be removed. Local copies stay in Home."
            } else {
                "Eligible files will be added to the repository on this computer."
            },
            destructive: stop,
            dark,
        };
        let mut text = format!("Repository\n{}\n\n", p.context.repo.shown);
        if !p.preview.folders.is_empty() {
            text.push_str(if stop {
                "Remove folder rules\n"
            } else {
                "Capture future files in\n"
            });
            text.push_str(&p.preview.folders.join("\n"));
            text.push_str("\n\n");
        }
        text.push_str(if stop {
            "Remove repository sources\n"
        } else {
            "Track files\n"
        });
        text.push_str(&p.preview.files.join("\n"));
        if !p.preview.held.is_empty() {
            text.push_str("\n\nHeld back\n");
            for (key, why) in &p.preview.held {
                let _ = writeln!(text, "{key} — {why}");
            }
        }
        if !crate::confirm::ask_sync(&window, prompt, text, flag.clone())?
            || flag.load(Ordering::Acquire)
        {
            return Ok(false);
        }
        let result = call(
            &p.context,
            p.action,
            &p.paths,
            Some(&p.preview.token),
            &flag,
        )?;
        if result.files != p.preview.files
            || result.folders != p.preview.folders
            || result.held != p.preview.held
            || result.existing != p.preview.existing
        {
            return Err("Tracking receipt changed. Refresh to check the repository.".into());
        }
        Ok(true)
    })
    .await;
    let mut inner = state.inner.lock().map_err(|_| "Status unavailable.")?;
    inner.active = None;
    inner.reply = None;
    inner.review = None;
    inner.diff_receipts.clear();
    inner.source_doc = None;
    drop(inner);
    result.map_err(|_| "Tracking ended unexpectedly. Refresh to check the repository.")?
}
