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
    show(window, title, action, text, cancelled, None)
}

pub struct SyncPrompt {
    pub title: &'static str,
    pub action: &'static str,
    pub scope: &'static str,
    pub destructive: bool,
    pub dark: Option<bool>,
}

pub fn ask_sync(
    window: &tauri::WebviewWindow,
    prompt: SyncPrompt,
    text: String,
    cancelled: Arc<AtomicBool>,
) -> Result<bool, &'static str> {
    show(
        window,
        prompt.title,
        prompt.action,
        text,
        cancelled,
        Some(prompt),
    )
}

fn sync_details(text: &str, accessible_title: &str) -> gtk::TextView {
    let buffer = gtk::TextBuffer::default();
    buffer.set_text(text);
    let field = gtk::TextTag::builder()
        .weight(700)
        .family("sans-serif")
        .pixels_below_lines(4)
        .build();
    if let Some(table) = buffer.tag_table() {
        table.add(&field);
        for (index, line) in text.lines().enumerate() {
            if matches!(
                line,
                "Capture future files in"
                    | "Remove folder rules"
                    | "Remove repository sources"
                    | "Track files"
                    | "Held back"
                    | "Repository"
                    | "Capture from this computer"
                    | "Repository changes"
                    | "Local commits to publish"
                    | "Edited commit message"
                    | "Suggested commit message (may be regenerated)"
                    | "Sync scope"
                    | "Approve these exact repository deletions:"
            ) && let Ok(row) = i32::try_from(index)
            {
                let start = buffer.iter_at_line(row);
                let mut end = start;
                end.forward_to_line_end();
                buffer.apply_tag(&field, &start, &end);
            }
        }
    }
    let details = gtk::TextView::builder()
        .buffer(&buffer)
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .monospace(true)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(14)
        .right_margin(14)
        .pixels_below_lines(3)
        .build();
    if let Some(accessible) = details.accessible() {
        accessible.set_name(accessible_title);
    }
    details
}

fn sync_dialog(parent: &gtk::Window, prompt: &SyncPrompt, text: &str) -> gtk::Dialog {
    let dialog = gtk::Dialog::with_buttons(
        Some(prompt.title),
        Some(parent),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[],
    );
    if let Some(dark) = prompt.dark
        && let Some(screen) = gtk::prelude::WidgetExt::screen(&dialog)
    {
        let style = gtk::CssProvider::new();
        if style
            .load_from_data(include_bytes!("sync-confirm.css"))
            .is_ok()
        {
            let context = dialog.style_context();
            context.add_class("dotman-sync");
            if dark {
                context.add_class("amoled");
            }
            gtk::StyleContext::add_provider_for_screen(
                &screen,
                &style,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            dialog.connect_destroy(move |_| {
                gtk::StyleContext::remove_provider_for_screen(&screen, &style);
            });
        }
    }
    let (width, height) = parent.size();
    dialog.set_default_size(width.clamp(260, 580), height.clamp(240, 460));
    let content = dialog.content_area();
    content.set_border_width(18);
    content.set_spacing(12);
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    let symbol = if prompt.destructive {
        "dialog-warning-symbolic"
    } else {
        "emblem-synchronizing-symbolic"
    };
    header.pack_start(
        &gtk::Image::from_icon_name(Some(symbol), gtk::IconSize::Dialog),
        false,
        false,
        0,
    );
    let intro = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let heading = gtk::Label::new(None);
    let escaped = gtk::glib::markup_escape_text(prompt.title);
    heading.set_markup(&format!(
        "<span size='large' weight='bold'>{escaped}</span>"
    ));
    heading.set_xalign(0.0);
    let scope = gtk::Label::builder()
        .label(prompt.scope)
        .xalign(0.0)
        .wrap(true)
        .build();
    intro.pack_start(&heading, false, false, 0);
    intro.pack_start(&scope, false, false, 0);
    header.pack_start(&intro, true, true, 0);
    content.pack_start(&header, false, false, 0);

    let details = sync_details(text, prompt.title);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .shadow_type(gtk::ShadowType::In)
        .build();
    scroll.add(&details);
    content.pack_start(&scroll, true, true, 0);
    dialog
}

fn show(
    window: &tauri::WebviewWindow,
    title: &str,
    action: &'static str,
    text: String,
    cancelled: Arc<AtomicBool>,
    sync: Option<SyncPrompt>,
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
            let dialog = sync.as_ref().map_or_else(
                || {
                    gtk::MessageDialog::new(
                        Some(&parent),
                        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
                        gtk::MessageType::Question,
                        gtk::ButtonsType::None,
                        &text,
                    )
                    .upcast()
                },
                |prompt| sync_dialog(&parent, prompt, &text),
            );
            dialog.set_title(&title);
            dialog.add_button("Cancel", gtk::ResponseType::Cancel);
            let accept = dialog.add_button(action, gtk::ResponseType::Yes);
            if let Some(prompt) = &sync {
                accept.style_context().add_class(if prompt.destructive {
                    "destructive-action"
                } else {
                    "suggested-action"
                });
            }
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
