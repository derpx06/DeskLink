//! Transfers that failed: the last few kept to show, their plain-words
//! reasons, and their rows with Try again.
//!
//! Sibling edges: none (the parent's helpers).

use super::*;

/// A send or receive that ended badly: kept on the page until it is tried
/// again or dismissed, so the reason is not lost with a toast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Failure {
    pub(super) id: String,
    pub(super) device_id: String,
    pub(super) name: String,
    pub(super) incoming: bool,
    pub(super) reason: String,
    pub(super) retry: Option<std::path::PathBuf>,
}

/// How many failures the page keeps.
const MAX_FAILURES: usize = 5;

/// The failures part of the page, and how far along the board's outcomes it
/// has read.
pub(super) struct Failures {
    pub(super) list: gtk::Box,
    pub(super) kept: RefCell<Vec<Failure>>,
    seen: Cell<u64>,
    /// What is drawn now, so the list is rebuilt only when it changes.
    /// `None`: nothing drawn that can be trusted (the next sync redraws).
    drawn: RefCell<Option<Vec<Failure>>>,
}

/// "Pixel 9 ran out of space." as a sentence: the first letter up, a full
/// stop; "Something went wrong." when nothing was said.
pub(super) fn reason_words(reason: &str) -> String {
    let reason = reason.trim().trim_end_matches('.');
    let mut chars = reason.chars();
    match chars.next() {
        None => "Something went wrong.".to_string(),
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
    }
}

impl Failures {
    pub(super) fn new() -> Rc<Self> {
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .visible(false)
            .build();
        list.update_property(&[gtk::accessible::Property::Label("Failed transfers")]);
        Rc::new(Self {
            list,
            kept: RefCell::default(),
            // Only what fails from now on; earlier outcomes were toasts.
            seen: Cell::new(crate::device_links::webrtc::transfer_board::ended_since(u64::MAX).1),
            drawn: RefCell::default(),
        })
    }

    /// Reads the outcomes since last time and redraws when the list changed.
    pub(super) fn sync(self: &Rc<Self>, daemon: &DaemonHandle, device: &str) {
        use crate::device_links::webrtc::transfer_board::{ended_since, Outcome};
        let (news, seq) = ended_since(self.seen.get());
        self.seen.set(seq);
        {
            let mut kept = self.kept.borrow_mut();
            for done in news {
                if let Outcome::Failed(reason) = done.outcome {
                    kept.retain(|failure| failure.id != done.id);
                    kept.push(Failure {
                        id: done.id,
                        device_id: done.device_id,
                        name: done.name,
                        incoming: done.incoming,
                        reason: reason_words(&reason),
                        retry: done.retry,
                    });
                    if kept.len() > MAX_FAILURES {
                        kept.remove(0);
                    }
                }
            }
        }
        #[allow(unused_mut)]
        let mut shown: Vec<Failure> = self
            .kept
            .borrow()
            .iter()
            .filter(|failure| failure.device_id == device)
            .cloned()
            .collect();
        #[cfg(debug_assertions)]
        shown.extend(debug_failures(device));
        if self.drawn.borrow().as_ref() == Some(&shown) {
            return;
        }
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for failure in shown.iter().rev() {
            self.list.append(&failed_row(self, daemon, failure));
        }
        self.list.set_visible(!shown.is_empty());
        self.drawn.replace(Some(shown));
    }

    pub(super) fn forget(&self, id: &str) {
        self.kept.borrow_mut().retain(|failure| failure.id != id);
        self.drawn.replace(None);
    }
}

/// A failed transfer: what, why, and Try again when the file is still known.
fn failed_row(failures: &Rc<Failures>, daemon: &DaemonHandle, failure: &Failure) -> gtk::Box {
    let row = gtk::Box::builder()
        .spacing(12)
        .css_classes(["files-row", "failed"])
        .build();
    row.append(&ui_kit::icon_square("desklink-error-symbolic", Tone::Error));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let verb = if failure.incoming { "receive" } else { "send" };
    let name = ellipsized(&failure.name, &["dl-mono", "files-name"]);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    words.append(&name);
    words.append(&crate::window::wrapped(
        &format!("Could not {verb} it. {}", failure.reason),
        &["dl-secondary"],
    ));
    row.append(&words);
    if let (Some(path), false) = (&failure.retry, failure.device_id.is_empty()) {
        let again = ui_kit::button(ButtonKind::Tonal, "Try again", None);
        again.set_halign(gtk::Align::Start);
        again.set_margin_top(6);
        again.set_tooltip_text(Some("Send this file again"));
        again.update_property(&[gtk::accessible::Property::Label(&format!(
            "Try sending {} again",
            failure.name
        ))]);
        let (failures, daemon, device, path, id) = (
            Rc::downgrade(failures),
            daemon.clone(),
            failure.device_id.clone(),
            path.clone(),
            failure.id.clone(),
        );
        again.connect_clicked(move |button| {
            // Once is enough: the row goes at the next sync, and a second
            // activation before then asks for nothing.
            if !button.is_sensitive() {
                return;
            }
            button.set_sensitive(false);
            daemon.send(DaemonCommand::SendFile(device.clone(), path.clone()));
            if let Some(failures) = failures.upgrade() {
                failures.forget(&id);
            }
        });
        words.append(&again);
    }
    let dismiss = ui_kit::icon_button("desklink-close-symbolic", "Dismiss");
    dismiss.set_valign(gtk::Align::Start);
    dismiss.update_property(&[gtk::accessible::Property::Label(&format!(
        "Dismiss the failed {verb} of {}",
        failure.name
    ))]);
    {
        let (failures, id) = (Rc::downgrade(failures), failure.id.clone());
        dismiss.connect_clicked(move |_| {
            if let Some(failures) = failures.upgrade() {
                failures.forget(&id);
            }
        });
    }
    row.append(&dismiss);
    row.update_property(&[gtk::accessible::Property::Label(&format!(
        "{}: could not {verb}. {}",
        failure.name, failure.reason
    ))]);
    row
}
