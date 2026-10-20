//! The Notifications page: the phone's notifications, each with Reply (an
//! inline entry, sent to the phone), its own actions and Dismiss, and Clear
//! All.
//!
//! Cards are kept by notification id and only added, removed or reordered
//! as notifications come and go, so a reply being typed survives the
//! once-a-second refresh.

use std::cell::RefCell;

use adw::prelude::*;

use super::{ellipsized, icon_button, icon_square, text, toast_from, DesklinkWindow};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::{DeviceNotification, DeviceView};

#[derive(Debug)]
struct Card {
    id: String,
    /// What the card was drawn from.
    key: String,
    widget: gtk::Box,
    entry: gtk::Entry,
    revealer: gtk::Revealer,
}

#[derive(Debug)]
pub(super) struct Page {
    pub(super) root: gtk::Widget,
    list: gtk::Box,
    count: gtk::Label,
    clear: gtk::Button,
    empty: gtk::Box,
    hidden: gtk::Box,
    hidden_title: gtk::Label,
    cards: RefCell<Vec<Card>>,
    /// The device the cards belong to.
    device: RefCell<String>,
    /// A reply asked for from the overview, opened once its card exists.
    pending_reply: RefCell<Option<String>>,
}

impl Page {
    pub(super) fn new(win: &DesklinkWindow) -> Self {
        let count = text("", &["text-dim"]);
        let clear = super::soft_button("Clear All");
        let header = gtk::Box::builder().spacing(10).build();
        count.set_hexpand(true);
        header.append(&count);
        header.append(&clear);

        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(14)
            .build();
        let empty = status(
            "preferences-system-notifications-symbolic",
            "You're all caught up",
            "New phone notifications appear here.",
        )
        .0;
        let (hidden, hidden_title) = status(
            "preferences-system-notifications-symbolic",
            "",
            "Turn them on to see them here and on your desktop.",
        );
        let show = super::soft_button("Show Notifications");
        show.set_css_classes(&["suggested-action", "pill"]);
        show.set_halign(gtk::Align::Center);
        hidden.append(&show);

        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(14)
            .margin_top(8)
            .margin_bottom(28)
            .margin_start(28)
            .margin_end(28)
            .build();
        page.append(&header);
        page.append(&list);
        page.append(&empty);
        page.append(&hidden);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(
                &adw::Clamp::builder()
                    .maximum_size(900)
                    .tightening_threshold(700)
                    .child(&page)
                    .build(),
            )
            .build();

        let window = win.downgrade();
        clear.connect_clicked(move |button| {
            let Some(window) = window.upgrade() else {
                return;
            };
            let (Some(daemon), Some(id)) = (window.daemon(), window.selected_id()) else {
                return;
            };
            let Some(device) = window
                .known_devices(&daemon)
                .into_iter()
                .find(|device| device.id == id)
            else {
                return;
            };
            for notification in device.notifications.iter().filter(|n| n.is_clearable) {
                daemon.send(DaemonCommand::DismissNotification(
                    device.id.clone(),
                    notification.id.clone(),
                ));
            }
            toast_from(button, "Notifications cleared");
        });
        let window = win.downgrade();
        show.connect_clicked(move |_| {
            let Some(window) = window.upgrade() else {
                return;
            };
            if let (Some(daemon), Some(id)) = (window.daemon(), window.selected_id()) {
                daemon.send(DaemonCommand::SetShowNotifications(id, true));
                window.refresh_soon();
            }
        });

        Self {
            root: scroll.upcast(),
            list,
            count,
            clear,
            empty,
            hidden,
            hidden_title,
            cards: RefCell::default(),
            device: RefCell::default(),
            pending_reply: RefCell::default(),
        }
    }

    /// Opens notification `id`'s reply entry, now or once its card exists.
    pub(super) fn open_reply(&self, id: &str) {
        self.pending_reply.replace(Some(id.to_string()));
        self.open_pending();
    }

    fn open_pending(&self) {
        let Some(id) = self.pending_reply.borrow().clone() else {
            return;
        };
        if let Some(card) = self.cards.borrow().iter().find(|card| card.id == id) {
            card.revealer.set_reveal_child(true);
            card.entry.grab_focus();
            self.pending_reply.replace(None);
        }
    }

    pub(super) fn update(&self, _win: &DesklinkWindow, daemon: &DaemonHandle, device: &DeviceView) {
        if *self.device.borrow() != device.id {
            self.device.replace(device.id.clone());
            for card in self.cards.borrow_mut().drain(..) {
                self.list.remove(&card.widget);
            }
        }
        let shown = crate::device_links::config::app_settings().shows_notifications(&device.id);
        self.hidden.set_visible(!shown);
        self.list.set_visible(shown);
        self.hidden_title
            .set_label(&format!("Notifications from {} are hidden", device.name));
        if !shown {
            self.empty.set_visible(false);
            self.count.set_label("Hidden on this computer");
            self.clear.set_sensitive(false);
            return;
        }
        let notifications = &device.notifications;
        self.count.set_label(&match notifications.len() {
            0 => "None".to_string(),
            count => format!("{count} new"),
        });
        self.empty.set_visible(notifications.is_empty());
        self.clear.set_sensitive(
            notifications
                .iter()
                .any(|notification| notification.is_clearable),
        );

        let mut cards = self.cards.borrow_mut();
        cards.retain(|card| {
            let keep = notifications.iter().any(|n| n.id == card.id);
            if !keep {
                self.list.remove(&card.widget);
            }
            keep
        });
        let mut previous: Option<gtk::Box> = None;
        for notification in notifications {
            let key = serde_json::to_string(notification).unwrap_or_default();
            let position = cards.iter().position(|card| card.id == notification.id);
            let typing = |card: &Card| !card.entry.text().is_empty() || card.entry.has_focus();
            match position {
                Some(index) if cards[index].key == key || typing(&cards[index]) => {}
                Some(index) => {
                    let fresh = build_card(daemon, &device.id, notification, key);
                    self.list
                        .insert_child_after(&fresh.widget, Some(&cards[index].widget));
                    self.list.remove(&cards[index].widget);
                    cards[index] = fresh;
                }
                None => {
                    let fresh = build_card(daemon, &device.id, notification, key);
                    self.list.append(&fresh.widget);
                    cards.push(fresh);
                }
            }
            let index = cards
                .iter()
                .position(|card| card.id == notification.id)
                .unwrap_or_default();
            self.list
                .reorder_child_after(&cards[index].widget, previous.as_ref());
            previous = Some(cards[index].widget.clone());
        }
        drop(cards);
        // A reply asked for a notification that has since gone: forget it.
        let gone = self
            .pending_reply
            .borrow()
            .as_ref()
            .is_some_and(|id| notifications.iter().all(|n| &n.id != id));
        if gone {
            self.pending_reply.replace(None);
        }
        self.open_pending();
    }
}

/// An icon, a title and a line, centred: an empty or hidden list.
fn status(icon: &str, title: &str, description: &str) -> (gtk::Box, gtk::Label) {
    let status = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(80)
        .halign(gtk::Align::Center)
        .build();
    status.append(&icon_square(icon, 64, "status-icon"));
    let title = gtk::Label::builder()
        .label(title)
        .justify(gtk::Justification::Center)
        .wrap(true)
        .css_classes(["title-4"])
        .build();
    status.append(&title);
    status.append(
        &gtk::Label::builder()
            .label(description)
            .justify(gtk::Justification::Center)
            .wrap(true)
            .css_classes(["text-dim"])
            .build(),
    );
    (status, title)
}

/// A glyph for the notifying app, from its name.
pub(super) fn app_icon(app: &str) -> &'static str {
    let app = app.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| app.contains(word));
    if has(&["mail", "outlook", "proton"]) {
        "mail-unread-symbolic"
    } else if has(&["calendar", "meet"]) {
        "x-office-calendar-symbolic"
    } else if has(&["phone", "dialer", "call"]) {
        "call-start-symbolic"
    } else if has(&[
        "signal", "message", "whatsapp", "telegram", "chat", "slack", "discord", "sms",
    ]) {
        "user-available-symbolic"
    } else {
        "preferences-system-notifications-symbolic"
    }
}

/// "Mara · Signal": the title in bold, then the app.
pub(super) fn heading_line(notification: &DeviceNotification) -> gtk::Box {
    let line = gtk::Box::builder().build();
    if notification.title.is_empty() {
        line.append(&ellipsized(&notification.app_name, &["heading"]));
    } else {
        let title = ellipsized(&notification.title, &["heading"]);
        line.append(&title);
        let app = ellipsized(
            &format!(" · {}", notification.app_name),
            &["caption", "text-faint"],
        );
        app.set_valign(gtk::Align::Center);
        line.append(&app);
    }
    line
}

/// What the notification says.
pub(super) fn body(notification: &DeviceNotification) -> &str {
    if notification.text.is_empty() {
        &notification.ticker
    } else {
        &notification.text
    }
}

fn build_card(
    daemon: &DaemonHandle,
    device_id: &str,
    notification: &DeviceNotification,
    key: String,
) -> Card {
    let card = super::card(0);
    card.add_css_class("notification-card");
    let top = gtk::Box::builder().spacing(14).build();
    top.append(&icon_square(
        app_icon(&notification.app_name),
        38,
        "avatar-circle",
    ));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    words.append(&heading_line(notification));
    let said = text(body(notification), &["text-dim", "body-small"]);
    said.set_wrap(true);
    said.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    said.set_lines(3);
    said.set_ellipsize(gtk::pango::EllipsizeMode::End);
    words.append(&said);
    top.append(&words);

    for action in notification.actions.iter().take(2) {
        let button = super::soft_button(action);
        let (daemon, device_id, key, action) = (
            daemon.clone(),
            device_id.to_string(),
            notification.id.clone(),
            action.clone(),
        );
        button.connect_clicked(move |_| {
            daemon.send(DaemonCommand::TriggerNotificationAction(
                device_id.clone(),
                key.clone(),
                action.clone(),
            ));
        });
        top.append(&button);
    }

    let entry = gtk::Entry::builder()
        .placeholder_text("Write a reply…")
        .hexpand(true)
        .css_classes(["flat"])
        .build();
    entry.update_property(&[gtk::accessible::Property::Label(&format!(
        "Reply to {}",
        if notification.title.is_empty() {
            &notification.app_name
        } else {
            &notification.title
        }
    ))]);
    let revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .build();
    if let Some(reply_id) = &notification.request_reply_id {
        let reply = icon_button("mail-reply-sender-symbolic", "Reply", &["soft-button"]);
        let (shown, field) = (revealer.downgrade(), entry.downgrade());
        reply.connect_clicked(move |_| {
            if let (Some(revealer), Some(entry)) = (shown.upgrade(), field.upgrade()) {
                let open = !revealer.reveals_child();
                revealer.set_reveal_child(open);
                if open {
                    entry.grab_focus();
                }
            }
        });
        top.append(&reply);

        let bar = gtk::Box::builder()
            .spacing(8)
            .margin_start(52)
            .margin_top(12)
            .css_classes(["reply-bar"])
            .build();
        bar.append(&entry);
        let send = gtk::Button::builder()
            .icon_name("go-up-symbolic")
            .tooltip_text("Send Reply")
            .valign(gtk::Align::Center)
            .css_classes(["circular", "suggested-action", "send-button"])
            .build();
        send.update_property(&[gtk::accessible::Property::Label("Send Reply")]);
        let send_reply = {
            let (daemon, device_id, reply_id) =
                (daemon.clone(), device_id.to_string(), reply_id.clone());
            let (field, shown) = (entry.downgrade(), revealer.downgrade());
            move || {
                let Some(entry) = field.upgrade() else {
                    return;
                };
                let message = entry.text().trim().to_string();
                if message.is_empty() {
                    return;
                }
                daemon.send(DaemonCommand::ReplyNotification(
                    device_id.clone(),
                    reply_id.clone(),
                    message,
                ));
                entry.set_text("");
                if let Some(revealer) = shown.upgrade() {
                    revealer.set_reveal_child(false);
                }
                toast_from(&entry, "Reply sent");
            }
        };
        entry.connect_activate({
            let send_reply = send_reply.clone();
            move |_| send_reply()
        });
        send.connect_clicked(move |_| send_reply());
        bar.append(&send);
        revealer.set_child(Some(&bar));
    }
    if notification.is_clearable {
        let dismiss = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Dismiss")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        dismiss.update_property(&[gtk::accessible::Property::Label("Dismiss")]);
        let (daemon, device_id, id) = (
            daemon.clone(),
            device_id.to_string(),
            notification.id.clone(),
        );
        dismiss.connect_clicked(move |button| {
            button.set_sensitive(false);
            daemon.send(DaemonCommand::DismissNotification(
                device_id.clone(),
                id.clone(),
            ));
        });
        top.append(&dismiss);
    }
    card.append(&top);
    card.append(&revealer);
    Card {
        id: notification.id.clone(),
        key,
        widget: card,
        entry,
        revealer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_kind_of_app_has_a_glyph() {
        assert_eq!(app_icon("Signal"), "user-available-symbolic");
        assert_eq!(app_icon("Gmail"), "mail-unread-symbolic");
        assert_eq!(app_icon("Google Calendar"), "x-office-calendar-symbolic");
        assert_eq!(
            app_icon("Toaster"),
            "preferences-system-notifications-symbolic"
        );
    }
}
