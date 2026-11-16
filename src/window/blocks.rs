//! Small building blocks shared by the pages: labels, icon squares and
//! buttons, page links, the confirm dialog, picking and dropping files to send.
//!
//! Sibling edges: none (the parent's helpers only).

use super::*;

// ── Small building blocks shared by the pages ─────────────────────────────

/// A label with classes, left-aligned.
pub(super) fn text(label: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(label)
        .xalign(0.0)
        .use_markup(false)
        .css_classes(classes.to_vec())
        .build()
}

/// The same, ellipsized at the end. When it is cut, hovering it (or Orca's
/// description) gives the whole text.
pub(super) fn ellipsized(label: &str, classes: &[&str]) -> gtk::Label {
    let label = text(label, classes);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    tooltip_when_cut(&label);
    label
}

/// Hovering `label` gives its whole text when it is drawn cut short (it
/// shows nothing while it fits).
pub(super) fn tooltip_when_cut(label: &gtk::Label) {
    label.set_has_tooltip(true);
    label.connect_query_tooltip(|label, _, _, _, tooltip| {
        if label.layout().is_ellipsized() {
            tooltip.set_text(Some(&label.text()));
            true
        } else {
            false
        }
    });
}

/// The same, wrapping onto more lines instead of being cut: for what the
/// person must read in full (a status, a reason, a name that matters).
pub(super) fn wrapped(label: &str, classes: &[&str]) -> gtk::Label {
    let label = text(label, classes);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label
}

/// A rounded square with an icon in it: a card's or row's badge.
pub(super) fn icon_square(icon: &str, size: i32, class: &str) -> gtk::Box {
    let square = gtk::Box::builder()
        .width_request(size)
        .height_request(size)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .css_classes([class])
        .build();
    square.set_hexpand(false);
    square.append(
        &gtk::Image::builder()
            .icon_name(icon)
            .hexpand(true)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    square
}

/// A button with an icon and a label.
pub(super) fn icon_button(icon: &str, label: &str, classes: &[&str]) -> gtk::Button {
    let content = gtk::Box::builder()
        .spacing(7)
        .halign(gtk::Align::Center)
        .build();
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(&gtk::Label::new(Some(label)));
    let button = gtk::Button::builder()
        .child(&content)
        .valign(gtk::Align::Center)
        .css_classes(classes.to_vec())
        .build();
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button
}

/// "View All ›": an accent link to another page.
pub(super) fn page_link(label: &str, page: &str) -> gtk::Button {
    let content = gtk::Box::builder().spacing(2).build();
    content.append(&gtk::Label::new(Some(label)));
    content.append(&gtk::Image::from_icon_name("go-next-symbolic"));
    let button = gtk::Button::builder()
        .child(&content)
        .valign(gtk::Align::Center)
        .action_name("win.show-page")
        .action_target(&page.to_variant())
        .css_classes(["link-button"])
        .build();
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button
}

/// Asks before `confirm`ing something that cuts the phone off.
pub(crate) fn confirm(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    action: &str,
    confirm: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[("cancel", "_Cancel"), ("go", action)]);
    dialog.set_response_appearance("go", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.choose(parent, gio::Cancellable::NONE, move |response| {
        if response == "go" {
            confirm();
        }
    });
}

/// Where the last files sent from were picked: `~/.config/desklink/send-folder`.
fn send_folder_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("desklink").join("send-folder"))
}

/// A file dialog for sending, opened in the folder files were last sent from.
pub(crate) fn send_files_dialog(title: &str) -> gtk::FileDialog {
    let dialog = gtk::FileDialog::builder().title(title).modal(true).build();
    let folder = send_folder_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(std::path::PathBuf::from)
        .filter(|folder| folder.is_dir());
    if let Some(folder) = folder {
        dialog.set_initial_folder(Some(&gio::File::for_path(folder)));
    }
    dialog
}

/// Remembers the folder of the first of `files` for [`send_files_dialog`].
pub(crate) fn remember_send_folder(files: &gio::ListModel) {
    let folder = files
        .item(0)
        .and_downcast::<gio::File>()
        .and_then(|file| file.path())
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf));
    if let (Some(folder), Some(path)) = (folder, send_folder_path()) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, folder.to_string_lossy().as_bytes());
    }
}

/// Asks for files and sends each to `id`.
pub(super) fn pick_and_send_files(daemon: &DaemonHandle, id: &str, widget: &impl IsA<gtk::Widget>) {
    let Some(root) = widget.root().and_downcast::<gtk::Window>() else {
        return;
    };
    let dialog = send_files_dialog("Select Files to Send");
    let (daemon, id) = (daemon.clone(), id.to_string());
    dialog.open_multiple(Some(&root), gio::Cancellable::NONE, move |result| {
        let Ok(files) = result else {
            return;
        };
        remember_send_folder(&files);
        for file in files.iter::<gio::File>().flatten() {
            if let Some(path) = file.path() {
                debug_log!("[UI] Selected file to send: {:?}", path);
                daemon.send(DaemonCommand::SendFile(id.clone(), path));
            }
        }
    });
}

/// Lets files be dropped on `target`, sent to the device `resolve` names
/// (with the daemon), or refused with a word why when it names none.
/// Folders are refused too: only files travel.
pub(super) fn accept_dropped_files(
    target: &impl IsA<gtk::Widget>,
    resolve: impl Fn() -> Option<(DaemonHandle, String)> + 'static,
) {
    accept_dropped_files_hover(target, resolve, |_| {});
}

/// [`accept_dropped_files`], and `hover(true)` while files are dragged over
/// `target` and a phone can take them, `hover(false)` when they leave or drop.
pub(super) fn accept_dropped_files_hover(
    target: &impl IsA<gtk::Widget>,
    resolve: impl Fn() -> Option<(DaemonHandle, String)> + 'static,
    hover: impl Fn(bool) + 'static,
) {
    let drop = gtk::DropTarget::new(
        gtk::gdk::FileList::static_type(),
        gtk::gdk::DragAction::COPY,
    );
    let resolve = std::rc::Rc::new(resolve);
    let hover = std::rc::Rc::new(hover);
    {
        let (resolve, hover) = (resolve.clone(), hover.clone());
        drop.connect_enter(move |_, _, _| {
            hover(resolve().is_some());
            gtk::gdk::DragAction::COPY
        });
    }
    {
        let hover = hover.clone();
        drop.connect_leave(move |_| hover(false));
    }
    let widget = target.as_ref().downgrade();
    drop.connect_drop(move |_, value, _, _| {
        hover(false);
        let Ok(files) = value.get::<gtk::gdk::FileList>() else {
            return false;
        };
        let say = |message: &str| {
            if let Some(widget) = widget.upgrade() {
                toast_from(&widget, message);
            }
        };
        let Some((daemon, id)) = resolve() else {
            say("Files can be sent once the phone is connected.");
            return false;
        };
        let (mut sent, mut folders, mut remote) = (0, 0, 0);
        for file in files.files() {
            match file.path() {
                // A local stat only; a remote location has no path at all.
                Some(path) if std::fs::metadata(&path).is_ok_and(|meta| meta.is_dir()) => {
                    folders += 1
                }
                Some(path) => {
                    daemon.send(DaemonCommand::SendFile(id.clone(), path));
                    sent += 1;
                }
                None => remote += 1,
            }
        }
        if folders > 0 {
            say("Folders can't be sent. Drop the files inside instead.");
        } else if remote > 0 {
            say("Only files stored on this computer can be sent.");
        }
        sent > 0
    });
    target.add_controller(drop);
}
