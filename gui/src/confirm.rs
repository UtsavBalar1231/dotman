mod outgoing;
mod tracking;
use dotman_gui::runner::CommitMessage;
use gtk::prelude::*;
pub use outgoing::{MessageDraft, OutgoingDetails};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;
pub use tracking::TrackingDetails;

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
    show(window, title, action, Content::Message(text), cancelled).map(|reply| reply.is_some())
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
        Content::Sync(prompt, text),
        cancelled,
    )
    .map(|reply| reply.is_some())
}

enum Content {
    Message(String),
    Outgoing(SyncPrompt, OutgoingDetails, Option<MessageDraft>),
    Sync(SyncPrompt, String),
    Tracking(SyncPrompt, TrackingDetails),
}

impl Content {
    fn build(&self, parent: &gtk::Window) -> (gtk::Dialog, Option<outgoing::Editor>) {
        let dialog = match self {
            Self::Outgoing(prompt, details, draft) => {
                return outgoing::dialog(parent, prompt, details, draft.as_ref());
            }
            Self::Message(text) => gtk::MessageDialog::new(
                Some(parent),
                gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
                gtk::MessageType::Question,
                gtk::ButtonsType::None,
                text,
            )
            .upcast(),
            Self::Sync(prompt, text) => sync_dialog(parent, prompt, text),
            Self::Tracking(prompt, details) => tracking::dialog(parent, prompt, details),
        };
        (dialog, None)
    }
}

pub struct Approval {
    pub message: Option<CommitMessage>,
}

pub fn ask_outgoing(
    window: &tauri::WebviewWindow,
    prompt: SyncPrompt,
    details: OutgoingDetails,
    draft: Option<MessageDraft>,
    cancelled: Arc<AtomicBool>,
) -> Result<Option<Approval>, &'static str> {
    show(
        window,
        prompt.title,
        prompt.action,
        Content::Outgoing(prompt, details, draft),
        cancelled,
    )
}

pub fn ask_tracking(
    window: &tauri::WebviewWindow,
    prompt: SyncPrompt,
    details: TrackingDetails,
    cancelled: Arc<AtomicBool>,
) -> Result<bool, &'static str> {
    show(
        window,
        prompt.title,
        prompt.action,
        Content::Tracking(prompt, details),
        cancelled,
    )
    .map(|reply| reply.is_some())
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
                "Repository"
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

fn styled_dialog(parent: &gtk::Window, prompt: &SyncPrompt) -> gtk::Dialog {
    let dialog = gtk::Dialog::with_buttons(
        Some(prompt.title),
        Some(parent),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        &[],
    );
    let (width, height) = parent.size();
    let wide = width >= 1920 && height >= 720;
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
            if wide {
                context.add_class("wide");
            }
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
    dialog
}

fn sync_dialog(parent: &gtk::Window, prompt: &SyncPrompt, text: &str) -> gtk::Dialog {
    let dialog = styled_dialog(parent, prompt);
    let (width, height) = parent.size();
    let wide = width >= 1920 && height >= 720;
    let (max_width, max_height) = if wide { (870, 690) } else { (580, 460) };
    dialog.set_default_size(width.clamp(260, max_width), height.clamp(240, max_height));
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

fn configure_actions(dialog: &gtk::Dialog, title: &str, action: &str, content: &Content) {
    dialog.set_title(title);
    let cancel = dialog.add_button("Cancel", gtk::ResponseType::Cancel);
    let accept = dialog.add_button(action, gtk::ResponseType::Yes);
    if matches!(content, Content::Tracking(_, _) | Content::Outgoing(..))
        && let Some(actions) = accept
            .parent()
            .and_then(|p| p.downcast::<gtk::ButtonBox>().ok())
    {
        actions.set_child_non_homogeneous(&cancel, true);
        actions.set_child_non_homogeneous(&accept, true);
        actions.set_spacing(8);
    }
    if let Content::Sync(prompt, _) | Content::Tracking(prompt, _) | Content::Outgoing(prompt, ..) =
        content
    {
        accept.style_context().add_class(if prompt.destructive {
            "destructive-action"
        } else {
            "suggested-action"
        });
    }
    dialog.set_default_response(gtk::ResponseType::Cancel);
}

fn show(
    window: &tauri::WebviewWindow,
    title: &str,
    action: &'static str,
    content: Content,
    cancelled: Arc<AtomicBool>,
) -> Result<Option<Approval>, &'static str> {
    if window.label() != "main" {
        return Err("Could not show confirmation");
    }
    let title = title.to_owned();
    let (sender, receiver) = mpsc::channel();
    window
        .with_webview(move |platform| {
            if cancelled.load(Ordering::Acquire) {
                let _ = sender.send(None);
                return;
            }
            let webview = platform.inner();
            let Some(parent) = webview
                .toplevel()
                .and_then(|widget| widget.downcast::<gtk::Window>().ok())
            else {
                return;
            };
            let (dialog, editor) = content.build(&parent);
            configure_actions(&dialog, &title, action, &content);
            if let Some(editor) = &editor {
                editor.bind(&dialog);
            }
            let completion = Rc::new(RefCell::new(Some(sender)));
            let on_response = completion.clone();
            let response_cancelled = cancelled.clone();
            let handler = Rc::new(RefCell::new(None));
            let response_handler = handler.clone();
            let signal = dialog.connect_response(move |dialog, response| {
                let result = if approved(response, response_cancelled.load(Ordering::Acquire)) {
                    match editor.as_ref().map(outgoing::Editor::message).transpose() {
                        Ok(message) => Some(Approval {
                            message: message.flatten(),
                        }),
                        Err(_) => return,
                    }
                } else {
                    None
                };
                let sender = on_response.borrow_mut().take();
                if let Some(sender) = sender {
                    let handler = response_handler.borrow_mut().take();
                    if let Some(handler) = handler {
                        dialog.disconnect(handler);
                    }
                    let _ = sender.send(result);
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
                    let _ = sender.send(None);
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
