//! The Notifications page: the phone's notifications, each with Reply (an
//! inline entry, sent to the phone), its own actions and Dismiss, and Clear
//! all. Cards are the kit's; Delete or Backspace on a focused card dismisses
//! it and Enter opens its reply.
//!
//! Cards are kept by notification id and only added, removed or reordered
//! as notifications come and go, so a reply being typed survives the
//! once-a-second refresh.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::ui_kit::{self, ButtonKind, Tone};
use super::{clipboard, text, toast_from, wrapped, DesklinkWindow};
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
    /// What a reply is sent under now. The phone mints a new id each time it
    /// posts or updates a notification, so a card kept while a reply is being
    /// typed must follow it: an old id is "No such notification" there.
    reply_id: Rc<RefCell<Option<String>>>,
}

#[derive(Debug)]
pub(super) struct Page {
    pub(super) root: gtk::Widget,
    list: gtk::Box,
    count: gtk::Label,
    clear: gtk::Button,
    empty: clipboard::EmptyState,
    hidden: clipboard::EmptyState,
    cards: RefCell<Vec<Card>>,
    /// The device the cards belong to.
    device: RefCell<String>,
    /// A reply asked for from the overview, opened once its card exists.
    pending_reply: RefCell<Option<String>>,
}

impl Page {
    pub(super) fn new(win: &DesklinkWindow) -> Self {
        clipboard::ensure_css();
        let count = wrapped("", &["dl-secondary"]);
        let clear = ui_kit::button(ButtonKind::Tonal, "Clear all", None);
        clear.set_tooltip_text(Some("Dismiss every notification here and on the phone"));
        let header = gtk::Box::builder()
            .spacing(10)
            .valign(gtk::Align::Center)
            .build();
        count.set_hexpand(true);
        count.set_valign(gtk::Align::Center);
        header.append(&count);
        header.append(&clear);

        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        let empty = clipboard::empty_state(
            "desklink-notifications-symbolic",
            "No notifications",
            "Your phone's notifications show up here.",
        );
        let hidden = clipboard::empty_state(
            "desklink-notifications-symbolic",
            "Notifications are turned off",
            "Turn them on to see them here and on your desktop.",
        );
        let show = ui_kit::button(ButtonKind::Primary, "Turn on", None);
        show.set_halign(gtk::Align::Center);
        hidden.root.append(&show);

        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(8)
            .margin_bottom(28)
            .css_classes(["page-margins"])
            .build();
        page.append(&header);
        page.append(&list);
        page.append(&empty.root);
        page.append(&hidden.root);
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

    pub(super) fn update(&self, win: &DesklinkWindow, daemon: &DaemonHandle, device: &DeviceView) {
        if *self.device.borrow() != device.id {
            self.device.replace(device.id.clone());
            for card in self.cards.borrow_mut().drain(..) {
                self.list.remove(&card.widget);
            }
        }
        let shown = crate::device_links::config::app_settings().shows_notifications(&device.id);
        self.hidden.root.set_visible(!shown);
        self.list.set_visible(shown);
        self.hidden.title.set_label(&format!(
            "Notifications from {} are turned off",
            device.name
        ));
        if !shown {
            self.empty.root.set_visible(false);
            self.count.set_label("Hidden on this computer");
            self.clear.set_sensitive(false);
            return;
        }
        let notifications = &device.notifications;
        let mut count = match notifications.len() {
            0 => "None".to_string(),
            count => format!("{count} new"),
        };
        if let Some(seen) = super::last_seen_words(device) {
            count.push_str(&format!(" · {seen}"));
        }
        self.count.set_label(&count);
        self.empty.root.set_visible(notifications.is_empty());
        self.empty.body.set_label(&empty_line(
            &device.name,
            device.feature_transport_ready,
            device.notification_access,
        ));
        // Dismiss, reply and actions go to the phone: none while the link
        // is down (the list is what it last reported).
        let ready = device.feature_transport_ready;
        self.list.set_sensitive(ready);
        self.clear.set_sensitive(
            ready
                && notifications
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
        let mut built = false;
        for notification in notifications {
            let key = serde_json::to_string(notification).unwrap_or_default();
            let position = cards.iter().position(|card| card.id == notification.id);
            let typing = |card: &Card| !card.entry.text().is_empty() || card.entry.has_focus();
            match position {
                Some(index) if cards[index].key == key || typing(&cards[index]) => {
                    // Kept as it is, but a reply goes out under the id the phone has now.
                    cards[index]
                        .reply_id
                        .replace(notification.request_reply_id.clone());
                }
                Some(index) => {
                    let fresh = build_card(daemon, &device.id, notification, key);
                    built = true;
                    // Focus on the old card (or a button in it) moves to the new one.
                    let had_focus = gtk::prelude::RootExt::focus(win).is_some_and(|focus| {
                        focus == *cards[index].widget.upcast_ref::<gtk::Widget>()
                            || focus.is_ancestor(&cards[index].widget)
                    });
                    self.list
                        .insert_child_after(&fresh.widget, Some(&cards[index].widget));
                    self.list.remove(&cards[index].widget);
                    if had_focus {
                        fresh.widget.grab_focus();
                    }
                    cards[index] = fresh;
                }
                None => {
                    let fresh = build_card(daemon, &device.id, notification, key);
                    built = true;
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
        if built {
            // A new card is laid out for the page's width (buttons under the words when narrow).
            win.apply_layout();
        }
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

/// What the empty list says: an empty list with the phone connected may also
/// mean the phone never got notification access (J11), which nothing else
/// shows.
fn empty_line(phone: &str, connected: bool, access: Option<bool>) -> String {
    if access == Some(false) {
        format!("Turn on notification access in DeskLink on {phone}.")
    } else if connected {
        format!(
            "{phone}'s notifications show up here. Nothing arriving? Allow notification access in DeskLink on {phone}."
        )
    } else {
        format!("{phone}'s notifications show up here.")
    }
}

/// A glyph for the notifying app, from its name.
pub(crate) fn app_icon(app: &str) -> &'static str {
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

/// The kit's glyph for the notifying app (`app_icon` names theme icons, for
/// the desktop's own notifications).
fn glyph(app: &str) -> &'static str {
    match app_icon(app) {
        "mail-unread-symbolic" => "desklink-description-symbolic",
        "x-office-calendar-symbolic" => "desklink-schedule-symbolic",
        "call-start-symbolic" => "desklink-call-symbolic",
        "user-available-symbolic" => "desklink-person-symbolic",
        _ => "desklink-notifications-symbolic",
    }
}

/// "Mara · Signal": the title in bold, then the app. It wraps (two lines at
/// most) rather than being cut to a few letters beside the buttons.
pub(super) fn heading_line(notification: &DeviceNotification) -> gtk::Box {
    let line = gtk::Box::builder().build();
    let label = if notification.title.is_empty() {
        wrapped(&notification.app_name, &["heading"])
    } else {
        let label = wrapped("", &["heading"]);
        label.set_use_markup(true);
        label.set_markup(&format!(
            "{}<span weight=\"normal\" size=\"smaller\" alpha=\"60%\"> · {}</span>",
            gtk::glib::markup_escape_text(&notification.title),
            gtk::glib::markup_escape_text(&notification.app_name),
        ));
        label
    };
    label.set_lines(2);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_hexpand(true);
    super::tooltip_when_cut(&label);
    line.append(&label);
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
    let card = ui_kit::card(None);
    card.set_spacing(0);
    card.add_css_class("dl-note-card");
    card.set_focusable(true);
    // The words, then the buttons: with a button of its own (reply or an
    // action) they go under the words when the page is narrow (the
    // `responsive` box turns vertical), so the title keeps its room; a card
    // with only its Dismiss cross keeps it beside the words.
    let top = gtk::Box::builder().spacing(10).build();
    if notification.request_reply_id.is_some() || !notification.actions.is_empty() {
        top.set_widget_name("responsive");
    }
    let lead = gtk::Box::builder().spacing(12).hexpand(true).build();
    let actions = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Center)
        .build();
    let who = if notification.title.is_empty() {
        &notification.app_name
    } else {
        &notification.title
    };
    lead.append(&ui_kit::icon_square(
        glyph(&notification.app_name),
        Tone::Accent,
    ));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    words.append(&heading_line(notification));
    let said = text(body(notification), &["dl-secondary"]);
    said.set_wrap(true);
    said.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    said.set_lines(3);
    said.set_ellipsize(gtk::pango::EllipsizeMode::End);
    words.append(&said);
    lead.append(&words);
    // The card reads as one thing: who, what the notification says.
    card.update_property(&[gtk::accessible::Property::Label(&format!(
        "{who}, {}: {}",
        notification.app_name,
        body(notification)
    ))]);

    for action in notification.actions.iter().take(2) {
        let button = ui_kit::button(ButtonKind::Text, action, None);
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
        actions.append(&button);
    }

    let entry = gtk::Entry::builder()
        .placeholder_text("Write a reply…")
        .hexpand(true)
        .css_classes(["flat"])
        .build();
    entry.update_property(&[gtk::accessible::Property::Label(&format!("Reply to {who}"))]);
    let revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .build();
    let mut reply_button = None;
    let reply_id = Rc::new(RefCell::new(notification.request_reply_id.clone()));
    if notification.request_reply_id.is_some() {
        let reply = ui_kit::button(ButtonKind::Tonal, "Reply", Some("desklink-reply-symbolic"));
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
        actions.append(&reply);
        reply_button = Some(reply);

        let bar = gtk::Box::builder()
            .spacing(8)
            .margin_start(52)
            .margin_top(12)
            .css_classes(["dl-reply-bar"])
            .build();
        bar.append(&entry);
        let send = gtk::Button::builder()
            .icon_name("desklink-arrow-upward-symbolic")
            .tooltip_text("Send reply")
            .valign(gtk::Align::Center)
            .css_classes(["dl-icon-btn", "dl-send"])
            .build();
        send.update_property(&[gtk::accessible::Property::Label("Send reply")]);
        let send_reply = {
            let (daemon, device_id, reply_id) =
                (daemon.clone(), device_id.to_string(), Rc::clone(&reply_id));
            let (field, shown) = (entry.downgrade(), revealer.downgrade());
            move || {
                let Some(entry) = field.upgrade() else {
                    return;
                };
                let message = entry.text().trim().to_string();
                if message.is_empty() {
                    return;
                }
                // The notification can have lost its reply since: say so, keep the text.
                let Some(reply_id) = reply_id.borrow().clone() else {
                    toast_from(&entry, "This notification can't be replied to any more");
                    return;
                };
                daemon.send(DaemonCommand::ReplyNotification(
                    device_id.clone(),
                    reply_id,
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
    let mut dismiss_button = None;
    if notification.is_clearable {
        let dismiss = ui_kit::icon_button("desklink-close-symbolic", "Dismiss");
        dismiss.set_valign(gtk::Align::Center);
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
        actions.append(&dismiss);
        dismiss_button = Some(dismiss);
    }
    // Keyboard: on the card itself (not while typing a reply), Delete or
    // Backspace dismisses it and Enter opens the reply.
    let keys = gtk::EventControllerKey::new();
    {
        let (card_weak, dismiss, reply) = (
            card.downgrade(),
            dismiss_button.as_ref().map(|button| button.downgrade()),
            reply_button.as_ref().map(|button| button.downgrade()),
        );
        keys.connect_key_pressed(move |_, key, _, _| {
            use gtk::gdk::Key;
            if !card_weak.upgrade().is_some_and(|card| card.is_focus()) {
                return glib::Propagation::Proceed;
            }
            match key {
                Key::Delete | Key::BackSpace => {
                    if let Some(button) = dismiss.as_ref().and_then(|button| button.upgrade()) {
                        if button.is_sensitive() {
                            button.emit_clicked();
                        }
                        return glib::Propagation::Stop;
                    }
                }
                Key::Return | Key::KP_Enter => {
                    if let Some(button) = reply.as_ref().and_then(|button| button.upgrade()) {
                        button.emit_clicked();
                        return glib::Propagation::Stop;
                    }
                }
                _ => {}
            }
            glib::Propagation::Proceed
        });
    }
    card.add_controller(keys);
    actions.set_visible(actions.first_child().is_some());
    top.append(&lead);
    top.append(&actions);
    card.append(&top);
    card.append(&revealer);
    Card {
        id: notification.id.clone(),
        key,
        widget: card,
        entry,
        revealer,
        reply_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_list_with_the_phone_connected_points_at_notification_access() {
        assert!(
            empty_line("Pixel", true, None).contains("notification access in DeskLink on Pixel")
        );
        assert!(!empty_line("Pixel", false, None).contains("access"));
        // The phone said so: it is the answer, connected or not.
        assert!(empty_line("Pixel", true, Some(false)).starts_with("Turn on notification access"));
        assert!(!empty_line("Pixel", true, Some(true)).starts_with("Turn on"));
    }

    /// Every widget under `root`, depth first.
    fn descendants(root: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut found = vec![root.clone()];
        let mut child = root.first_child();
        while let Some(next) = child {
            found.extend(descendants(&next));
            child = next.next_sibling();
        }
        found
    }

    /// A long title wraps (two lines) instead of being cut to a few letters
    /// beside the buttons, the buttons sit in a box that stacks under the
    /// words on a narrow page, and each icon-only button has a name.
    /// (Needs a display.)
    #[test]
    fn a_notification_card_wraps_its_title_and_names_its_buttons() {
        crate::test_display::run(|| {
            let (daemon, _commands) = DaemonHandle::detached();
            let notification = DeviceNotification {
                id: "1".into(),
                app_name: "Signal".into(),
                title: "A rather long title that has to go somewhere on a narrow page".into(),
                text: "Are you deploying today?".into(),
                ticker: String::new(),
                is_clearable: true,
                request_reply_id: Some("r1".into()),
                actions: vec!["Snooze".into()],
                silent: false,
            };
            let card = build_card(&daemon, "phone", &notification, "k".into());
            let widgets = descendants(card.widget.upcast_ref());
            let title = widgets
                .iter()
                .filter_map(|w| w.downcast_ref::<gtk::Label>())
                .find(|label| label.label().contains("A rather long title"))
                .expect("the title");
            assert!(title.wraps() && title.lines() == 2, "wraps to two lines");
            assert!(
                widgets.iter().any(|w| w.widget_name() == "responsive"),
                "the buttons stack under the words on a narrow page"
            );
            for button in widgets
                .iter()
                .filter_map(|w| w.downcast_ref::<gtk::Button>())
            {
                assert!(
                    button.label().is_some()
                        || button.tooltip_text().is_some()
                        || button.child().is_some_and(|c| !descendants(&c)
                            .iter()
                            .filter_map(|w| w.downcast_ref::<gtk::Label>())
                            .all(|l| l.label().is_empty())),
                    "a button with a name"
                );
            }
            let names: Vec<_> = widgets
                .iter()
                .filter_map(|w| w.downcast_ref::<gtk::Button>())
                .filter_map(|b| b.tooltip_text())
                .collect();
            assert!(names.iter().any(|name| name == "Dismiss"));
            assert!(names.iter().any(|name| name == "Send reply"));
        });
    }

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

    /// A card kept while a reply is typed sends it under the id the phone has
    /// now, not the one it had when the card was built. (Needs a display.)
    #[test]
    fn a_reply_is_sent_under_the_newest_reply_id() {
        crate::test_display::run(|| {
            let (daemon, commands) = DaemonHandle::detached();
            let notification = DeviceNotification {
                id: "1".into(),
                app_name: "Signal".into(),
                title: "Mara".into(),
                text: "Are you deploying today?".into(),
                ticker: String::new(),
                is_clearable: true,
                request_reply_id: Some("r1".into()),
                actions: vec![],
                silent: false,
            };
            let card = build_card(&daemon, "phone", &notification, "k".into());
            card.entry.set_text("yes, at five");
            // The phone updated the notification: a new id.
            card.reply_id.replace(Some("r2".into()));
            card.entry.emit_activate();
            match commands.try_recv() {
                Ok(DaemonCommand::ReplyNotification(device, id, message)) => {
                    assert_eq!((device.as_str(), id.as_str()), ("phone", "r2"));
                    assert_eq!(message, "yes, at five");
                }
                other => panic!("expected the reply, got {other:?}"),
            }
            // No reply id any more: nothing is sent and the text stays.
            card.entry.set_text("late");
            card.reply_id.replace(None);
            card.entry.emit_activate();
            assert!(commands.try_recv().is_err());
            assert_eq!(card.entry.text(), "late");
        });
    }
}
