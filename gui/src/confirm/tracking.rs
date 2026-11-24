use super::{SyncPrompt, styled_dialog};
use gtk::prelude::*;
use std::collections::BTreeMap;

pub struct TrackingDetails {
    pub repository: String,
    pub root: Option<String>,
    pub folders: Vec<String>,
    pub files: Vec<String>,
    pub omitted: Vec<String>,
    pub held: BTreeMap<String, String>,
}

pub(super) fn label(text: &str, class: &str) -> gtk::Label {
    let value = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .max_width_chars(1)
        .build();
    value.style_context().add_class(class);
    value
}

fn fact(body: &gtk::Box, name: &str, value: &str) {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 3);
    row.style_context().add_class("tracking-fact");
    row.pack_start(&label(name, "tracking-caption"), false, false, 0);
    let content = label(value, "tracking-value");
    content.set_selectable(true);
    row.pack_start(&content, false, false, 0);
    body.pack_start(&row, false, false, 0);
}

fn section<'a>(
    body: &gtk::Box,
    title: &str,
    paths: impl ExactSizeIterator<Item = (&'a str, Option<&'a str>)>,
    root: Option<&str>,
    expanded: bool,
    note: Option<&str>,
) {
    let count = paths.len();
    if count == 0 {
        return;
    }
    let group = gtk::ToggleButton::new();
    group.style_context().add_class("tracking-disclosure");
    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let arrow = gtk::Image::new();
    arrow.set_pixel_size(14);
    heading.pack_start(&arrow, false, false, 0);
    heading.pack_start(&label(title, "tracking-group-title"), true, true, 0);
    heading.pack_end(&gtk::Label::new(Some(&count.to_string())), false, false, 0);
    group.add(&heading);
    if let Some(accessible) = group.accessible() {
        accessible.set_name(&format!("{title} ({count})"));
    }
    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    if let Some(note) = note {
        list.pack_start(&label(note, "tracking-caption"), false, false, 0);
    }
    let prefix = root.map(|dir| format!("{dir}/"));
    for (path, reason) in paths {
        let display = prefix
            .as_deref()
            .and_then(|dir| path.strip_prefix(dir))
            .unwrap_or(path);
        let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
        row.style_context().add_class("tracking-path");
        let name = label(display, "tracking-value");
        name.set_selectable(true);
        name.set_tooltip_text(Some(path));
        row.pack_start(&name, false, false, 0);
        if let Some(reason) = reason {
            row.pack_start(&label(reason, "tracking-caption"), false, false, 0);
        }
        list.pack_start(&row, false, false, 0);
    }
    let reveal = gtk::Revealer::new();
    reveal.set_transition_type(gtk::RevealerTransitionType::None);
    reveal.add(&list);
    body.pack_start(&group, false, false, 0);
    body.pack_start(&reveal, false, false, 0);
    let display = move |button: &gtk::ToggleButton| {
        let open = button.is_active();
        let icon = if open {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        };
        arrow.set_from_icon_name(Some(icon), gtk::IconSize::Menu);
        reveal.set_reveal_child(open);
    };
    group.set_active(expanded);
    display(&group);
    group.connect_toggled(display);
}

impl TrackingDetails {
    fn title(&self, stop: bool) -> String {
        let count = self.files.len();
        match (stop, count) {
            (false, 0) => "Enable folder tracking?".into(),
            (true, 0) => "Remove folder tracking?".into(),
            _ => {
                let verb = if stop { "Stop tracking" } else { "Track" };
                let noun = if count == 1 { "file" } else { "files" };
                format!("{verb} {count} {noun}?")
            }
        }
    }
}

pub(super) fn dialog(
    parent: &gtk::Window,
    prompt: &SyncPrompt,
    details: &TrackingDetails,
) -> gtk::Dialog {
    let dialog = styled_dialog(parent, prompt);
    dialog.style_context().add_class("tracking-confirm");
    let (width, height) = parent.size();
    let wide = width >= 1920 && height >= 720;
    let inset = if width < 500 { 12 } else { 24 };
    let dialog_width = width
        .saturating_sub(24)
        .clamp(260, if wide { 710 } else { 600 });
    let content = dialog.content_area();
    content.set_border_width(inset);
    content.set_spacing(12);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.pack_start(
        &label(&details.title(prompt.destructive), "tracking-title"),
        false,
        false,
        0,
    );
    content.pack_start(&label(prompt.scope, "tracking-scope"), false, false, 0);
    let facts = gtk::Box::new(gtk::Orientation::Vertical, 0);
    facts.style_context().add_class("tracking-facts");
    fact(&facts, "Repository", &details.repository);
    if !details.folders.is_empty() {
        let name = if prompt.destructive {
            "Remove folder rules"
        } else {
            "Capture future files in"
        };
        fact(&facts, name, &details.folders.join("\n"));
    } else if let Some(root) = &details.root {
        fact(&facts, "Selected folder", root);
        facts.pack_start(
            &label(
                "This selection adds no new folder capture rule.",
                "tracking-caption",
            ),
            false,
            false,
            0,
        );
    }
    body.pack_start(&facts, false, false, 0);
    let title = if prompt.destructive {
        "Repository sources to remove"
    } else {
        "Selected files"
    };
    section(
        &body,
        title,
        details.files.iter().map(|path| (path.as_str(), None)),
        details.root.as_deref(),
        details.files.len() <= 3,
        None,
    );
    section(
        &body,
        "Leave untracked",
        details.omitted.iter().map(|path| (path.as_str(), None)),
        details.root.as_deref(),
        false,
        Some("Folder rules covering these paths will ignore them."),
    );
    section(
        &body,
        "Held back",
        details
            .held
            .iter()
            .map(|(path, why)| (path.as_str(), Some(why.as_str()))),
        details.root.as_deref(),
        false,
        None,
    );
    scroll.add(&body);
    content.pack_start(&scroll, true, true, 0);
    content.show_all();
    let inner_width =
        dialog_width.saturating_sub(i32::try_from(inset.saturating_mul(2)).unwrap_or(48));
    let (_, form_height) = body.preferred_height_for_width(inner_width);
    let (_, header_height) = content.preferred_height_for_width(dialog_width);
    let desired = form_height.saturating_add(header_height);
    dialog.set_default_size(
        dialog_width,
        desired.clamp(220, height.saturating_sub(32).max(220)),
    );
    dialog
}
