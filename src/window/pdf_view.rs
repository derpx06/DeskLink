//! A PDF in the file viewer: pages one under another, scrolled; zoom
//! (buttons, Ctrl+scroll, pinch) relative to the page fitting the width;
//! "Page n / N"; and Open in Default App.
//!
//! Pages are drawn by poppler (`crate::pdf`) on one worker thread per open
//! document, and only those on or near the screen: a page first comes at a
//! quarter of its width, then sharp. Pages far off screen let their picture
//! go again, so a long document stays light.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;

use adw::prelude::*;
use gtk::{gdk, glib};

/// Space between pages, and around them.
const GAP: i32 = 12;
/// How far beyond the screen (in screen heights) pages are kept drawn.
const KEEP_SCREENS: f64 = 2.0;

enum Job {
    Render { index: usize, width: i32 },
}

enum Reply {
    Opened(Vec<(f64, f64)>),
    Failed(String),
    Page {
        index: usize,
        width: i32,
        height: i32,
        stride: usize,
        bgra: Vec<u8>,
    },
}

struct Slot {
    picture: gtk::Picture,
    /// Page size in points.
    points: (f64, f64),
    /// The width it is drawn at now (0: not drawn), and the width asked of
    /// the worker last.
    drawn: Cell<i32>,
    asked: Cell<i32>,
}

pub(super) struct PdfView {
    pub(super) root: gtk::Widget,
    path: PathBuf,
    scroller: gtk::ScrolledWindow,
    column: gtk::Box,
    stack: gtk::Stack,
    page_label: gtk::Label,
    slots: RefCell<Vec<Slot>>,
    /// 1 fits a page to the width.
    zoom: Cell<f64>,
    jobs: mpsc::Sender<Job>,
    /// How many render jobs were asked of the worker, in all.
    asked_total: Cell<u64>,
    pump: RefCell<Option<Rc<Pump>>>,
}

/// The view for `path`. Opening and drawing start at once, off the main
/// thread; the view polls for pages while it is alive.
pub(super) fn open(path: &Path) -> Rc<PdfView> {
    let (jobs, job_queue) = mpsc::channel();
    let (reply_tx, replies) = mpsc::channel();
    let source = path.to_path_buf();
    let _ = std::thread::Builder::new()
        .name("desklink-pdf".into())
        .spawn(move || work(&source, &job_queue, &reply_tx));

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(GAP)
        .margin_top(GAP)
        .margin_bottom(GAP * 6)
        .halign(gtk::Align::Center)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .vexpand(true)
        .child(&column)
        .build();
    let page_label = gtk::Label::builder()
        .css_classes(["numeric"])
        .margin_start(8)
        .margin_end(8)
        .build();
    let bar = gtk::Box::builder()
        .spacing(4)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::End)
        .margin_bottom(14)
        .css_classes(["toolbar", "osd", "floating-toolbar"])
        .build();
    let zoom_out = tool("zoom-out-symbolic", "Zoom Out");
    let fit = tool("zoom-fit-best-symbolic", "Fit Width");
    let zoom_in = tool("zoom-in-symbolic", "Zoom In");
    // Short, so the bar fits a phone-width window.
    let open_default = gtk::Button::builder()
        .label("Open")
        .tooltip_text("Open in Default App")
        .css_classes(["flat"])
        .build();
    open_default.update_property(&[gtk::accessible::Property::Label("Open in Default App")]);
    bar.append(&page_label);
    bar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    bar.append(&zoom_out);
    bar.append(&fit);
    bar.append(&zoom_in);
    bar.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    bar.append(&open_default);
    let pages = gtk::Overlay::builder().child(&scroller).build();
    pages.add_overlay(&bar);

    let loading = gtk::Spinner::builder()
        .spinning(true)
        .width_request(32)
        .height_request(32)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    let stack = gtk::Stack::builder().hexpand(true).vexpand(true).build();
    stack.add_named(&loading, Some("loading"));
    stack.add_named(&pages, Some("pages"));

    let view = Rc::new(PdfView {
        root: stack.clone().upcast(),
        path: path.to_path_buf(),
        scroller,
        column,
        stack,
        page_label,
        slots: RefCell::default(),
        zoom: Cell::new(1.0),
        jobs,
        asked_total: Cell::new(0),
        pump: RefCell::new(None),
    });
    let weak = Rc::downgrade(&view);
    zoom_out.connect_clicked(move |_| {
        if let Some(view) = weak.upgrade() {
            view.zoom_by(1.0 / 1.25);
        }
    });
    let weak = Rc::downgrade(&view);
    zoom_in.connect_clicked(move |_| {
        if let Some(view) = weak.upgrade() {
            view.zoom_by(1.25);
        }
    });
    let weak = Rc::downgrade(&view);
    fit.connect_clicked(move |_| {
        if let Some(view) = weak.upgrade() {
            view.set_zoom(1.0);
        }
    });
    let file = path.to_path_buf();
    open_default.connect_clicked(move |button| open_in_default_app(button, &file));

    // Replies, layout and what to draw: checked every 40 ms while something is
    // happening (opening, pages being drawn, scrolling, zooming, resizing)
    // and not at all once it has been quiet for a moment: scanning every
    // page 25 times a second for as long as the viewer was open cost a core
    // on a long document.
    let pump = Rc::new(Pump {
        replies,
        running: Cell::new(false),
        quiet: Cell::new(0),
        width_seen: Cell::new(0),
    });
    *view.pump.borrow_mut() = Some(Rc::clone(&pump));
    let weak = Rc::downgrade(&view);
    let wake = move || {
        if let Some(view) = weak.upgrade() {
            view.wake();
        }
    };
    let on_scroll = wake.clone();
    view.scroller
        .vadjustment()
        .connect_value_changed(move |_| on_scroll());
    let on_resize = wake.clone();
    view.scroller
        .vadjustment()
        .connect_page_size_notify(move |_| on_resize());
    let on_zoom = wake.clone();
    view.scroller
        .vadjustment()
        .connect_upper_notify(move |_| on_zoom());
    let on_width = wake.clone();
    view.scroller
        .hadjustment()
        .connect_page_size_notify(move |_| on_width());
    let on_map = wake.clone();
    view.scroller.connect_map(move |_| on_map());
    view.wake();
    view
}

/// What the poll needs between turns.
struct Pump {
    replies: mpsc::Receiver<Reply>,
    running: Cell<bool>,
    /// Turns in a row in which nothing arrived and nothing was asked.
    quiet: Cell<u32>,
    width_seen: Cell<i32>,
}

/// Quiet turns (40 ms each) after which the poll stops until something
/// wakes it: long enough for the page just asked for to come back.
const QUIET_TURNS: u32 = 25;

/// Whether the poll goes on after `quiet` turns of nothing.
fn keeps_polling(quiet: u32) -> bool {
    quiet < QUIET_TURNS
}

fn tool(icon: &str, label: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(label)
        .css_classes(["flat", "circular"])
        .build();
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button
}

/// The worker: opens the document, then draws what is asked, newest first
/// for each page (a burst of asks while scrolling collapses).
fn work(path: &Path, jobs: &mpsc::Receiver<Job>, replies: &mpsc::Sender<Reply>) {
    let document = match crate::pdf::Document::open(path) {
        Ok(document) => document,
        Err(error) => {
            let _ = replies.send(Reply::Failed(error));
            return;
        }
    };
    if replies
        .send(Reply::Opened(document.sizes().to_vec()))
        .is_err()
    {
        return;
    }
    while let Ok(first) = jobs.recv() {
        let mut wanted: Vec<(usize, i32)> = Vec::new();
        let mut push = |Job::Render { index, width }: Job| {
            wanted.retain(|(known, _)| *known != index);
            wanted.push((index, width));
        };
        push(first);
        while let Ok(more) = jobs.try_recv() {
            push(more);
        }
        for (index, width) in wanted {
            let Ok(page) = document.render(index, width) else {
                continue;
            };
            let sent = replies.send(Reply::Page {
                index,
                width,
                height: page.height,
                stride: page.stride,
                bgra: page.bgra,
            });
            if sent.is_err() {
                return;
            }
        }
    }
}

impl PdfView {
    /// Something may have changed: poll (again) for replies, layout and what
    /// to draw, until it has been quiet for a while.
    fn wake(self: &Rc<Self>) {
        let Some(pump) = self.pump.borrow().clone() else {
            return;
        };
        pump.quiet.set(0);
        if pump.running.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(std::time::Duration::from_millis(40), move || {
            let Some(view) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let Some(pump) = view.pump.borrow().clone() else {
                return glib::ControlFlow::Break;
            };
            let mut busy = false;
            while let Ok(reply) = pump.replies.try_recv() {
                busy = true;
                view.take(reply);
            }
            let width = view.scroller.width();
            if width != pump.width_seen.get() {
                pump.width_seen.set(width);
                busy = true;
                view.lay_out();
            }
            let before = view.asked_total.get();
            view.draw_visible();
            busy |= view.asked_total.get() != before;
            pump.quiet.set(if busy { 0 } else { pump.quiet.get() + 1 });
            if keeps_polling(pump.quiet.get()) {
                glib::ControlFlow::Continue
            } else {
                pump.running.set(false);
                glib::ControlFlow::Break
            }
        });
    }

    fn take(&self, reply: Reply) {
        match reply {
            Reply::Opened(sizes) => {
                let mut slots = self.slots.borrow_mut();
                for (index, points) in sizes.into_iter().enumerate() {
                    let picture = gtk::Picture::builder()
                        .content_fit(gtk::ContentFit::Fill)
                        .css_classes(["pdf-page"])
                        .alternative_text(format!("Page {}", index + 1))
                        .build();
                    self.column.append(&picture);
                    slots.push(Slot {
                        picture,
                        points,
                        drawn: Cell::new(0),
                        asked: Cell::new(0),
                    });
                }
                drop(slots);
                self.stack.set_visible_child_name("pages");
                self.lay_out();
            }
            Reply::Failed(error) => {
                let page = adw::StatusPage::builder()
                    .icon_name("x-office-document-symbolic")
                    .title("Can't preview this PDF here")
                    .description(glib::markup_escape_text(&error))
                    .hexpand(true)
                    .build();
                let open = gtk::Button::builder()
                    .label("Open in Default App")
                    .halign(gtk::Align::Center)
                    .css_classes(["suggested-action", "pill"])
                    .build();
                let file = self.path.clone();
                open.connect_clicked(move |button| open_in_default_app(button, &file));
                page.set_child(Some(&open));
                self.stack.add_named(&page, Some("failed"));
                self.stack.set_visible_child_name("failed");
            }
            Reply::Page {
                index,
                width,
                height,
                stride,
                bgra,
            } => {
                let slots = self.slots.borrow();
                let Some(slot) = slots.get(index) else {
                    return;
                };
                // A sharper picture is never replaced by a blurrier one
                // that arrives late.
                if width < slot.drawn.get() && slot.drawn.get() <= self.target_width() * 2 {
                    return;
                }
                let texture = gdk::MemoryTexture::new(
                    width,
                    height,
                    gdk::MemoryFormat::B8g8r8a8Premultiplied,
                    &glib::Bytes::from_owned(bgra),
                    stride,
                );
                slot.picture.set_paintable(Some(&texture));
                slot.drawn.set(width);
            }
        }
    }

    /// A page's width on screen, in logical pixels.
    fn page_width(&self) -> i32 {
        let available = (self.scroller.width() - GAP * 4).max(200);
        ((f64::from(available) * self.zoom.get()) as i32).clamp(120, 6000)
    }

    /// The width a page is drawn at: its screen width in device pixels.
    fn target_width(&self) -> i32 {
        self.page_width() * self.scroller.scale_factor().max(1)
    }

    fn lay_out(&self) {
        let width = self.page_width();
        for slot in self.slots.borrow().iter() {
            let height = (f64::from(width) * slot.points.1 / slot.points.0).round() as i32;
            slot.picture.set_size_request(width, height.max(1));
        }
    }

    pub(super) fn zoom_by(&self, factor: f64) {
        self.set_zoom(self.zoom.get() * factor);
    }

    pub(super) fn zoom(&self) -> f64 {
        self.zoom.get()
    }

    pub(super) fn set_zoom(&self, zoom: f64) {
        let adjustment = self.scroller.vadjustment();
        let fraction = adjustment.value() / adjustment.upper().max(1.0);
        self.zoom.set(zoom.clamp(0.25, 6.0));
        self.lay_out();
        // Keep the same place in the document in view.
        let adjustment = self.scroller.vadjustment();
        glib::idle_add_local_once(move || {
            adjustment.set_value(fraction * adjustment.upper());
        });
    }

    /// Asks for the pages on (or near) the screen at the current width,
    /// lets pages far away go, and says which page is in the middle.
    fn draw_visible(&self) {
        let slots = self.slots.borrow();
        if slots.is_empty() {
            return;
        }
        let adjustment = self.scroller.vadjustment();
        let (top, height) = (adjustment.value(), adjustment.page_size().max(1.0));
        let middle = top + height / 2.0;
        let target = self.target_width();
        let mut current = 0;
        for (index, slot) in slots.iter().enumerate() {
            let Some(at) = slot
                .picture
                .compute_point(&self.column, &gtk::graphene::Point::zero())
            else {
                continue;
            };
            let (start, end) = (
                f64::from(at.y()),
                f64::from(at.y()) + f64::from(slot.picture.height()),
            );
            if start <= middle {
                current = index;
            }
            let on_screen = end >= top - height * 0.5 && start <= top + height * 1.5;
            let kept =
                end >= top - height * KEEP_SCREENS && start <= top + height * (1.0 + KEEP_SCREENS);
            if on_screen {
                if slot.drawn.get() == 0 && slot.asked.get() == 0 {
                    // Something at once, then sharp.
                    self.asked_total.set(self.asked_total.get() + 1);
                    let _ = self.jobs.send(Job::Render {
                        index,
                        width: (target / 4).max(64),
                    });
                }
                if slot.asked.get() != target {
                    slot.asked.set(target);
                    self.asked_total.set(self.asked_total.get() + 1);
                    let _ = self.jobs.send(Job::Render {
                        index,
                        width: target,
                    });
                }
            } else if !kept && slot.drawn.get() != 0 {
                slot.picture.set_paintable(None::<&gdk::Paintable>);
                slot.drawn.set(0);
                slot.asked.set(0);
            }
        }
        self.page_label.set_label(&page_words(current, slots.len()));
    }
}

/// "Page 3 / 12".
fn page_words(index: usize, count: usize) -> String {
    format!("Page {} / {count}", (index + 1).min(count.max(1)))
}

fn open_in_default_app(widget: &impl IsA<gtk::Widget>, path: &Path) {
    let launcher = gtk::FileLauncher::new(Some(&gtk::gio::File::for_path(path)));
    let window = widget.root().and_downcast::<gtk::Window>();
    launcher.launch(window.as_ref(), gtk::gio::Cancellable::NONE, |result| {
        if let Err(error) = result {
            eprintln!("[UI] Could not open the PDF: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_count_reads_as_n_of_total() {
        assert_eq!(page_words(0, 12), "Page 1 / 12");
        assert_eq!(page_words(11, 12), "Page 12 / 12");
        assert_eq!(page_words(40, 12), "Page 12 / 12");
    }

    /// The page poll stops once it has been quiet for a second, so a viewer
    /// left open on a long document costs nothing; any scroll, zoom, resize
    /// or arriving page starts it again (`wake`).
    #[test]
    fn the_page_poll_stops_when_it_has_been_quiet() {
        assert!(keeps_polling(0));
        assert!(keeps_polling(QUIET_TURNS - 1));
        assert!(!keeps_polling(QUIET_TURNS));
        assert!(!keeps_polling(u32::MAX));
        // 25 turns of 40 ms: about a second after the last thing happened.
        assert_eq!(QUIET_TURNS * 40, 1000);
    }
}
