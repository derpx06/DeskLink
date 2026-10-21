//! The viewer for transferred files, inside DeskLink: an image to fit or at
//! full size (Ctrl+scroll or pinch to zoom), a video playing with its
//! controls, anything else with Open With… and Show in Folder. Left and
//! Right page through the files; Escape closes.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, gio, glib};

use super::DesklinkWindow;
use crate::device_links::transfer_log::LoggedFile;
use crate::thumbnails::{kind_of, FileKind};

/// What the viewer shows and how.
struct State {
    items: Vec<LoggedFile>,
    index: usize,
    device: String,
    /// `None` fits the image to the window; otherwise its scale.
    zoom: Option<f64>,
    /// The image shown, once loaded.
    texture: Option<gdk::Texture>,
    picture: Option<gtk::Picture>,
    /// The video playing, for Space and for stopping it.
    stream: Option<gtk::MediaStream>,
}

struct Viewer {
    dialog: adw::Dialog,
    title: adw::WindowTitle,
    area: gtk::Box,
    zoom: gtk::Button,
    previous: gtk::Button,
    next: gtk::Button,
    toasts: adw::ToastOverlay,
    state: RefCell<State>,
}

thread_local! {
    /// The viewer on screen. Its widgets' handlers hold it weakly.
    static OPEN: RefCell<Option<Rc<Viewer>>> = const { RefCell::new(None) };
}

/// Opens `items[index]` over `win`.
pub(super) fn show(win: &DesklinkWindow, items: &[LoggedFile], index: usize, device: &str) {
    if items.is_empty() || win.visible_dialog().is_some() {
        return;
    }
    let width = (f64::from(win.width()) * 0.86) as i32;
    let height = (f64::from(win.height()) * 0.86) as i32;
    let dialog = adw::Dialog::builder()
        .content_width(width.max(360))
        .content_height(height.max(320))
        .build();
    let title = adw::WindowTitle::new("", "");
    let header = adw::HeaderBar::builder().title_widget(&title).build();

    let open_with = gtk::Button::builder().label("Open With…").build();
    let zoom = header_button("zoom-original-symbolic", "Actual Size");
    let copy = header_button("edit-copy-symbolic", "Copy");
    let folder = header_button("folder-open-symbolic", "Show in Folder");
    let remove = header_button("list-remove-symbolic", "Remove from List");
    header.pack_start(&open_with);
    header.pack_end(&remove);
    header.pack_end(&folder);
    header.pack_end(&copy);
    header.pack_end(&zoom);

    let area = gtk::Box::builder()
        .hexpand(true)
        .vexpand(true)
        .css_classes(["viewer-area"])
        .build();
    let previous = side_button("go-previous-symbolic", "Previous File", gtk::Align::Start);
    let next = side_button("go-next-symbolic", "Next File", gtk::Align::End);
    let stage = gtk::Overlay::builder().child(&area).build();
    stage.add_overlay(&previous);
    stage.add_overlay(&next);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stage));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));
    dialog.set_child(Some(&toolbar));

    let viewer = Rc::new(Viewer {
        dialog: dialog.clone(),
        title,
        area,
        zoom: zoom.clone(),
        previous: previous.clone(),
        next: next.clone(),
        toasts,
        state: RefCell::new(State {
            items: items.to_vec(),
            index: index.min(items.len() - 1),
            device: device.to_string(),
            zoom: None,
            texture: None,
            picture: None,
            stream: None,
        }),
    });

    let weak = Rc::downgrade(&viewer);
    let with = move |action: fn(&Rc<Viewer>)| {
        let weak = weak.clone();
        move |_: &gtk::Button| {
            if let Some(viewer) = weak.upgrade() {
                action(&viewer);
            }
        }
    };
    previous.connect_clicked(with(|viewer| viewer.step(-1)));
    next.connect_clicked(with(|viewer| viewer.step(1)));
    zoom.connect_clicked(with(|viewer| viewer.toggle_zoom()));
    copy.connect_clicked(with(|viewer| viewer.copy()));
    folder.connect_clicked(with(|viewer| viewer.launch(true)));
    open_with.connect_clicked(with(|viewer| viewer.launch(false)));
    {
        let window = win.downgrade();
        let weak = Rc::downgrade(&viewer);
        remove.connect_clicked(move |_| {
            if let (Some(viewer), Some(window)) = (weak.upgrade(), window.upgrade()) {
                viewer.remove(&window);
            }
        });
    }

    // Left/Right page; Space plays and pauses a video. Escape is the
    // dialog's own.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(&viewer);
    keys.connect_key_pressed(move |_, key, _, _| {
        let Some(viewer) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        match key {
            gdk::Key::Left => viewer.step(-1),
            gdk::Key::Right => viewer.step(1),
            gdk::Key::space => {
                let stream = viewer.state.borrow().stream.clone();
                match stream {
                    Some(stream) => stream.set_playing(!stream.is_playing()),
                    None => return glib::Propagation::Proceed,
                }
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    dialog.add_controller(keys);

    // Ctrl+scroll and pinch zoom an image.
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(&viewer);
    scroll.connect_scroll(move |controller, _, dy| {
        let ctrl = controller
            .current_event_state()
            .contains(gdk::ModifierType::CONTROL_MASK);
        match weak.upgrade() {
            Some(viewer) if ctrl => {
                let scale = viewer.scale();
                viewer.set_zoom(Some(scale * 1.15_f64.powf(-dy)));
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    viewer.area.add_controller(scroll);
    let pinch = gtk::GestureZoom::new();
    let base = Rc::new(std::cell::Cell::new(1.0));
    let (weak, start) = (Rc::downgrade(&viewer), Rc::clone(&base));
    pinch.connect_begin(move |_, _| {
        if let Some(viewer) = weak.upgrade() {
            start.set(viewer.scale());
        }
    });
    let weak = Rc::downgrade(&viewer);
    pinch.connect_scale_changed(move |_, scale| {
        if let Some(viewer) = weak.upgrade() {
            viewer.set_zoom(Some(base.get() * scale));
        }
    });
    viewer.area.add_controller(pinch);

    // The viewer lives while its dialog is open; a video stops with it.
    OPEN.with(|open| open.replace(Some(Rc::clone(&viewer))));
    dialog.connect_closed(|_| {
        if let Some(viewer) = OPEN.with(|open| open.take()) {
            if let Some(stream) = viewer.state.borrow_mut().stream.take() {
                stream.set_playing(false);
            }
        }
    });
    viewer.render();
    dialog.present(Some(win));
}

fn header_button(icon: &str, label: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(label)
        .build();
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button
}

fn side_button(icon: &str, label: &str, side: gtk::Align) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(label)
        .halign(side)
        .valign(gtk::Align::Center)
        .margin_start(12)
        .margin_end(12)
        .css_classes(["circular", "osd", "viewer-step"])
        .build();
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button
}

impl Viewer {
    fn current(&self) -> Option<LoggedFile> {
        let state = self.state.borrow();
        state.items.get(state.index).cloned()
    }

    fn step(&self, by: isize) {
        let (index, count) = {
            let state = self.state.borrow();
            (state.index as isize + by, state.items.len() as isize)
        };
        if index < 0 || index >= count {
            return;
        }
        self.state.borrow_mut().index = index as usize;
        self.render();
    }

    /// Shows the current file.
    fn render(&self) {
        let Some(file) = self.current() else {
            self.dialog.close();
            return;
        };
        {
            let mut state = self.state.borrow_mut();
            if let Some(stream) = state.stream.take() {
                stream.set_playing(false);
            }
            state.zoom = None;
            state.texture = None;
            state.picture = None;
        }
        let (index, count, device) = {
            let state = self.state.borrow();
            (state.index, state.items.len(), state.device.clone())
        };
        let way = if file.incoming {
            format!("from {device}")
        } else {
            format!("sent to {device}")
        };
        self.title.set_title(&file.name);
        self.title.set_subtitle(&format!(
            "{} · {way}",
            super::transfers::size_and_time(&file)
        ));
        self.dialog.set_title(&file.name);
        self.previous.set_visible(index > 0);
        self.next.set_visible(index + 1 < count);
        while let Some(child) = self.area.first_child() {
            self.area.remove(&child);
        }
        let kind = kind_of(&file.path);
        self.zoom.set_visible(kind == FileKind::Image);
        self.update_zoom_button();
        match kind {
            FileKind::Image => self.show_image(&file),
            FileKind::Video => self.show_video(&file),
            _ => self.show_other(&file),
        }
    }

    fn show_image(&self, file: &LoggedFile) {
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .hexpand(true)
            .vexpand(true)
            .alternative_text(&file.name)
            .build();
        let spinner = gtk::Spinner::builder()
            .spinning(true)
            .width_request(32)
            .height_request(32)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        let stage = gtk::Overlay::builder().child(&picture).build();
        stage.add_overlay(&spinner);
        let scroller = gtk::ScrolledWindow::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&stage)
            .build();
        self.area.append(&scroller);
        self.state.borrow_mut().picture = Some(picture.clone());
        // Decoded on a worker: a large photo must not stall the window.
        let (path, picture, spinner) =
            (file.path.clone(), picture.downgrade(), spinner.downgrade());
        let dialog = self.dialog.downgrade();
        glib::spawn_future_local(async move {
            let loaded = gio::spawn_blocking(move || gdk::Texture::from_filename(&path)).await;
            if let Some(spinner) = spinner.upgrade() {
                spinner.set_visible(false);
            }
            let (Some(picture), Some(_dialog)) = (picture.upgrade(), dialog.upgrade()) else {
                return;
            };
            match loaded {
                Ok(Ok(texture)) => picture.set_paintable(Some(&texture)),
                _ => picture.set_alternative_text(Some("This image could not be opened")),
            }
        });
    }

    fn show_video(&self, file: &LoggedFile) {
        let video = gtk::Video::builder()
            .file(&gio::File::for_path(&file.path))
            .autoplay(true)
            .hexpand(true)
            .vexpand(true)
            .css_classes(["viewer-video"])
            .build();
        video.update_property(&[gtk::accessible::Property::Label(&file.name)]);
        if let Some(stream) = video.media_stream() {
            stream.set_muted(false);
            self.state.borrow_mut().stream = Some(stream);
        }
        self.area.append(&video);
        video.grab_focus();
    }

    /// No picture: what it is, and the ways to open it.
    fn show_other(&self, file: &LoggedFile) {
        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(14)
            .hexpand(true)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        let (content_type, _) = gio::content_type_guess(Some(&file.name), &[]);
        page.append(
            &gtk::Image::builder()
                .gicon(&gio::content_type_get_icon(&content_type))
                .pixel_size(96)
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
        page.append(
            &gtk::Label::builder()
                .label(&file.name)
                .wrap(true)
                .justify(gtk::Justification::Center)
                .css_classes(["title-4"])
                .build(),
        );
        page.append(
            &gtk::Label::builder()
                .label(gio::content_type_get_description(&content_type))
                .css_classes(["text-dim"])
                .build(),
        );
        let buttons = gtk::Box::builder()
            .spacing(10)
            .halign(gtk::Align::Center)
            .margin_top(8)
            .build();
        let open = gtk::Button::builder()
            .label("Open With…")
            .css_classes(["suggested-action", "pill"])
            .build();
        let folder = gtk::Button::builder()
            .label("Show in Folder")
            .css_classes(["pill"])
            .build();
        let path = file.path.clone();
        open.connect_clicked(move |button| launch(button, &path, false));
        let path = file.path.clone();
        folder.connect_clicked(move |button| launch(button, &path, true));
        buttons.append(&open);
        buttons.append(&folder);
        page.append(&buttons);
        self.area.append(&page);
    }

    /// The image's scale now: its zoom, or what fitting it gives.
    fn scale(&self) -> f64 {
        let state = self.state.borrow();
        if let Some(zoom) = state.zoom {
            return zoom;
        }
        match (
            &state.picture,
            state.picture.as_ref().and_then(|p| p.paintable()),
        ) {
            (Some(picture), Some(paintable)) if paintable.intrinsic_width() > 0 => {
                let width = f64::from(picture.width()) / f64::from(paintable.intrinsic_width());
                let height = f64::from(picture.height()) / f64::from(paintable.intrinsic_height());
                width.min(height).max(0.05)
            }
            _ => 1.0,
        }
    }

    fn toggle_zoom(&self) {
        let fitted = self.state.borrow().zoom.is_none();
        self.set_zoom(fitted.then_some(1.0));
    }

    /// `None` fits the image; otherwise shows it at that scale, scrolling.
    fn set_zoom(&self, zoom: Option<f64>) {
        let zoom = zoom.map(|zoom| zoom.clamp(0.05, 8.0));
        let picture = {
            let mut state = self.state.borrow_mut();
            state.zoom = zoom;
            state.picture.clone()
        };
        if let Some(picture) = picture {
            match (zoom, picture.paintable()) {
                (Some(zoom), Some(paintable)) => picture.set_size_request(
                    (f64::from(paintable.intrinsic_width()) * zoom) as i32,
                    (f64::from(paintable.intrinsic_height()) * zoom) as i32,
                ),
                _ => picture.set_size_request(-1, -1),
            }
        }
        self.update_zoom_button();
    }

    fn update_zoom_button(&self) {
        let fitted = self.state.borrow().zoom.is_none();
        let (icon, label) = if fitted {
            ("zoom-original-symbolic", "Actual Size")
        } else {
            ("zoom-fit-best-symbolic", "Fit to Window")
        };
        self.zoom.set_icon_name(icon);
        self.zoom.set_tooltip_text(Some(label));
        self.zoom
            .update_property(&[gtk::accessible::Property::Label(label)]);
    }

    /// The image itself if it is one, else the file, to paste elsewhere.
    fn copy(&self) {
        let Some(file) = self.current() else {
            return;
        };
        let clipboard = self.dialog.clipboard();
        let texture = self
            .state
            .borrow()
            .picture
            .as_ref()
            .and_then(|picture| picture.paintable())
            .and_downcast::<gdk::Texture>();
        match texture {
            Some(texture) => clipboard.set_texture(&texture),
            None => {
                let files = gdk::FileList::from_array(&[gio::File::for_path(&file.path)]);
                clipboard
                    .set_content(Some(&gdk::ContentProvider::for_value(&files.to_value())))
                    .ok();
            }
        }
        self.toast("Copied");
    }

    fn launch(&self, in_folder: bool) {
        if let Some(file) = self.current() {
            launch(&self.dialog, &file.path, in_folder);
        }
    }

    /// Takes the file off the list (the file itself stays).
    fn remove(&self, window: &DesklinkWindow) {
        let Some(file) = self.current() else {
            return;
        };
        crate::device_links::transfer_log::remove(&file);
        {
            let mut state = self.state.borrow_mut();
            let index = state.index;
            state.items.remove(index);
            state.index = index.min(state.items.len().saturating_sub(1));
        }
        if let Some(page) = window.imp().transfers.borrow().as_ref() {
            page.invalidate();
        }
        window.refresh_soon();
        self.render();
    }

    fn toast(&self, message: &str) {
        let toast = adw::Toast::new(message);
        toast.set_timeout(2);
        self.toasts.add_toast(toast);
    }
}

/// Another app for `path` (asking which), or its folder.
fn launch(widget: &impl IsA<gtk::Widget>, path: &std::path::Path, in_folder: bool) {
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
    let window = widget.root().and_downcast::<gtk::Window>();
    let done = |result: Result<(), glib::Error>| {
        if let Err(error) = result {
            eprintln!("[UI] Could not open the file: {error}");
        }
    };
    if in_folder {
        launcher.open_containing_folder(window.as_ref(), gio::Cancellable::NONE, done);
    } else {
        launcher.set_always_ask(true);
        launcher.launch(window.as_ref(), gio::Cancellable::NONE, done);
    }
}
