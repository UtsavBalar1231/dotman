#![deny(unsafe_code)]

mod app;
mod confirm;
mod folder_chooser;
mod key_chooser;

use std::os::unix::process::CommandExt;
use tauri::Manager;

fn bundled_document(url: &str) -> bool {
    matches!(url, "tauri://localhost" | "tauri://localhost/")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("DOTMAN_AGE_KEY").is_some() {
        let executable = std::env::current_exe()?;
        if !executable.is_absolute() {
            return Err("GUI executable path is not absolute".into());
        }
        let error = std::process::Command::new(executable)
            .args(std::env::args_os().skip(1))
            .env_remove("DOTMAN_AGE_KEY")
            .env("DOTMAN_GUI_KEY_STRIPPED", "1")
            .exec();
        return Err(error.into());
    }
    let stripped = std::env::var_os("DOTMAN_GUI_KEY_STRIPPED").is_some();
    tauri::Builder::default()
        .manage(app::AppState::new(stripped))
        .invoke_handler(tauri::generate_handler![
            app::refresh,
            app::cancel,
            app::open_diff,
            app::open_review,
            app::discard_review,
            app::run_reviewed_sync,
            app::apply_reviewed_files,
            app::accept_reviewed_identity,
            app::policy_trust::review_policy,
            app::approve_pending_hooks,
            app::run_fresh_setup,
            app::run_clone_setup,
            app::import_setup::run_import_setup,
            app::keys::run_key,
            app::held::run_held,
            app::variant::run_host_variant
        ])
        .setup(|app| {
            tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::default())
                .title("dotman")
                .inner_size(1000.0, 720.0)
                .min_inner_size(320.0, 320.0)
                .devtools(false)
                .on_navigation(|url| bundled_document(url.as_str()))
                .on_new_window(|_, _| tauri::webview::NewWindowResponse::Deny)
                .build()?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if matches!(
                event,
                tauri::WindowEvent::CloseRequested { .. } | tauri::WindowEvent::Destroyed
            ) {
                let _ = window.state::<app::AppState>().stop();
            }
        })
        .run(tauri::generate_context!())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::bundled_document;

    #[test]
    fn navigation_accepts_only_bundled_base_document() {
        for url in ["tauri://localhost", "tauri://localhost/"] {
            assert!(bundled_document(url));
        }
        for url in [
            "tauri://localhost/index.html",
            "tauri://localhost/app.js",
            "tauri://localhost/?x=1",
            "tauri://localhost/#fragment",
            "tauri://user@localhost/",
            "tauri://localhost:80/",
            "https://localhost/",
            "tauri://localhost.evil/",
        ] {
            assert!(!bundled_document(url));
        }
    }
}
