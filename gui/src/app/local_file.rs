use super::{AppState, HeldPaths, Inner, WriterView, wait_apply, writer_view};
use dotman_gui::{held, protocol, runner};
use serde::Serialize;
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Serialize)]
pub(super) struct Saved {
    id: String,
    label: String,
}

const fn label(name: &str) -> &str {
    if name.is_empty() {
        "[redacted filename]"
    } else {
        name
    }
}

pub(super) fn saved(report: &protocol::StatusReport, generation: u64) -> Vec<Saved> {
    report
        .kept_local
        .values()
        .enumerate()
        .map(|(index, name)| Saved {
            id: format!("local:{generation}:{index}"),
            label: label(name).into(),
        })
        .collect()
}

pub(super) fn eligible(entry: &protocol::StatusEntry) -> bool {
    use protocol::Hold;
    entry.scope == "home"
        && entry.status == "held"
        && !entry.system
        && matches!(
            entry.hold,
            Some(
                Hold::Binary | Hold::Large | Hold::Content | Hold::SecretName | Hold::SecretBinary
            )
        )
}

struct Choice {
    home: std::path::PathBuf,
    request: Vec<u8>,
    name: String,
    restore: bool,
}

fn select(inner: &Inner, id: &str) -> Result<Choice, &'static str> {
    if inner.active.is_some() || inner.recovery.is_some() || inner.generation == u64::MAX {
        return Err("Status is unavailable");
    }
    let Some(protocol::StatusReply::Report(report)) = &inner.reply else {
        return Err("Refresh file status");
    };
    let (restore, token) = id
        .strip_prefix("local:")
        .map_or((false, id), |token| (true, token));
    let (generation, index) = token.split_once(':').ok_or("Invalid file selection")?;
    let i: usize = index.parse().map_err(|_| "Invalid file selection")?;
    if generation != inner.generation.to_string() || index != i.to_string() {
        return Err("The file selection changed");
    }
    let (operation, key, name, hidden) = if restore {
        let (digest, display) = report
            .kept_local
            .iter()
            .nth(i)
            .ok_or("Unknown local choice")?;
        ("review_again", digest, label(display), display.is_empty())
    } else {
        let entry = report
            .entries
            .get(i)
            .filter(|entry| eligible(entry))
            .ok_or("This file cannot be kept local")?;
        let hidden = HeldPaths::new(report).matches(entry, true);
        (
            "keep_local",
            &entry.key.raw,
            if hidden {
                label("")
            } else {
                entry.key.shown.as_str()
            },
            hidden,
        )
    };
    let context = &report.context;
    let repo_id = &context
        .repo_id
        .as_ref()
        .ok_or("Missing repository identity")?
        .raw;
    let request = json!({"schema_version":1,"operation":operation,"key":key,"repo_id":repo_id,
        "home": context.home.path.to_str().ok_or("Unrepresentable Home path")?,
        "repo": context.repo.path.to_str().ok_or("Unrepresentable repository path")?,
        "state": context.state.path.to_str().ok_or("Unrepresentable state path")?, "hide_name":hidden});
    Ok(Choice {
        home: context.home.path.clone(),
        request: serde_json::to_vec(&request).map_err(|_| "Invalid request")?,
        name: name.into(),
        restore,
    })
}

#[tauri::command]
pub async fn set_local_file(
    id: String,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<WriterView, String> {
    let flag = Arc::new(AtomicBool::new(false));
    let choice = {
        let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
        let choice = select(&inner, &id)?;
        inner.active = Some(flag.clone());
        inner.reply = None;
        inner.source_doc = None;
        inner.tracking = None;
        inner.review = None;
        inner.diff_receipts.clear();
        choice
    };
    let shared = state.inner.clone();
    let worker =
        tauri::async_runtime::spawn_blocking(move || change(choice, &window, &shared, &flag)).await;
    let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
    let result = worker.map_err(|_| "Local choice worker stopped")?;
    if !matches!(&result,Ok(view) if view.outcome == "cleanup_unresolved") {
        inner.active = None;
    }
    drop(inner);
    result.map_err(str::to_owned)
}

fn change(
    choice: Choice,
    window: &tauri::WebviewWindow,
    shared: &std::sync::Mutex<Inner>,
    flag: &Arc<AtomicBool>,
) -> Result<WriterView, &'static str> {
    let (title, action, description, expected, success) = if choice.restore {
        (
            "Review this file again?",
            "Review again",
            "Dotman will check this path again. An eligible file can be captured on the next sync.",
            "review_again",
            "Local choice removed. This path follows the usual capture rules.",
        )
    } else {
        (
            "Keep this file local?",
            "Keep local",
            "Folder tracking will skip this exact path on this computer, even if its contents change. You can undo this in Settings → Kept local.",
            "kept_local",
            "Kept local on this computer.",
        )
    };
    let approved = crate::confirm::ask_sync(
        window,
        crate::confirm::SyncPrompt {
            title,
            action,
            scope: description,
            destructive: false,
            dark: None,
        },
        format!("File\n  {}", choice.name),
        flag.clone(),
    )?;
    let mut run = {
        let guard = shared.lock().map_err(|_| "Status state unavailable")?;
        if !approved || flag.load(Ordering::Acquire) {
            return Ok(writer_view("declined", "Local choice unchanged.", true));
        }
        let run = runner::start_remedy(&choice.home, choice.request)
            .map_err(|_| "Could not save local choice")?;
        drop(guard);
        run
    };
    let receipt = match wait_apply(&mut run, flag) {
        Ok(receipt) => receipt,
        Err(view) => return Ok(view),
    };
    let result = held::records(&receipt.stdout, receipt.status.code())
        .ok()
        .and_then(|(first, _)| serde_json::from_slice::<serde_json::Value>(first).ok());
    if result != Some(json!({"schema_version":1,"record":"local_file_result","result":expected})) {
        return Ok(writer_view(
            "attention",
            "Could not verify the local choice. Check the refreshed file status.",
            true,
        ));
    }
    Ok(writer_view("done", success, true))
}
