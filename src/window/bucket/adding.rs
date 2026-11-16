//! Putting things in the Bucket from the page: choosing files or a folder,
//! drops and paste.
//!
//! Sibling edges: `browse` (the folder being shown).

use super::*;

thread_local! {
    /// Ctrl+V is installed on the window once.
    static PASTE_READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Puts `paths` (files and folders) in the folder shown of `device`'s
/// Bucket. What was left out is told when the copy is done.
fn add_paths(device: &str, paths: Vec<PathBuf>, into: &str) -> bool {
    !paths.is_empty() && folders::import(device, paths, into)
}

/// "Add Files…".
pub(super) fn choose(widget: &impl IsA<gtk::Widget>, device: &str) {
    let Some(root) = widget.root().and_downcast::<gtk::Window>() else {
        return;
    };
    let device = device.to_string();
    let dialog = gtk::FileDialog::builder()
        .title("Add Files to the Bucket")
        .modal(true)
        .build();
    dialog.open_multiple(Some(&root), gio::Cancellable::NONE, move |result| {
        if let Ok(files) = result {
            let paths: Vec<PathBuf> = files
                .iter::<gio::File>()
                .flatten()
                .filter_map(|file| file.path())
                .collect();
            add_paths(&device, paths, &current());
        }
    });
}

/// "Add Folder…".
pub(super) fn choose_folder(widget: &impl IsA<gtk::Widget>, device: &str) {
    let Some(root) = widget.root().and_downcast::<gtk::Window>() else {
        return;
    };
    let device = device.to_string();
    let dialog = gtk::FileDialog::builder()
        .title("Add a Folder to the Bucket")
        .modal(true)
        .build();
    dialog.select_multiple_folders(Some(&root), gio::Cancellable::NONE, move |result| {
        if let Ok(files) = result {
            let paths: Vec<PathBuf> = files
                .iter::<gio::File>()
                .flatten()
                .filter_map(|file| file.path())
                .collect();
            add_paths(&device, paths, &current());
        }
    });
}

/// Files dropped anywhere on `target` go in the Bucket.
/// Where a drop goes: the explicit folder of the target, else the one shown.
pub(super) fn drop_folder(explicit: Option<&str>, shown: &str) -> String {
    explicit.unwrap_or(shown).to_string()
}

/// `into`: the folder they go in; `None` the one shown at the time of the
/// drop, `Some("")` the top (the Overview card, whatever the Bucket page
/// last showed).
pub(super) fn accept_drops(target: &impl IsA<gtk::Widget>, device: &str, into: Option<&str>) {
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    let device = device.to_string();
    let into = into.map(str::to_string);
    let shown = target.clone().upcast::<gtk::Widget>();
    drop.connect_enter(move |_, _, _| {
        shown.add_css_class("bucket-drop-hover");
        gdk::DragAction::COPY
    });
    let shown = target.clone().upcast::<gtk::Widget>();
    drop.connect_leave(move |_| shown.remove_css_class("bucket-drop-hover"));
    let shown = target.clone().upcast::<gtk::Widget>();
    drop.connect_drop(move |_, value, _, _| {
        shown.remove_css_class("bucket-drop-hover");
        let Ok(list) = value.get::<gdk::FileList>() else {
            return false;
        };
        let paths: Vec<PathBuf> = list.files().iter().filter_map(|file| file.path()).collect();
        add_paths(&device, paths, &drop_folder(into.as_deref(), &current()))
    });
    target.as_ref().add_controller(drop);
}

/// Ctrl+V on the Bucket page: files, a picture, a link or text. Installed
/// on the window once; it passes when another page is shown, and a focused
/// text field gets its own Ctrl+V first.
pub(super) fn install_paste(win: &DesklinkWindow) {
    if PASTE_READY.with(|ready| ready.replace(true)) {
        return;
    }
    let controller = gtk::ShortcutController::new();
    controller.set_scope(gtk::ShortcutScope::Managed);
    let action = gtk::CallbackAction::new(|widget, _| {
        let Some(win) = widget.downcast_ref::<DesklinkWindow>() else {
            return glib::Propagation::Proceed;
        };
        if win.imp().page.borrow().as_str() != "files" {
            return glib::Propagation::Proceed;
        }
        let Some(device) = win.selected_id().filter(|id| bucket_of(id).is_some()) else {
            return glib::Propagation::Proceed;
        };
        paste(win, &device);
        glib::Propagation::Stop
    });
    controller.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("<Control>v"),
        Some(action),
    ));
    // Ctrl+A selects everything shown; Escape leaves selecting.
    let on_bucket = |widget: &gtk::Widget| {
        widget
            .downcast_ref::<DesklinkWindow>()
            .is_some_and(|win| win.imp().page.borrow().as_str() == "files")
    };
    controller.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("<Control>a"),
        Some(gtk::CallbackAction::new(move |widget, _| {
            if on_bucket(widget) {
                select_all();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        })),
    ));
    controller.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Escape"),
        Some(gtk::CallbackAction::new(move |widget, _| {
            if on_bucket(widget) && selecting() {
                set_selecting(false);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        })),
    ));
    win.add_controller(controller);
}

fn paste(win: &DesklinkWindow, device: &str) {
    let clipboard = win.clipboard();
    let formats = clipboard.formats();
    let device = device.to_string();
    let anchor = win.clone();
    let here = current();
    if formats.contains_type(gdk::FileList::static_type()) {
        clipboard.read_value_async(
            gdk::FileList::static_type(),
            glib::Priority::DEFAULT,
            gio::Cancellable::NONE,
            move |value| {
                let paths: Vec<PathBuf> = value
                    .ok()
                    .and_then(|value| value.get::<gdk::FileList>().ok())
                    .map(|list| list.files().iter().filter_map(|file| file.path()).collect())
                    .unwrap_or_default();
                add_paths(&device, paths, &here);
            },
        );
    } else if formats.contains_type(gdk::Texture::static_type()) {
        clipboard.read_texture_async(gio::Cancellable::NONE, move |texture| {
            let Some(texture) = texture.ok().flatten() else {
                return;
            };
            // Encoding a big picture takes a while: not on this thread.
            let stamp = glib::DateTime::now_local()
                .ok()
                .and_then(|now| now.format("%Y-%m-%d %H.%M.%S").ok())
                .map(|stamp| stamp.to_string())
                .unwrap_or_default();
            let (device, here) = (device.clone(), here.clone());
            std::thread::spawn(move || {
                let png = texture.save_to_png_bytes();
                let _ = folders::import_bytes(
                    &device,
                    &format!("Picture {stamp}.png"),
                    png.to_vec(),
                    &here,
                );
            });
            anchor.show_feature_toast("Pasted into the Bucket");
        });
    } else {
        clipboard.read_text_async(gio::Cancellable::NONE, move |text| {
            let Some(text) = text.ok().flatten().filter(|text| !text.trim().is_empty()) else {
                anchor.show_feature_toast("Nothing to paste");
                return;
            };
            let Ok(now) = glib::DateTime::now_local() else {
                return;
            };
            let (name, bytes) = folders::pasted_text(&text, &now);
            match folders::import_bytes(&device, &name, bytes, &here) {
                Ok(()) => anchor.show_feature_toast("Pasted into the Bucket"),
                Err(error) => anchor.show_feature_toast(&error),
            }
        });
    }
}
