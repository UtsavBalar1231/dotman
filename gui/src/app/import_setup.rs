use super::{AppState, WriterView, reserve_setup, run_status, wait_apply, writer_view};
use dotman_gui::{protocol, runner, setup};
use gtk::prelude::*;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

fn choose(
    window: &tauri::WebviewWindow,
    cancelled: Arc<AtomicBool>,
) -> Result<Option<PathBuf>, &'static str> {
    if window.label() != "main" {
        return Err("Cannot open directory chooser");
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
            crate::folder_chooser::choose(&parent, cancelled, move |path| {
                let _ = sender.send(path);
            });
        })
        .map_err(|_| "Cannot open directory chooser")?;
    receiver.recv().map_err(|_| "Cannot open directory chooser")
}

#[tauri::command]
pub async fn run_import_setup(
    id: String,
    action: String,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<WriterView, String> {
    if action != "import" || id.len() > 64 {
        return Err("Invalid import action".into());
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let intent = {
        let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
        reserve_setup(&mut inner, &id, None, cancelled.clone())?.0
    };
    let shared = state.inner.clone();
    let flag = cancelled.clone();
    let uncertain = Arc::new(AtomicBool::new(false));
    let worker_uncertain = uncertain.clone();
    let worker = tauri::async_runtime::spawn_blocking(move || {
        let source = match choose(&window, flag.clone()) {
            Ok(Some(source)) if !flag.load(Ordering::Acquire) => source,
            _ => return writer_view("cancelled", "Import was not started.", true),
        };
        let Ok(import) = setup::ImportIntent::new(intent, source) else {
            return writer_view("attention", "The selected directory cannot be used. Refresh status to try again.", true);
        };
        let Ok(mut run) = runner::start_import_preview(&import) else {
            return writer_view("error", "Could not start import preview.", true);
        };
        let receipt = match wait_apply(&mut run, &flag) {
            Ok(receipt) => receipt,
            Err(view) => {
                if view.outcome == "cleanup_unresolved" { worker_uncertain.store(true, Ordering::Release); }
                return view;
            }
        };
        if flag.load(Ordering::Acquire) { return writer_view("cancelled", "Import was not started.", true); }
        if receipt.status.code() == Some(4) {
            if let Ok(mut inner) = shared.lock() { inner.recovery = Some(Vec::new()); }
            return writer_view("terminal_recovery", "Terminal recovery is required. Check dotman in a terminal before refreshing.", false);
        }
        let Ok(review) = import.parse_preview(&receipt.stdout, receipt.status.code()) else {
            return writer_view("attention", "Import preview was refused or could not be verified. Use the terminal importer for unsupported sources.", true);
        };
        let Ok(text) = review.confirmation() else {
            return writer_view("attention", "The complete import cannot be shown safely; nothing was imported.", true);
        };
        let approved = crate::confirm::ask(&window, "Import dotfiles snapshot?", text, flag.clone()).unwrap_or(false);
        if !approved || flag.load(Ordering::Acquire) {
            return writer_view("declined", "Import was not approved.", true);
        }
        let mut run = {
            let Ok(guard) = shared.lock() else { return writer_view("error", "Status state unavailable.", false); };
            if flag.load(Ordering::Acquire) { return writer_view("cancelled", "Import was not started.", true); }
            worker_uncertain.store(true, Ordering::Release);
            let Ok(run) = runner::start_import(&review) else {
                return writer_view("attention", "Import could not start; check status before retrying.", false);
            };
            drop(guard);
            run
        };
        let receipt = match wait_apply(&mut run, &flag) {
            Ok(receipt) => receipt,
            Err(mut view) => {
                view.message = "Import could not be verified; the repository may already exist. Check status in a terminal.";
                return view;
            }
        };
        worker_uncertain.store(false, Ordering::Release);
        if receipt.status.code() == Some(4) {
            if let Ok(mut inner) = shared.lock() { inner.recovery = Some(Vec::new()); }
            return writer_view("terminal_recovery", "Terminal recovery is required. Check dotman in a terminal before refreshing.", false);
        }
        let Ok(publication) = review.parse_publication(&receipt.stdout, receipt.status.code()) else {
            return writer_view("attention", "Import could not be verified; the repository may already exist. Refresh status manually.", false);
        };
        if flag.load(Ordering::Acquire) { return writer_view("attention", "Import may have completed. Refresh status manually.", false); }
        match run_status(&flag) {
            Ok(protocol::StatusReply::Report(after)) if review.setup().is_published_in(&publication, &after) =>
                writer_view("done", "Snapshot imported and verified. Review files before applying.", true),
            _ => writer_view("attention", "Import could not be verified; the repository may already exist. Refresh status manually.", false),
        }
    }).await;
    let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
    if !uncertain.load(Ordering::Acquire) {
        inner.active = None;
    }
    drop(inner);
    worker.map_err(|_| "Import worker ended unexpectedly; check status in a terminal.".into())
}
