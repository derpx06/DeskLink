//! The Preferences page, for the selected phone and this computer: Access,
//! Connection and Appearance, and Unpair. With no phone paired yet the
//! computer-wide part is offered as a dialog instead.
//!
//! Layout: this file holds the live switches, the pages and their navigation,
//! the shared row helpers (icons, markup) and what the rest of the window uses;
//! `groups` builds a page's groups from `computer_rows` and `phone_rows`, and
//! `search` lists every row's words for finding a setting. No cycles.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use super::ui_kit::{self, ButtonKind, Tone};
use super::{clipboard, DesklinkWindow};
use crate::device_links::config::AppSettings;
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::DeviceView;
use crate::device_links::relay_link::RELAY_ENABLED;

mod computer_rows;
mod groups;
mod phone_rows;
mod search;
use computer_rows::*;
use groups::*;
use phone_rows::*;
use search::*;

// What the rest of the window uses (everything else stays inside).
pub(crate) use groups::show_dialog;
pub(super) use phone_rows::approve_remote_control;
#[cfg(test)]
pub(super) use search::{fits, SYNONYMS};
pub(super) use search::{GROUPS, ROWS};

/// A switch that follows a setting a phone can also change, and how to read
/// it. Synced in place each refresh: rebuilding the page would move focus.
type LiveSwitch = (
    glib::WeakRef<adw::SwitchRow>,
    fn(&AppSettings, &str) -> bool,
);

thread_local! {
    static LIVE: std::cell::RefCell<Vec<LiveSwitch>> = const { std::cell::RefCell::new(Vec::new()) };
}

thread_local! {
    /// Set while [`sync_live`] moves a switch to what a phone set: the move
    /// is not the person's click and must not be sent back.
    static SYNCING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A followed switch's handler: runs for a click, not for [`sync_live`].
fn toggled(row: &adw::SwitchRow, handler: impl Fn(&adw::SwitchRow) + 'static) {
    row.connect_active_notify(move |row| {
        if !SYNCING.with(|syncing| syncing.get()) {
            handler(row);
        }
    });
}

fn follow(row: &adw::SwitchRow, read: fn(&AppSettings, &str) -> bool) {
    LIVE.with(|live| live.borrow_mut().push((row.downgrade(), read)));
}

/// Shows changes made from a phone (or anywhere) on the open switches.
fn sync_live(device_id: &str) {
    let settings = crate::device_links::config::app_settings();
    LIVE.with(|live| {
        live.borrow_mut().retain(|(row, read)| {
            let Some(row) = row.upgrade() else {
                return false;
            };
            let value = read(&settings, device_id);
            if row.is_active() != value {
                SYNCING.with(|syncing| syncing.set(true));
                row.set_active(value);
                SYNCING.with(|syncing| syncing.set(false));
            }
            true
        })
    });
}

pub(super) fn update(win: &DesklinkWindow, daemon: &DaemonHandle, device: &DeviceView) {
    sync_live(&device.id);
    let imp = win.imp();
    let approving = imp.approving.borrow().contains(&device.id);
    let approved = crate::device_links::config::remote_desktop_token(&device.id).is_some();
    // Only what the switches cannot show by themselves: a switch the user
    // just flipped already says so, and redrawing would move focus.
    let settings = crate::device_links::config::app_settings();
    // Glide's availability and problem; its switch and edge update in place.
    let glide_status = crate::device_links::glide::status();
    let needs = super::requirements::missing(&super::requirements::Facts::now(Some(device)));
    let key = serde_json::to_string(&(
        super::requirements::key(&needs),
        format!(
            "{:?}{:?}{}{}{:?}{}{}",
            glide_status.availability,
            glide_status.problem,
            glide_status.asking,
            glide_status.can_ask_again,
            glide_status.ready,
            crate::device_links::glide::phone_needs_service(&device.id),
            device.feature_transport_ready
        ),
        // The phone's "allowed" report changes the glide row's words.
        crate::device_links::glide::phone_allows(&device.id),
        crate::device_links::glide::phone_locked(&device.id),
        // Every feature only the phone can turn on, and its ask.
        format!(
            "{:?}{:?}",
            crate::device_links::glide::phone_ask(&device.id),
            super::requirements::PhoneFacts::from_device(device).steps
        ),
        &device.id,
        &device.name,
        approving,
        approved,
        device.terminal_access,
        top_bar_key(),
        settings.forward_notifications,
        &settings.forward_apps,
        crate::device_links::desk_notifications::seen_apps(),
        settings.desk_mic_to_phone,
        crate::device_links::folders::bucket_on(&device.id),
    ))
    .unwrap_or_default();
    if !win.page_changed("settings", key) {
        return;
    }
    LIVE.with(|live| live.borrow_mut().clear());
    clipboard::ensure_css();
    let scroll = imp
        .bodies
        .borrow()
        .get("settings")
        .map(|(scroll, _)| scroll.vadjustment());
    let view = build_view(scroll, daemon, &settings, device, &glide_status, approving);
    win.replace_body(
        "settings",
        &adw::Clamp::builder()
            .maximum_size(720)
            .child(&view.root)
            .build(),
    );
}

/// Where the ten groups sit in the list (`UI-REDESIGN.md` 3.17): the section,
/// the group (an index into [`GROUPS`]), its glyph and its one line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    Computer,
    Phone,
    Privacy,
}

const PAGES: [(Section, usize, &str, &str); 10] = [
    (
        Section::Computer,
        0,
        "desklink-wifi-symbolic",
        "Stay awake, start at login",
    ),
    (
        Section::Phone,
        1,
        "desklink-notifications-symbolic",
        "Show, forward and silence",
    ),
    (
        Section::Phone,
        2,
        "desklink-content-paste-symbolic",
        "Share what you copy",
    ),
    (
        Section::Phone,
        3,
        "desklink-call-symbolic",
        "Names and music",
    ),
    (
        Section::Phone,
        4,
        "desklink-volume-up-symbolic",
        "Microphone",
    ),
    (
        Section::Phone,
        5,
        "desklink-mouse-symbolic",
        "Glide and remote control",
    ),
    (
        Section::Phone,
        6,
        "desklink-folder-symbolic",
        "Bucket, downloads, photos",
    ),
    (
        Section::Privacy,
        7,
        "desklink-lock-symbolic",
        "Password, terminals, unlock, unpair",
    ),
    (
        Section::Privacy,
        8,
        "desklink-palette-symbolic",
        "Style and tray icon",
    ),
    (
        Section::Privacy,
        9,
        "desklink-tune-symbolic",
        if RELAY_ENABLED {
            "Away from home, history, checks"
        } else {
            "History, checks"
        },
    ),
];

fn section_label(section: Section, phone: &str) -> String {
    match section {
        Section::Computer => "This computer".to_string(),
        Section::Phone => format!("Share with {phone}"),
        Section::Privacy => "Privacy and safety".to_string(),
    }
}

thread_local! {
    /// The subpage open (an index into [`GROUPS`]) and the view it is in:
    /// a rebuild (a setting changed) puts the person back on the same page.
    static OPEN: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// What was typed in the search box, kept across rebuilds.
    static QUERY: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
    /// Where each page (`None` is the list) was scrolled to.
    static SCROLLS: std::cell::RefCell<std::collections::HashMap<Option<usize>, f64>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    static NAV: std::cell::RefCell<glib::WeakRef<adw::NavigationView>> =
        std::cell::RefCell::new(glib::WeakRef::new());
    static SEARCH: std::cell::RefCell<glib::WeakRef<gtk::SearchEntry>> =
        std::cell::RefCell::new(glib::WeakRef::new());
}

fn tag(index: usize) -> String {
    format!("page-{index}")
}

/// Whether subpage `index` is already pushed (showing, or on its way).
fn shows(nav: &adw::NavigationView, index: usize) -> bool {
    let stack = nav.navigation_stack();
    (0..stack.n_items()).any(|i| {
        stack
            .item(i)
            .and_downcast::<adw::NavigationPage>()
            .and_then(|page| page.tag())
            .is_some_and(|shown| shown == tag(index))
    })
}

/// The Settings page: a list of destinations that push a subpage each.
struct View {
    root: adw::NavigationView,
    /// The subpages, by group index (the tests read their rows).
    #[cfg_attr(not(test), allow(dead_code))]
    pages: Vec<adw::NavigationPage>,
}

fn build_view(
    scroll: Option<gtk::Adjustment>,
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
    glide_status: &crate::device_links::glide::Status,
    approving: bool,
) -> View {
    let groups = groups(daemon, settings, device, glide_status, approving);
    // Markup, to set back while searching. libadwaita 1.5 (Ubuntu 24.04)
    // hands back the shown text, which set as markup drew no heading for
    // "Privacy & security"; later releases hand back the markup.
    let titles: Vec<String> = groups
        .iter()
        .map(|group| glib::markup_escape_text(&plain(&group.title())).to_string())
        .collect();
    let nav = adw::NavigationView::builder()
        .animate_transitions(true)
        .build();
    let searchables = searchable(&groups);

    // Subpages: a way back, the title, the phone it is for, then the group.
    let mut pages = Vec::new();
    let mut homes = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        let section = PAGES[index].0;
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(8)
            .margin_bottom(28)
            .css_classes(["page-margins"])
            .build();
        let back = ui_kit::button(
            ButtonKind::Text,
            "Settings",
            Some("desklink-arrow-back-symbolic"),
        );
        back.set_halign(gtk::Align::Start);
        back.set_tooltip_text(Some("Back to Settings (Esc)"));
        {
            let nav = nav.downgrade();
            back.connect_clicked(move |_| {
                if let Some(nav) = nav.upgrade() {
                    nav.pop();
                }
            });
        }
        content.append(&back);
        let title = gtk::Label::builder()
            .label(GROUPS[index])
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dl-title"])
            .accessible_role(gtk::AccessibleRole::Heading)
            .build();
        title.update_property(&[gtk::accessible::Property::Level(1)]);
        content.append(&title);
        let whose = match section {
            Section::Phone => format!("For {}", device.name),
            Section::Privacy if index == 7 => format!("For {}", device.name),
            _ => "For this computer".to_string(),
        };
        content.append(&super::wrapped(&whose, &["dl-page-sub"]));
        let home = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        group.set_title("");
        group.add_css_class("dl-settings-group");
        home.append(group);
        content.append(&home);
        let keys = gtk::EventControllerKey::new();
        {
            let nav = nav.downgrade();
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape {
                    if let Some(nav) = nav.upgrade() {
                        return if nav.pop() {
                            glib::Propagation::Stop
                        } else {
                            glib::Propagation::Proceed
                        };
                    }
                }
                glib::Propagation::Proceed
            });
        }
        content.add_controller(keys);
        let page = adw::NavigationPage::builder()
            .title(GROUPS[index])
            .tag(tag(index))
            .child(&content)
            .build();
        nav.add(&page);
        pages.push(page);
        homes.push(home);
    }

    // The list.
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .build();
    // Setup: what is still missing, the same list as Home's card.
    let needs = super::requirements::missing(&super::requirements::Facts::now(Some(device)));
    if !needs.is_empty() {
        let setup = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        setup.set_widget_name("settings-setup");
        setup.append(&ui_kit::section_header("Setup"));
        setup.append(&super::requirements::rows(&needs));
        if needs.iter().any(|need| need.allow.is_some()) {
            let all = ui_kit::button(ButtonKind::Tonal, "Allow all on this computer", None);
            all.set_halign(gtk::Align::Start);
            let (daemon, needs) = (daemon.clone(), needs.clone());
            all.connect_clicked(move |button| {
                super::requirements::allow_all(button, &daemon, &needs)
            });
            setup.append(&all);
        }
        list.append(&setup);
    }
    let quick = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    quick.append(&ui_kit::section_header("Quick switches"));
    let switches: Vec<adw::ActionRow> = vec![
        show_notifications_row(daemon, settings, device).upcast(),
        clipboard_row(daemon, settings).upcast(),
        keep_awake_row(daemon, settings).upcast(),
    ];
    quick.append(&ui_kit::row_group(&switches));
    list.append(&quick);
    for section in [Section::Computer, Section::Phone, Section::Privacy] {
        let label = section_label(section, &device.name);
        let header = ui_kit::section_header(&label);
        header.set_wrap(false);
        header.set_ellipsize(gtk::pango::EllipsizeMode::End);
        header.set_tooltip_text(Some(&label));
        let mut rows = Vec::new();
        for (page_section, index, icon, summary) in PAGES {
            if page_section != section {
                continue;
            }
            let row = ui_kit::list_row(icon, Tone::Neutral, GROUPS[index], Some(summary));
            row.set_activatable(true);
            row.add_suffix(
                &gtk::Image::builder()
                    .icon_name("desklink-chevron-right-symbolic")
                    .accessible_role(gtk::AccessibleRole::Presentation)
                    .build(),
            );
            let nav = nav.downgrade();
            row.connect_activated(move |_| {
                // A second activation while the push animates (a double
                // click) would push it again: Adwaita-CRITICAL, not a page.
                if let Some(nav) = nav.upgrade().filter(|nav| !shows(nav, index)) {
                    nav.push_by_tag(&tag(index));
                }
            });
            rows.push(row);
        }
        let block = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .build();
        block.append(&header);
        block.append(&ui_kit::row_group(&rows));
        list.append(&block);
    }
    // Help: the Get started guide (also in the main menu and Find anything).
    let guide = ui_kit::list_row(
        "desklink-info-symbolic",
        Tone::Neutral,
        "Get started",
        Some("What DeskLink can do, and how to start each one"),
    );
    guide.set_widget_name("settings-guide");
    guide.set_activatable(true);
    guide.add_suffix(
        &gtk::Image::builder()
            .icon_name("desklink-chevron-right-symbolic")
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    guide.connect_activated(|row| {
        row.activate_action("win.guide", None).ok();
    });
    let help = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    help.append(&ui_kit::section_header("Help"));
    help.append(&ui_kit::row_group(&[guide]));
    list.append(&help);

    // Search: the matching rows themselves, under their group's name.
    let results = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .build();
    let nothing = super::wrapped("No setting matches. Try another word.", &["dl-secondary"]);
    nothing.set_visible(false);
    results.append(&nothing);
    let stack = gtk::Stack::builder().vhomogeneous(false).build();
    stack.add_named(&list, Some("list"));
    stack.add_named(&results, Some("results"));
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search settings")
        .build();
    search.update_property(&[gtk::accessible::Property::Label("Search settings")]);
    {
        let (groups, results, stack, nothing) = (
            groups.clone(),
            results.clone(),
            stack.clone(),
            nothing.clone(),
        );
        let apply = std::rc::Rc::new(move |search: &gtk::SearchEntry| {
            let query = search.text();
            QUERY.with(|slot| slot.replace(query.to_string()));
            let searching = !query.trim().is_empty();
            for (index, group) in groups.iter().enumerate() {
                let target = if searching { &results } else { &homes[index] };
                if group.parent().as_ref() != Some(target.upcast_ref()) {
                    if let Some(old) = group.parent().and_downcast::<gtk::Box>() {
                        old.remove(group);
                    }
                    target.append(group);
                    group.set_title(if searching { &titles[index] } else { "" });
                }
            }
            let any = filter_groups(&searchables, &query);
            nothing.set_visible(searching && !any);
            stack.set_visible_child_name(if searching { "results" } else { "list" });
        });
        search.connect_search_changed({
            let apply = apply.clone();
            move |search| (*apply)(search)
        });
        // A rebuild (a setting changed) keeps what was typed.
        let kept = QUERY.with(|slot| slot.borrow().clone());
        if !kept.is_empty() {
            search.set_text(&kept);
            (*apply)(&search);
        }
    }
    let root_content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(8)
        .margin_bottom(28)
        .css_classes(["page-margins"])
        .build();
    root_content.append(&super::wrapped(
        "Changes apply right away.",
        &["dl-page-sub"],
    ));
    root_content.append(&search);
    root_content.append(&stack);
    let root_page = adw::NavigationPage::builder()
        .title("Settings")
        .tag("list")
        .child(&root_content)
        .build();
    nav.add(&root_page);
    nav.push(&root_page);

    // Go back to the subpage that was open before a rebuild, then remember
    // the one open from now on, and where each page was scrolled to: the
    // window's one scroller must not carry the list's offset into a subpage.
    let open = OPEN
        .with(|slot| slot.get())
        .filter(|index| *index < pages.len());
    OPEN.with(|slot| slot.set(open));
    if let Some(index) = open {
        nav.set_animate_transitions(false);
        nav.push_by_tag(&tag(index));
        nav.set_animate_transitions(true);
    }
    nav.connect_visible_page_notify(move |nav| {
        let now: Option<usize> = nav.visible_page().and_then(|page| {
            page.tag()
                .and_then(|tag| tag.strip_prefix("page-").and_then(|n| n.parse().ok()))
        });
        let before = OPEN.with(|slot| slot.replace(now));
        if before == now {
            return;
        }
        let Some(scroll) = scroll.clone() else {
            return;
        };
        SCROLLS.with(|slot| {
            let mut slot = slot.borrow_mut();
            slot.insert(before, scroll.value());
            let offset = slot.get(&now).copied().unwrap_or(0.0);
            // After the page is laid out, or the offset would be clamped.
            glib::timeout_add_local_once(std::time::Duration::from_millis(60), move || {
                scroll.set_value(offset)
            });
        });
    });
    NAV.with(|slot| slot.replace(nav.downgrade()));
    SEARCH.with(|slot| slot.replace(search.downgrade()));
    View { root: nav, pages }
}

/// Opens Privacy & security (from "Terminal access is off for <phone>"): the
/// next time Settings shows, or now when it is built.
pub(super) fn show_privacy() {
    show_group(7);
}

/// The subpages "Find anything" lists: those a phone has, or the computer-wide
/// ones that exist with none (the settings dialog).
pub(super) fn subpages(paired: bool) -> Vec<super::palette_model::SettingsPage> {
    PAGES
        .iter()
        .filter(|(_, index, _, _)| paired || [0, 7, 8, 9].contains(index))
        .map(
            |(_, index, icon, summary)| super::palette_model::SettingsPage {
                index: *index,
                title: GROUPS[*index],
                summary,
                icon,
            },
        )
        .collect()
}

/// Opens subpage `index` (an index into [`GROUPS`]) the next time Settings
/// shows, or now when it is built.
pub(super) fn show_group(index: usize) {
    QUERY.with(|slot| slot.borrow_mut().clear());
    OPEN.with(|slot| slot.set(Some(index)));
    if let Some(search) = SEARCH.with(|slot| slot.borrow().upgrade()) {
        if !search.text().is_empty() {
            search.set_text("");
            search.emit_by_name::<()>("search-changed", &[]);
        }
    }
    if let Some(nav) = NAV.with(|slot| slot.borrow().upgrade()) {
        nav.set_animate_transitions(false);
        nav.pop_to_tag("list");
        nav.push_by_tag(&tag(index));
        nav.set_animate_transitions(true);
    }
}

/// Debug e2e: `settings-page list|N|search`, to walk the pages in a screenshot.
#[cfg(debug_assertions)]
pub(super) fn debug_open(id: &str, arg: &str) {
    let Some(nav) = NAV.with(|slot| slot.borrow().upgrade()) else {
        return;
    };
    match id {
        "list" => {
            nav.pop_to_tag("list");
        }
        "search" => {
            nav.pop_to_tag("list");
            if let Some(search) = SEARCH.with(|slot| slot.borrow().upgrade()) {
                search.set_text(&arg.replace('_', " "));
            }
        }
        number => {
            if let Ok(index) = number.parse::<usize>() {
                nav.push_by_tag(&tag(index));
            }
        }
    }
}

/// Text for a row, group or description that reads markup: escaped, so a
/// device named `Pixel & <9>` reads as typed.
fn esc(text: impl AsRef<str>) -> glib::GString {
    glib::markup_escape_text(text.as_ref())
}

/// The kit's glyph (and tone) for the GNOME icon name a row asks for.
fn material(icon: &str) -> (&str, Tone) {
    match icon {
        "preferences-system-notifications-symbolic" => {
            ("desklink-notifications-symbolic", Tone::Neutral)
        }
        "audio-input-microphone-symbolic" => ("desklink-mic-symbolic", Tone::Neutral),
        "avatar-default-symbolic" => ("desklink-person-symbolic", Tone::Neutral),
        "media-playback-pause-symbolic" => ("desklink-pause-symbolic", Tone::Neutral),
        "edit-paste-symbolic" => ("desklink-content-paste-symbolic", Tone::Neutral),
        "folder-symbolic" => ("desklink-folder-symbolic", Tone::Neutral),
        "folder-download-symbolic" => ("desklink-download-symbolic", Tone::Neutral),
        "document-open-recent-symbolic" => ("desklink-history-symbolic", Tone::Neutral),
        "preferences-system-symbolic" => ("desklink-settings-symbolic", Tone::Neutral),
        "text-x-generic-symbolic" => ("desklink-description-symbolic", Tone::Neutral),
        "weather-clear-night-symbolic" => ("desklink-coffee-symbolic", Tone::Neutral),
        "changes-allow-symbolic" => ("desklink-lock-open-symbolic", Tone::Neutral),
        "image-x-generic-symbolic" => ("desklink-image-symbolic", Tone::Neutral),
        "notifications-disabled-symbolic" => ("desklink-volume-mute-symbolic", Tone::Neutral),
        "system-lock-screen-symbolic" => ("desklink-lock-symbolic", Tone::Neutral),
        "alarm-symbolic" => ("desklink-notifications-active-symbolic", Tone::Neutral),
        "system-shutdown-symbolic" => ("desklink-restart-alt-symbolic", Tone::Neutral),
        "phone-symbolic" => ("desklink-device-phone-symbolic", Tone::Neutral),
        "view-app-grid-symbolic" => ("desklink-tune-symbolic", Tone::Neutral),
        "preferences-desktop-appearance-symbolic" => ("desklink-palette-symbolic", Tone::Neutral),
        "utilities-terminal-symbolic" => ("desklink-terminal-symbolic", Tone::Neutral),
        "auth-fingerprint-symbolic" => ("desklink-fingerprint-symbolic", Tone::Neutral),
        "input-mouse-symbolic" => ("desklink-mouse-symbolic", Tone::Neutral),
        "dialog-warning-symbolic" => ("desklink-warning-symbolic", Tone::Warn),
        other => (other, Tone::Neutral),
    }
}

/// A row's 40 px icon square (the old `row-icon` badge, in the kit's look).
fn icon_square(icon: &str, _size: i32, _class: &str) -> gtk::CenterBox {
    let (name, tone) = material(icon);
    ui_kit::icon_square(name, tone)
}

fn with_icon(row: &impl IsA<adw::ActionRow>, icon: &str) {
    row.add_prefix(&icon_square(icon, 30, "row-icon"));
}

#[cfg(test)]
mod tests;
