use gtk::prelude::*;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

pub fn choose(
    parent: &gtk::Window,
    cancelled: Arc<AtomicBool>,
    complete: impl FnOnce(Option<PathBuf>) + 'static,
) {
    choose_paths(
        parent,
        cancelled,
        "Choose a dotfiles directory",
        true,
        None,
        move |paths| complete(paths.into_iter().next()),
    );
}

pub fn choose_paths(
    parent: &gtk::Window,
    cancelled: Arc<AtomicBool>,
    title: &'static str,
    folder: bool,
    home: Option<&std::path::Path>,
    complete: impl FnOnce(Vec<PathBuf>) + 'static,
) {
    let dialog = gtk::FileChooserDialog::with_buttons(
        Some(title),
        Some(parent),
        if folder {
            gtk::FileChooserAction::SelectFolder
        } else {
            gtk::FileChooserAction::Open
        },
        &[
            ("Cancel", gtk::ResponseType::Cancel),
            ("Select", gtk::ResponseType::Accept),
        ],
    );
    dialog.set_modal(true);
    dialog.set_destroy_with_parent(true);
    dialog.set_local_only(true);
    dialog.set_select_multiple(!folder);
    if let Some(home) = home {
        dialog.set_show_hidden(true);
        dialog.set_current_folder(home);
    }
    dialog.set_default_response(gtk::ResponseType::Cancel);
    let complete = Rc::new(RefCell::new(Some(complete)));
    let response = complete.clone();
    dialog.connect_response(move |dialog, answer| {
        let callback = response.borrow_mut().take();
        if let Some(callback) = callback {
            let path = dialog
                .filenames()
                .into_iter()
                .filter_map(|p| selected(answer, Some(p)))
                .collect();
            dialog.close();
            callback(path);
        }
    });
    dialog.connect_destroy(move |_| {
        if let Some(callback) = complete.borrow_mut().take() {
            callback(Vec::new());
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

fn selected(answer: gtk::ResponseType, path: Option<PathBuf>) -> Option<PathBuf> {
    if answer == gtk::ResponseType::Accept {
        path.filter(|path| path.is_absolute())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::selected;
    use std::path::PathBuf;

    #[test]
    fn chooser_accepts_only_an_absolute_selection() {
        assert_eq!(
            selected(gtk::ResponseType::Accept, Some(PathBuf::from("/selected"))),
            Some(PathBuf::from("/selected"))
        );
        assert_eq!(
            selected(gtk::ResponseType::Cancel, Some(PathBuf::from("/selected"))),
            None
        );
        assert_eq!(
            selected(gtk::ResponseType::Accept, Some(PathBuf::from("relative"))),
            None
        );
    }
}
