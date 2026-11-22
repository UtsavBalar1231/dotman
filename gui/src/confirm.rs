use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;

fn approved(response: gtk::ResponseType, cancelled: bool) -> bool {
    response == gtk::ResponseType::Yes && !cancelled
}

pub fn ask(
    window: &tauri::WebviewWindow,
    title: &str,
    action: &'static str,
    text: String,
    cancelled: Arc<AtomicBool>,
) -> Result<bool, &'static str> {
    if window.label() != "main" {
        return Err("Could not show confirmation");
    }
    let title = title.to_owned();
    let (sender, receiver) = mpsc::channel();
    window
        .with_webview(move |platform| {
            if cancelled.load(Ordering::Acquire) {
                let _ = sender.send(false);
                return;
            }
            let webview = platform.inner();
            let Some(parent) = webview
                .toplevel()
                .and_then(|widget| widget.downcast::<gtk::Window>().ok())
            else {
                return;
            };
            let dialog = gtk::MessageDialog::new(
                Some(&parent),
                gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
                gtk::MessageType::Question,
                gtk::ButtonsType::None,
                &text,
            );
            dialog.set_title(&title);
            dialog.add_button("Cancel", gtk::ResponseType::Cancel);
            dialog.add_button(action, gtk::ResponseType::Yes);
            dialog.set_default_response(gtk::ResponseType::Cancel);
            let completion = Rc::new(RefCell::new(Some(sender)));
            let on_response = completion.clone();
            let response_cancelled = cancelled.clone();
            let handler = Rc::new(RefCell::new(None));
            let response_handler = handler.clone();
            let signal = dialog.connect_response(move |dialog, response| {
                let sender = on_response.borrow_mut().take();
                if let Some(sender) = sender {
                    let handler = response_handler.borrow_mut().take();
                    if let Some(handler) = handler {
                        dialog.disconnect(handler);
                    }
                    let _ = sender.send(approved(
                        response,
                        response_cancelled.load(Ordering::Acquire),
                    ));
                    dialog.close();
                }
            });
            *handler.borrow_mut() = Some(signal);
            let on_destroy = completion;
            let parent_weak = parent.downgrade();
            let webview_weak = webview.downgrade();
            dialog.connect_destroy(move |_| {
                let sender = on_destroy.borrow_mut().take();
                if let Some(sender) = sender {
                    let _ = sender.send(false);
                }
                let parent = parent_weak.clone();
                let webview = webview_weak.clone();
                gtk::glib::idle_add_local_once(move || {
                    if let (Some(parent), Some(webview)) = (parent.upgrade(), webview.upgrade())
                        && parent.is_active()
                    {
                        webview.grab_focus();
                    }
                });
            });
            let weak_dialog = dialog.downgrade();
            gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
                let Some(dialog) = weak_dialog.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                if cancelled.load(Ordering::Acquire) {
                    dialog.response(gtk::ResponseType::Cancel);
                    return gtk::glib::ControlFlow::Break;
                }
                gtk::glib::ControlFlow::Continue
            });
            dialog.show_all();
            if let Some(cancel) = dialog.widget_for_response(gtk::ResponseType::Cancel) {
                cancel.grab_focus();
            }
        })
        .map_err(|_| "Could not show confirmation")?;
    receiver.recv().map_err(|_| "Could not show confirmation")
}

#[cfg(test)]
mod tests {
    use super::approved;

    #[test]
    fn only_uncancelled_yes_approves() {
        assert!(approved(gtk::ResponseType::Yes, false));
        assert!(!approved(gtk::ResponseType::Yes, true));
        for response in [
            gtk::ResponseType::No,
            gtk::ResponseType::Cancel,
            gtk::ResponseType::DeleteEvent,
            gtk::ResponseType::None,
        ] {
            assert!(!approved(response, false));
        }
    }
}
