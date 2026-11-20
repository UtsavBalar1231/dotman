use gtk::prelude::*;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

/// Opens a local-file chooser on the GTK main thread.
///
/// The callback runs once, including when the dialog is destroyed. Its path is
/// an untrusted selection: the CLI must independently validate the source.
pub fn choose_identity(
    parent: &gtk::Window,
    cancelled: Arc<AtomicBool>,
    complete: impl FnOnce(Option<PathBuf>) + 'static,
) {
    let dialog = gtk::FileChooserDialog::with_buttons(
        Some("Choose an age identity"),
        Some(parent),
        gtk::FileChooserAction::Open,
        &[
            ("Cancel", gtk::ResponseType::Cancel),
            ("Open", gtk::ResponseType::Accept),
        ],
    );
    dialog.set_modal(true);
    dialog.set_destroy_with_parent(true);
    dialog.set_local_only(true);
    dialog.set_select_multiple(false);
    dialog.set_default_response(gtk::ResponseType::Cancel);

    let complete = Rc::new(RefCell::new(Some(complete)));
    let on_response = complete.clone();
    dialog.connect_response(move |dialog, response| {
        let callback = on_response.borrow_mut().take();
        let path = selected(response, dialog.filename());
        if let Some(complete) = callback {
            dialog.close();
            complete(path);
        }
    });
    dialog.connect_destroy(move |_| {
        let callback = complete.borrow_mut().take();
        if let Some(complete) = callback {
            complete(None);
        }
    });
    let weak = dialog.downgrade();
    gtk::glib::timeout_add_local(Duration::from_millis(100), move || {
        let Some(dialog) = weak.upgrade() else {
            return gtk::glib::ControlFlow::Break;
        };
        if cancelled.load(Ordering::Acquire) {
            dialog.response(gtk::ResponseType::Cancel);
            return gtk::glib::ControlFlow::Break;
        }
        gtk::glib::ControlFlow::Continue
    });
    dialog.show_all();
}

fn selected(response: gtk::ResponseType, path: Option<PathBuf>) -> Option<PathBuf> {
    if response == gtk::ResponseType::Accept {
        path.filter(|path| path.is_absolute())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn selection_requires_accept_and_absolute_local_path() {
        let path = Some(PathBuf::from("/private/key"));
        assert_eq!(
            selected(gtk::ResponseType::Accept, path.clone()).as_deref(),
            Some(Path::new("/private/key"))
        );
        assert_eq!(selected(gtk::ResponseType::Cancel, path), None);
        assert_eq!(
            selected(gtk::ResponseType::Accept, Some(PathBuf::from("relative"))),
            None
        );
        assert_eq!(selected(gtk::ResponseType::Accept, None), None);
    }
}
