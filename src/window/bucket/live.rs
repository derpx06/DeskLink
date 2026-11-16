//! The Bucket page's live parts: progress bars and words that follow a
//! transfer without redrawing the page.
//!
//! Sibling edges: none.

use super::*;

thread_local! {
    /// Progress widgets on screen, updated by `bucket_progress`'s tick (at
    /// most four times a second) without rebuilding the page.
    static LIVE: std::cell::RefCell<Vec<Live>> = const { std::cell::RefCell::new(Vec::new()) };
    static LISTENING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A progress widget kept current.
pub(super) enum Live {
    /// One file moving: "340 MB of 1.2 GiB · 2 min left" and its bar.
    Tile {
        device: String,
        path: String,
        incoming: bool,
        line: glib::WeakRef<gtk::Label>,
        bar: glib::WeakRef<gtk::ProgressBar>,
    },
    /// A folder tile: everything moving inside it, added up; `idle` when
    /// nothing is.
    Folder {
        device: String,
        prefix: String,
        idle: String,
        line: glib::WeakRef<gtk::Label>,
        bar: glib::WeakRef<gtk::ProgressBar>,
    },
    /// The page header or the overview card: the sync's summary while one
    /// goes, `idle` otherwise.
    Summary {
        device: String,
        idle: String,
        /// The idle words again, for a text that ages ("2 min ago"): the
        /// page is not rebuilt for it.
        idle_now: Option<std::rc::Rc<dyn Fn() -> String>>,
        line: glib::WeakRef<gtk::Label>,
        bar: glib::WeakRef<gtk::ProgressBar>,
        speed: Option<glib::WeakRef<gtk::Label>>,
    },
}

pub(super) fn watch(live: Live) {
    update_live(&live);
    LIVE.with(|all| all.borrow_mut().push(live));
    if !LISTENING.replace(true) {
        crate::bucket_progress::on_tick(|| {
            LIVE.with(|all| all.borrow_mut().retain(update_live));
            true
        });
    }
}

fn set_text(label: &gtk::Label, text: &str) {
    if label.label() != text {
        label.set_label(text);
    }
}

fn set_bar(bar: &gtk::ProgressBar, fraction: f64, text: &str) {
    if (bar.fraction() - fraction).abs() > f64::EPSILON {
        bar.set_fraction(fraction);
    }
    bar.update_property(&[gtk::accessible::Property::ValueText(text)]);
}

/// Shows the latest numbers; `false` once the widgets are gone.
fn update_live(live: &Live) -> bool {
    match live {
        Live::Tile {
            device,
            path,
            incoming,
            line,
            bar,
        } => {
            let (Some(line), Some(bar)) = (line.upgrade(), bar.upgrade()) else {
                return false;
            };
            if !line.is_mapped() && line.label() != "" {
                return true;
            }
            let tile = crate::bucket_progress::tile(device, path, *incoming);
            let text = tile
                .as_ref()
                .map_or_else(|| "Waiting…".to_string(), |tile| tile.line());
            set_text(&line, &text);
            set_bar(&bar, tile.map_or(0.0, |tile| tile.fraction()), &text);
        }
        Live::Folder {
            device,
            prefix,
            idle,
            line,
            bar,
        } => {
            let (Some(line), Some(bar)) = (line.upgrade(), bar.upgrade()) else {
                return false;
            };
            match crate::bucket_progress::folder(device, prefix) {
                Some(tile) => {
                    let text = tile.line();
                    set_text(&line, &text);
                    set_bar(&bar, tile.fraction(), &text);
                    bar.set_visible(true);
                }
                None => {
                    set_text(&line, idle);
                    bar.set_visible(false);
                }
            }
        }
        Live::Summary {
            device,
            idle,
            idle_now,
            line,
            bar,
            speed,
        } => {
            let (Some(line), Some(bar)) = (line.upgrade(), bar.upgrade()) else {
                return false;
            };
            if !line.is_mapped() && line.label() != "" {
                return true;
            }
            let speed = speed.as_ref().and_then(|speed| speed.upgrade());
            match crate::bucket_progress::summary(device) {
                Some(summary) => {
                    let text = summary.line();
                    set_text(&line, &text);
                    set_bar(&bar, summary.fraction(), &text);
                    bar.set_visible(true);
                    if let Some(speed) = speed {
                        let words = if summary.paused || summary.speed < 1.0 {
                            String::new()
                        } else {
                            crate::bucket_progress::speed_words(summary.speed)
                        };
                        speed.set_visible(!words.is_empty());
                        set_text(&speed, &words);
                    }
                }
                None => {
                    match idle_now {
                        Some(now) => set_text(&line, &now()),
                        None => set_text(&line, idle),
                    }
                    bar.set_visible(false);
                    if let Some(speed) = speed {
                        speed.set_visible(false);
                    }
                }
            }
        }
    }
    true
}

/// A thin bar for screen readers too.
pub(super) fn thin_bar(name: &str) -> gtk::ProgressBar {
    let bar = ui_kit::progress(0.0);
    bar.update_property(&[gtk::accessible::Property::Label(name)]);
    bar
}
