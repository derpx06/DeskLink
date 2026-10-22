//! The Contacts page (F5): the contacts the phone shares with this computer,
//! searchable, in alphabetical sections, each with Call (asked first, the
//! phone places it), Message (opens the phone's messaging app) and Copy.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};

use super::{can_send_feature, text, toast_from, DesklinkWindow};
use crate::device_links::contacts::{self, Contact};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::DeviceView;
use crate::device_links::packet::{
    PACKET_TYPE_CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS, PACKET_TYPE_TELEPHONY_REQUEST,
};

#[derive(Debug)]
pub(super) struct Page {
    pub(super) root: gtk::Widget,
    search: gtk::SearchEntry,
    refresh: gtk::Button,
    count: gtk::Label,
    list: gtk::ListBox,
    stack: gtk::Stack,
    empty: adw::StatusPage,
    /// What is shown: the device, its name and the cache generation.
    shown: RefCell<(String, String, u64)>,
    /// The listed contacts by uid, for the details and the search.
    contacts: Rc<RefCell<HashMap<String, Contact>>>,
    /// The device the details act for: id and whether it can call.
    device: Rc<RefCell<(String, bool)>>,
}

/// What a search term is matched against: name, numbers (digits too),
/// emails and company, lowercased.
fn haystack(contact: &Contact) -> String {
    let mut words = vec![contact.name.clone()];
    for phone in &contact.phones {
        words.push(phone.number.clone());
        words.push(phone.number.chars().filter(char::is_ascii_digit).collect());
    }
    words.extend(contact.emails.iter().cloned());
    words.extend(contact.org.iter().cloned());
    words.join(" ").to_lowercase()
}

/// The section a name sorts under: its first letter, or "#".
fn section(name: &str) -> String {
    name.chars()
        .next()
        .filter(|c| c.is_alphabetic())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "#".to_string())
}

fn avatar(contact: &Contact, size: i32) -> adw::Avatar {
    let avatar = adw::Avatar::new(size, Some(&contact.name), true);
    if let Some(texture) = contact
        .photo_bytes()
        .and_then(|bytes| gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)).ok())
    {
        avatar.set_custom_image(Some(&texture));
    }
    avatar
}

impl Page {
    pub(super) fn new(win: &DesklinkWindow) -> Self {
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search contacts")
            .hexpand(true)
            .build();
        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh from the phone")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        refresh.update_property(&[gtk::accessible::Property::Label("Refresh from the phone")]);
        let header = gtk::Box::builder().spacing(8).build();
        header.append(&search);
        header.append(&refresh);
        let count = text("", &["text-dim"]);
        count.set_xalign(0.0);

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .valign(gtk::Align::Start)
            .build();
        list.update_property(&[gtk::accessible::Property::Label("Contacts")]);
        let none = text("No matching contacts", &["text-dim"]);
        none.set_margin_top(18);
        none.set_margin_bottom(18);
        list.set_placeholder(Some(&none));
        let empty = adw::StatusPage::builder()
            .icon_name("x-office-address-book-symbolic")
            .vexpand(true)
            .build();
        empty.add_css_class("compact");
        let stack = gtk::Stack::builder().vhomogeneous(false).build();
        stack.add_named(&list, Some("list"));
        stack.add_named(&empty, Some("empty"));

        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(8)
            .margin_bottom(28)
            .margin_start(28)
            .margin_end(28)
            .build();
        page.append(&header);
        page.append(&count);
        page.append(&stack);
        let clamp = adw::Clamp::builder()
            .maximum_size(760)
            .tightening_threshold(560)
            .child(&page)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&clamp)
            .build();

        let contacts: Rc<RefCell<HashMap<String, Contact>>> = Rc::default();
        let terms = Rc::new(RefCell::new(String::new()));
        {
            let (contacts, terms) = (Rc::clone(&contacts), Rc::clone(&terms));
            list.set_filter_func(move |row| {
                let terms = terms.borrow();
                if terms.is_empty() {
                    return true;
                }
                let contacts = contacts.borrow();
                let Some(found) = contacts.get(row.widget_name().as_str()) else {
                    return false;
                };
                let haystack = haystack(found);
                terms.split_whitespace().all(|term| haystack.contains(term))
            });
        }
        list.set_header_func(|row, before| {
            let title = |row: &gtk::ListBoxRow| {
                row.downcast_ref::<adw::ActionRow>()
                    .map(|row| section(&row.title()))
            };
            let letter = title(row);
            if before.and_then(title) == letter {
                row.set_header(None::<&gtk::Widget>);
            } else if let Some(letter) = letter {
                let heading = text(&letter, &["list-heading"]);
                heading.set_xalign(0.0);
                heading.set_margin_start(12);
                heading.set_margin_top(8);
                heading.set_margin_bottom(4);
                row.set_header(Some(&heading));
            }
        });
        {
            let (list, terms) = (list.clone(), Rc::clone(&terms));
            search.connect_search_changed(move |entry| {
                terms.replace(entry.text().trim().to_lowercase());
                list.invalidate_filter();
                list.invalidate_headers();
            });
        }
        let device: Rc<RefCell<(String, bool)>> = Rc::default();
        {
            let (contacts, device, win) =
                (Rc::clone(&contacts), Rc::clone(&device), win.downgrade());
            list.connect_row_activated(move |_, row| {
                let Some(win) = win.upgrade() else {
                    return;
                };
                let contact = contacts.borrow().get(row.widget_name().as_str()).cloned();
                if let Some(contact) = contact {
                    let (id, can_call) = device.borrow().clone();
                    show_details(&win, &id, can_call, &contact);
                }
            });
        }
        let page = Self {
            root: scroll.upcast(),
            search,
            refresh,
            count,
            list,
            stack,
            empty,
            shown: RefCell::default(),
            contacts,
            device,
        };
        {
            let device = Rc::clone(&page.device);
            let count = page.count.clone();
            page.refresh.connect_clicked(move |button| {
                let id = device.borrow().0.clone();
                crate::device_links::desk_features::run_later(move || {
                    crate::device_links::desk_features::send(&id, &contacts::refresh_request())
                });
                count.set_label("Asking the phone…");
                toast_from(button, "Asked the phone for its contacts");
            });
        }
        page
    }

    pub(super) fn update(
        &self,
        _win: &DesklinkWindow,
        _daemon: &DaemonHandle,
        device: &DeviceView,
    ) {
        let ready = device.feature_transport_ready;
        let can_refresh =
            ready && can_send_feature(device, PACKET_TYPE_CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS);
        self.refresh.set_sensitive(can_refresh);
        self.device.replace((
            device.id.clone(),
            ready && can_send_feature(device, PACKET_TYPE_TELEPHONY_REQUEST),
        ));
        let generation = contacts::generation();
        let shown = (device.id.clone(), device.name.clone(), generation);
        if *self.shown.borrow() == shown {
            return;
        }
        if self.shown.borrow().0 != device.id {
            self.search.set_text("");
        }
        self.shown.replace(shown);

        let list = contacts::list(&device.id);
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        let mut by_uid = HashMap::new();
        // ponytail: one row per contact, built at once; a GtkListView if
        // address books of many thousands feel slow to open.
        for contact in &list {
            let row = adw::ActionRow::builder()
                .title(&contact.name)
                .use_markup(false)
                .activatable(true)
                .build();
            let subtitle = contact
                .phones
                .first()
                .map(|phone| phone.number.clone())
                .or_else(|| contact.emails.first().cloned())
                .or_else(|| contact.org.clone());
            if let Some(subtitle) = subtitle {
                row.set_subtitle(&subtitle);
            }
            row.add_prefix(&avatar(contact, 36));
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            row.set_widget_name(&contact.uid);
            self.list.append(&row);
            by_uid.insert(contact.uid.clone(), contact.clone());
        }
        self.contacts.replace(by_uid);
        self.list.invalidate_filter();
        self.list.invalidate_headers();

        if list.is_empty() {
            self.count.set_label("");
            self.empty
                .set_title(&format!("No Contacts from {}", device.name));
            self.empty.set_description(Some(&format!(
                "Turn on “Share contacts” for this computer in DeskLink on {}. \
                 They are kept here encrypted, and removed when you unpair.",
                device.name
            )));
            self.stack.set_visible_child_name("empty");
        } else {
            self.count.set_label(&match list.len() {
                1 => format!("1 contact from {}", device.name),
                count => format!("{count} contacts from {}", device.name),
            });
            self.stack.set_visible_child_name("list");
        }
    }

    /// Opens `uid`'s details (from search), once the page shows `device_id`.
    pub(super) fn show_contact(&self, win: &DesklinkWindow, device_id: &str, uid: &str) {
        if let Some(contact) = contacts::by_uid(device_id, uid) {
            let can_call = self.device.borrow().1 && self.device.borrow().0 == device_id;
            show_details(win, device_id, can_call, &contact);
        }
    }
}

/// The contact's card: photo, name, numbers with Call, Message and Copy,
/// emails, company and note.
fn show_details(win: &DesklinkWindow, device_id: &str, can_call: bool, contact: &Contact) {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(18)
        .margin_end(18)
        .build();
    let top = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .halign(gtk::Align::Center)
        .build();
    top.append(&avatar(contact, 96));
    let name = text(&contact.name, &["title-2"]);
    name.set_wrap(true);
    name.set_justify(gtk::Justification::Center);
    top.append(&name);
    if let Some(org) = &contact.org {
        let org = text(org, &["text-dim"]);
        org.set_wrap(true);
        top.append(&org);
    }
    content.append(&top);

    let dialog = adw::Dialog::builder()
        .title(&contact.name)
        .content_width(420)
        .build();
    let copy_button = |value: String, what: &'static str| {
        let button = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text(format!("Copy {what}"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        button.update_property(&[gtk::accessible::Property::Label(&format!("Copy {what}"))]);
        button.connect_clicked(move |button| {
            button.clipboard().set_text(&value);
            toast_from(button, &format!("Copied {what}"));
        });
        button
    };
    if !contact.phones.is_empty() {
        let group = adw::PreferencesGroup::builder().title("Phone").build();
        for phone in &contact.phones {
            // "property": the kind small above, the number as the value.
            let row = adw::ActionRow::builder()
                .title(if phone.kind.is_empty() {
                    "Phone"
                } else {
                    phone.kind.as_str()
                })
                .subtitle(&phone.number)
                .use_markup(false)
                .subtitle_selectable(true)
                .build();
            row.add_css_class("property");
            let call = gtk::Button::builder()
                .icon_name("call-start-symbolic")
                .tooltip_text("Call")
                .valign(gtk::Align::Center)
                .sensitive(can_call)
                .css_classes(["flat"])
                .build();
            call.update_property(&[gtk::accessible::Property::Label(&format!(
                "Call {}",
                phone.number
            ))]);
            {
                let (win, id, number, dialog) = (
                    win.downgrade(),
                    device_id.to_string(),
                    phone.number.clone(),
                    dialog.downgrade(),
                );
                call.connect_clicked(move |_| {
                    if let Some(dialog) = dialog.upgrade() {
                        dialog.close();
                    }
                    if let Some(win) = win.upgrade() {
                        super::confirm_dial(&win, &number, Some(&id));
                    }
                });
            }
            let message = gtk::Button::builder()
                .icon_name("chat-message-new-symbolic")
                .tooltip_text("Message on the phone")
                .valign(gtk::Align::Center)
                .sensitive(can_call)
                .css_classes(["flat"])
                .build();
            message.update_property(&[gtk::accessible::Property::Label(&format!(
                "Message {} on the phone",
                phone.number
            ))]);
            {
                let (win, id, number) =
                    (win.downgrade(), device_id.to_string(), phone.number.clone());
                message.connect_clicked(move |button| {
                    let Some(daemon) = win.upgrade().and_then(|win| win.daemon()) else {
                        return;
                    };
                    daemon.send(DaemonCommand::Telephony(
                        id.clone(),
                        "message".to_string(),
                        Some(number.clone()),
                    ));
                    toast_from(button, "Opening messages on the phone");
                });
            }
            if !can_call {
                let why = "Connect the phone to call or message from here";
                call.set_tooltip_text(Some(why));
                message.set_tooltip_text(Some(why));
            }
            row.add_suffix(&call);
            row.add_suffix(&message);
            row.add_suffix(&copy_button(phone.number.clone(), "number"));
            group.add(&row);
        }
        content.append(&group);
    }
    if !contact.emails.is_empty() {
        let group = adw::PreferencesGroup::builder().title("Email").build();
        for email in &contact.emails {
            let row = adw::ActionRow::builder()
                .title(email)
                .use_markup(false)
                .build();
            row.add_suffix(&copy_button(email.clone(), "address"));
            group.add(&row);
        }
        content.append(&group);
    }
    if let Some(note) = &contact.note {
        let group = adw::PreferencesGroup::builder().title("Note").build();
        let label = text(note, &[]);
        label.set_wrap(true);
        label.set_xalign(0.0);
        label.set_selectable(true);
        group.add(&label);
        content.append(&group);
    }
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&content)
            .build(),
    ));
    dialog.set_child(Some(&view));
    dialog.present(Some(win));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contacts_sort_into_letter_sections_and_match_any_field() {
        assert_eq!(section("mara"), "M");
        assert_eq!(section("Åsa"), "Å");
        assert_eq!(section("+46 70"), "#");
        assert_eq!(section(""), "#");
        let contact = contacts::parse_vcard(
            "u",
            "FN:Mara Lindqvist\nTEL:+46 70-123\nEMAIL:mara@example.org\nORG:Acme",
        )
        .unwrap();
        let found = haystack(&contact);
        for term in ["mara", "lindq", "4670123", "70-123", "example.org", "acme"] {
            assert!(found.contains(term), "{term}");
        }
    }
}
