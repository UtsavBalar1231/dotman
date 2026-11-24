use super::{SyncPrompt, styled_dialog, tracking::label};
use dotman_gui::runner::CommitMessage;
use gtk::prelude::*;

#[derive(Default)]
pub struct OutgoingDetails {
    pub repository: String,
    pub captures: Vec<String>,
    pub changes: Vec<String>,
    pub commits: Vec<String>,
    pub selected: Option<String>,
    pub deletions: Vec<String>,
    pub bytes: usize,
}

pub struct MessageDraft {
    pub suggested: String,
    pub initial: Option<CommitMessage>,
}

pub(super) struct Editor {
    buffer: gtk::TextBuffer,
    hint: gtk::Label,
    restore: gtk::Button,
    suggestion: String,
}

fn validate(buffer: &gtk::TextBuffer) -> Result<CommitMessage, &'static str> {
    let (start, end) = buffer.bounds();
    let value = buffer
        .text(&start, &end, true)
        .ok_or("Enter a commit message.")?;
    CommitMessage::new(value.to_string())
}

impl Editor {
    pub(super) fn message(&self) -> Result<Option<CommitMessage>, &'static str> {
        Ok(Some(validate(&self.buffer)?).filter(|m| m.as_str() != self.suggestion))
    }

    pub(super) fn bind(&self, dialog: &gtk::Dialog) {
        let weak = dialog.downgrade();
        let hint = self.hint.clone();
        let restore = self.restore.downgrade();
        let suggestion = self.suggestion.clone();
        let update = move |buffer: &gtk::TextBuffer| {
            let Some(dialog) = weak.upgrade() else { return };
            let result = validate(buffer);
            let changed = result.as_ref().is_ok_and(|m| m.as_str() != suggestion);
            if let Some(restore) = restore.upgrade() {
                restore.set_sensitive(changed || result.is_err());
            }
            hint.set_text(match &result {
                Err(error) => error,
                Ok(_) if changed => "This message will be used if sync creates a commit.",
                Ok(_) => "Suggested by Dotman. Edit to set your own message.",
            });
            dialog.set_response_sensitive(gtk::ResponseType::Yes, result.is_ok());
        };
        update(&self.buffer);
        self.buffer.connect_changed(update);
    }
}

fn compose(body: &gtk::Box, draft: &MessageDraft) -> Editor {
    let group = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let heading = label("Commit message", "compose-title");
    let restore = gtk::Button::with_label("Use suggestion");
    restore.style_context().add_class("compose-restore");
    group.pack_start(&heading, true, true, 0);
    group.pack_end(&restore, false, false, 0);
    body.pack_start(&group, false, false, 0);

    let buffer = gtk::TextBuffer::default();
    buffer.set_text(
        draft
            .initial
            .as_ref()
            .map_or(draft.suggested.as_str(), CommitMessage::as_str),
    );
    let field = gtk::TextView::builder()
        .buffer(&buffer)
        .accepts_tab(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(12)
        .right_margin(12)
        .top_margin(9)
        .bottom_margin(9)
        .build();
    if let Some(a) = field.accessible() {
        a.set_name("Commit message");
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(78)
        .build();
    scroller.style_context().add_class("compose-input");
    scroller.add(&field);
    body.pack_start(&scroller, false, false, 0);
    let hint = label("", "tracking-caption");
    body.pack_start(&hint, false, false, 0);
    let initial = draft.suggested.clone();
    let field_weak = field.downgrade();
    restore.connect_clicked(move |_| {
        if let Some(field) = field_weak.upgrade()
            && let Some(buffer) = field.buffer()
        {
            buffer.set_text(&initial);
            field.grab_focus();
        }
    });
    Editor {
        buffer,
        hint,
        restore,
        suggestion: draft.suggested.clone(),
    }
}

fn section(body: &gtk::Box, title: &str, rows: &[String], expanded: bool) {
    if rows.is_empty() {
        return;
    }
    let group = gtk::Expander::new(Some(&format!("{title} ({})", rows.len())));
    group.style_context().add_class("outgoing-group");
    let paths = label(&rows.join("\n"), "outgoing-paths");
    paths.set_selectable(true);
    group.add(&paths);
    group.set_expanded(expanded);
    body.pack_start(&group, false, false, 0);
}

pub(super) fn dialog(
    parent: &gtk::Window,
    prompt: &SyncPrompt,
    details: &OutgoingDetails,
    draft: Option<&MessageDraft>,
) -> (gtk::Dialog, Option<Editor>) {
    let dialog = styled_dialog(parent, prompt);
    let context = dialog.style_context();
    context.add_class("tracking-confirm");
    context.add_class("outgoing-confirm");
    let (width, height) = parent.size();
    let wide = width >= 1920 && height >= 720;
    dialog.set_default_size(
        width
            .saturating_sub(32)
            .clamp(260, if wide { 800 } else { 620 }),
        height
            .saturating_sub(40)
            .clamp(260, if wide { 630 } else { 530 }),
    );
    let area = dialog.content_area();
    area.set_border_width(if width < 500 { 12 } else { 22 });
    area.set_spacing(8);
    area.pack_start(&label(prompt.title, "tracking-title"), false, false, 0);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.pack_start(&label(prompt.scope, "tracking-caption"), false, false, 0);
    let editor = draft.map(|draft| compose(&body, draft));
    if !details.repository.is_empty() {
        let fact = gtk::Box::new(gtk::Orientation::Vertical, 3);
        fact.style_context().add_class("tracking-facts");
        fact.pack_start(&label("Repository", "tracking-caption"), false, false, 0);
        let path = label(&details.repository, "tracking-value");
        path.set_selectable(true);
        fact.pack_start(&path, false, false, 0);
        body.pack_start(&fact, false, false, 0);
    }
    section(
        &body,
        "Repository deletions to approve",
        &details.deletions,
        true,
    );
    if let Some(selected) = &details.selected {
        let scope = label(selected, "outgoing-paths");
        scope.set_selectable(true);
        body.pack_start(&scope, false, false, 0);
    } else {
        section(
            &body,
            "Capture from this computer",
            &details.captures,
            false,
        );
        section(&body, "Repository changes", &details.changes, false);
        section(&body, "Local commits to publish", &details.commits, false);
        if draft.is_none()
            && !details.commits.is_empty()
            && details.changes.is_empty()
            && details.captures.is_empty()
        {
            body.pack_start(
                &label(
                    "These commits already exist. Their messages are part of their history.",
                    "tracking-caption",
                ),
                false,
                false,
                0,
            );
        }
        body.pack_start(&label("Incoming changes and the configured origin address are unverified. Sync excludes system files.", "tracking-caption"), false, false, 0);
    }
    scroll.add(&body);
    area.pack_start(&scroll, true, true, 0);
    (dialog, editor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a GTK display"]
    fn confirmation_edits_validate_and_restore_the_generated_message()
    -> Result<(), Box<dyn std::error::Error>> {
        gtk::init()?;
        let draft = MessageDraft {
            suggested: "Generated suggestion".into(),
            initial: Some(CommitMessage::new("Drawer draft".into())?),
        };
        let dialog = gtk::Dialog::new();
        let approve = dialog.add_button("Sync", gtk::ResponseType::Yes);
        let editor = compose(&dialog.content_area(), &draft);
        editor.bind(&dialog);
        assert_eq!(
            editor.message()?.as_ref().map(CommitMessage::as_str),
            Some("Drawer draft")
        );
        editor.buffer.set_text("");
        assert!(!approve.is_sensitive());
        assert!(editor.message().is_err());
        editor.restore.emit_clicked();
        assert!(approve.is_sensitive());
        assert!(editor.message()?.is_none());
        editor.buffer.set_text("Final title\n\nFinal body");
        assert_eq!(
            editor.message()?.as_ref().map(CommitMessage::as_str),
            Some("Final title\n\nFinal body")
        );
        dialog.close();
        Ok(())
    }
}
